//! Файловые операции runner в рабочем каталоге проекта и в `$HOME`.
//!
//! Каждый путь открывается ядром через `openat2` с `RESOLVE_BENEATH`: ссылка
//! наружу, абсолютная ссылка и всё, что выходит за каталог, отклоняются самим
//! ядром, без гонки между проверкой и открытием. `RESOLVE_NO_MAGICLINKS`
//! закрывает переходы через `/proc/self/fd`. Поверх этого `..` и абсолютный
//! путь отклоняются по тексту, даже если остаются внутри каталога: сессия
//! присылает только прямые относительные пути.
//!
//! В `$HOME` закрыт `.claude`: по тексту пути и по настоящему пути открытого
//! каталога или файла, поэтому ссылка внутрь `.claude` тоже отклоняется.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use nerve::protocol::{Attr, Content, DirEntry, Error, Expect, FsReply, FsRequest, FsResponse, Kind, MAX_FILE, Root};
use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

mod watch;

#[cfg(test)]
#[path = "../../tests/runner/fs.rs"]
mod tests;

use watch::Watcher;
pub(in crate::runner) use watch::serve as serve_watch;

/// Сколько раз повторять `openat2`, если ядро ответило `EAGAIN`.
const OPEN_ATTEMPTS: u32 = 64;

/// Сколько раз пробовать другое имя временного файла, если оно занято.
const TEMP_ATTEMPTS: u32 = 16;

/// Каталог конфигурации агента на runner: в `$HOME` закрыт, агент берёт свою конфигурацию из контейнера session.
const CLAUDE_DIR: &str = ".claude";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ключ блокировки записи: каталог (устройство и inode) и имя в нём. Ключ по inode, а не по тексту пути:
/// два пути через символические ссылки внутри каталога к одному файлу получают одну блокировку.
type LockKey = (u64, u64, String);

/// Рабочий каталог runner, открытый один раз.
#[derive(Debug)]
pub(super) struct Workdir {
    root: OwnedFd,
    /// Настоящий путь рабочего каталога: от него отсчитываются абсолютные пути сессии.
    path: PathBuf,
    /// Какой каталог runner это: проект или `$HOME`.
    root_kind: Root,
    /// Настоящий путь `$HOME` runner, если он задан и существует.
    home: Option<PathBuf>,
    /// Второй обслуживаемый каталог (проект — для `$HOME`): ссылка в него разрешена.
    sibling: Option<PathBuf>,
    /// Настоящие пути закрытого `.claude` в `$HOME`.
    denied: Vec<PathBuf>,
    watcher: Watcher,
    locks: Mutex<HashMap<LockKey, Arc<Mutex<()>>>>,
}

impl Workdir {
    /// Рабочий каталог проекта.
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let home = std::env::var_os("HOME").and_then(|home| std::fs::canonicalize(home).ok());
        Self::build(path, Root::Project, home, None)
    }

    /// `$HOME` runner; `project` — рабочий каталог проекта, ссылка в него разрешена.
    pub(super) fn open_home(path: &Path, project: &Path) -> io::Result<Self> {
        let home = std::fs::canonicalize(path)?;
        Self::build(&home, Root::Home, Some(home.clone()), Some(project.to_owned()))
    }

    fn build(path: &Path, root_kind: Root, home: Option<PathBuf>, sibling: Option<PathBuf>) -> io::Result<Self> {
        let root = rfs::open(path, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
        let watcher = Watcher::new(&root)?;
        let path = watch::fd_path(&root)?;
        let denied = home.as_deref().map(denied_paths).unwrap_or_default();
        Ok(Self {
            root,
            path,
            root_kind,
            home,
            sibling,
            denied,
            watcher,
            locks: Mutex::default(),
        })
    }

    pub(super) fn root_kind(&self) -> Root {
        self.root_kind
    }

    /// Путь внутри закрытого `.claude` корня `$HOME`: по тексту.
    fn hidden(&self, path: &str) -> bool {
        self.root_kind == Root::Home && path.split('/').find(|part| !matches!(*part, "" | ".")) == Some(CLAUDE_DIR)
    }

    fn is_denied(&self, real: &Path) -> bool {
        self.denied.iter().any(|denied| real.starts_with(denied))
    }

    /// Разбирает путь сессии; `.claude` в `$HOME` — `Access`.
    fn parts<'a>(&self, path: &'a str) -> Result<Vec<&'a str>, Error> {
        let parts = components(path)?;
        if self.hidden(path) {
            return Err(Error::Access);
        }
        Ok(parts)
    }

    /// Выполняет запрос; блокирует поток, поэтому из async-кода вызывается через `spawn_blocking`.
    pub(super) fn handle(&self, request: FsRequest) -> FsResponse {
        match request {
            FsRequest::Stat { path } => self.stat(&path).map(FsReply::Stat),
            FsRequest::ReadDir { path } => self.read_dir(&path).map(|(entries, watched)| FsReply::ReadDir { entries, watched }),
            FsRequest::Read { path } => self.read(&path),
            FsRequest::ReadLink { path } => self.read_link(&path).map(FsReply::Link),
            FsRequest::Write { path, expect, content } => self.write(&path, expect, content),
            FsRequest::MakeDir { path } => self.make_dir(&path).map(FsReply::MadeDir),
            FsRequest::Hash { path } => self.hash(&path),
            FsRequest::Remove { path } => self.remove(&path).map(|()| FsReply::Removed),
            FsRequest::Rename {
                from,
                to,
                expect_from,
                expect_to,
            } => self.rename(&from, &to, expect_from, expect_to).map(FsReply::Renamed),
        }
    }

    /// Конец сессии: каталоги больше не наблюдаются, неотправленные изменения отброшены.
    pub(super) fn end_session(&self) {
        self.watcher.clear();
    }

    /// Канал данных открылся заново: сессия не знает, что произошло в каталогах за время обрыва. Каждый наблюдаемый
    /// каталог получает `Unwatched` и снимается с наблюдения, накопленное отбрасывается; сессия прочтёт каталоги заново.
    pub(super) fn resync_watch(&self) {
        self.watcher.resync();
    }

    /// Каталог, в котором запускается команда: настоящий путь существующего каталога внутри рабочего. Абсолютный
    /// путь должен начинаться с рабочего каталога (сессия видит его по тому же пути, что и runner), остальные
    /// правила — как у файловых путей: `..` и ссылки наружу отклоняются.
    pub(super) fn resolve_cwd(&self, cwd: &str) -> Result<PathBuf, Error> {
        let relative = match Path::new(cwd).strip_prefix(&self.path) {
            Ok(relative) => relative.to_str().ok_or(Error::OutsideWorkdir)?,
            Err(_) => cwd,
        };
        let dir = self.open_dir(&self.parts(relative)?)?;
        watch::fd_path(&dir).map_err(|e| io_error(&e))
    }

    fn open_beneath(&self, relative: &str, flags: OFlags) -> Result<OwnedFd, Error> {
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
        // EAGAIN: во время разбора пути где-то в системе прошёл rename, и ядро не может поручиться, что путь не вышел из
        // каталога. Повтор разбирает путь заново.
        for _ in 0..OPEN_ATTEMPTS {
            match rfs::openat2(&self.root, relative, flags | OFlags::CLOEXEC, Mode::empty(), resolve) {
                Err(Errno::AGAIN) => {}
                Ok(fd) => return self.check_opened(fd),
                Err(e) => return Err(errno(e)),
            }
        }
        Err(errno(Errno::AGAIN))
    }

    /// Открытое через ссылку не должно оказаться в закрытом `.claude`: ядро считает его внутри `$HOME`.
    fn check_opened(&self, fd: OwnedFd) -> Result<OwnedFd, Error> {
        if self.root_kind == Root::Project {
            return Ok(fd);
        }
        let real = watch::fd_path(&fd).map_err(|e| io_error(&e))?;
        if self.is_denied(&real) { Err(Error::Access) } else { Ok(fd) }
    }

    fn open_dir(&self, parts: &[&str]) -> Result<OwnedFd, Error> {
        let relative = if parts.is_empty() { ".".to_owned() } else { parts.join("/") };
        self.open_beneath(&relative, OFlags::RDONLY | OFlags::DIRECTORY)
    }

    fn stat(&self, path: &str) -> Result<Attr, Error> {
        let parts = self.parts(path)?;
        let Some((name, dir)) = parts.split_last() else {
            return rfs::fstat(&self.root).map(|st| attr(&st)).map_err(errno);
        };
        let parent = self.open_dir(dir)?;
        rfs::statat(&parent, *name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| attr(&st))
            .map_err(errno)
    }

    fn read_dir(&self, path: &str) -> Result<(Vec<DirEntry>, bool), Error> {
        let parts = self.parts(path)?;
        let dir_fd = self.open_dir(&parts)?;
        // Подписка раньше чтения записей: изменение между ними не теряется.
        let watched = self.watcher.watch(&parts.join("/"), &dir_fd);
        let mut entries = Vec::new();
        let at_root = parts.is_empty();
        for entry in Dir::read_from(&dir_fd).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            let raw_name = entry.file_name();
            if matches!(raw_name.to_bytes(), b"." | b"..")
                || (at_root && self.root_kind == Root::Home && raw_name.to_bytes() == CLAUDE_DIR.as_bytes())
            {
                continue;
            }
            let Ok(name) = raw_name.to_str() else {
                tracing::warn!(name = ?raw_name, "имя не UTF-8, запись пропущена");
                continue;
            };
            match rfs::statat(&dir_fd, raw_name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => entries.push(DirEntry {
                    name: name.to_owned(),
                    attr: attr(&st),
                }),
                // Запись удалили между чтением каталога и stat.
                Err(Errno::NOENT) => {}
                Err(e) => return Err(errno(e)),
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((entries, watched))
    }

    fn read(&self, path: &str) -> FsResponse {
        let parts = self.parts(path)?;
        let relative = if parts.is_empty() { ".".to_owned() } else { parts.join("/") };
        // O_NONBLOCK: чтение именованного канала не должно вешать поток.
        let fd = self.open_beneath(&relative, OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY)?;
        let st = rfs::fstat(&fd).map_err(errno)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(Error::Io("не обычный файл".to_owned()));
        }
        let limit = u64::try_from(MAX_FILE).unwrap_or(u64::MAX);
        // Размер известен заранее: буфер не растёт удвоениями, а они на файле в гигабайт держали бы вдвое больше.
        let mut bytes = Vec::with_capacity(usize::try_from(u64::try_from(st.st_size).unwrap_or(0).min(limit)).unwrap_or(0));
        File::from(fd).take(limit + 1).read_to_end(&mut bytes).map_err(|e| io_error(&e))?;
        if bytes.len() > MAX_FILE {
            return Err(Error::Io(format!("файл больше {MAX_FILE} байт")));
        }
        let sha256 = Sha256::digest(&bytes).into();
        Ok(FsReply::Read {
            attr: attr(&st),
            sha256,
            content: Content(bytes),
        })
    }

    fn hash(&self, path: &str) -> FsResponse {
        let parts = self.parts(path)?;
        let relative = if parts.is_empty() { ".".to_owned() } else { parts.join("/") };
        let fd = self.open_beneath(&relative, OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY)?;
        let st = rfs::fstat(&fd).map_err(errno)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(Error::Io("не обычный файл".to_owned()));
        }
        let sha256 = hash_file(File::from(fd)).map_err(|e| io_error(&e))?;
        Ok(FsReply::Hashed { attr: attr(&st), sha256 })
    }

    /// Цель символической ссылки как записана в ней. Отдаётся, только если после разрешения от каталога ссылки цель
    /// лежит внутри рабочего каталога или `$HOME` runner: ядро сессии пойдёт по ссылке само и в другом случае
    /// показало бы агенту файл своей файловой системы под видом файла runner.
    fn read_link(&self, path: &str) -> Result<String, Error> {
        let parts = self.parts(path)?;
        let Some((name, dir)) = parts.split_last() else {
            return Err(Error::Io("рабочий каталог — не ссылка".to_owned()));
        };
        let parent = self.open_dir(dir)?;
        let target = rfs::readlinkat(&parent, *name, Vec::new()).map_err(errno)?;
        let target = target.into_string().map_err(|_| Error::Io("цель ссылки не UTF-8".to_owned()))?;
        let parent_path = watch::fd_path(&parent).map_err(|e| io_error(&e))?;
        let resolved = canonical(&parent_path.join(&target))?;
        let inside = [Some(&self.path), self.home.as_ref(), self.sibling.as_ref()]
            .into_iter()
            .flatten()
            .any(|root| resolved.starts_with(root));
        if inside && !self.is_denied(&resolved) {
            Ok(target)
        } else {
            Err(Error::Access)
        }
    }

    /// Удаляет файл, ссылку или пустой каталог под той же блокировкой имени, что и запись: удаление не вклинивается
    /// между проверкой хеша и `rename`.
    fn remove(&self, path: &str) -> Result<(), Error> {
        let parts = self.parts(path)?;
        let Some((name, dir)) = parts.split_last() else {
            return Err(Error::Io("рабочий каталог не удаляется".to_owned()));
        };
        let parent = self.open_dir(dir)?;
        let key = lock_key(&parent, name)?;
        self.with_locks(&[key], || {
            // unlinkat не идёт по ссылке: удаляется сама ссылка. Каталог ядро отвечает EISDIR.
            match rfs::unlinkat(&parent, *name, AtFlags::empty()) {
                Err(Errno::ISDIR) => rfs::unlinkat(&parent, *name, AtFlags::REMOVEDIR),
                other => other,
            }
            .map_err(errno)?;
            rfs::fsync(&parent).map_err(errno)
        })
    }

    fn make_dir(&self, path: &str) -> Result<Attr, Error> {
        let parts = self.parts(path)?;
        let Some((name, dir)) = parts.split_last() else {
            return Err(Error::Conflict);
        };
        let parent = self.open_dir(dir)?;
        rfs::mkdirat(&parent, *name, Mode::from_raw_mode(0o777)).map_err(|e| if e == Errno::EXIST { Error::Conflict } else { errno(e) })?;
        rfs::statat(&parent, *name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| attr(&st))
            .map_err(errno)
    }

    fn write(&self, path: &str, expect: Expect, content: Content) -> FsResponse {
        let Content(bytes) = content;
        let parts = self.parts(path)?;
        let Some((name, dir)) = parts.split_last() else {
            return Err(Error::Io("рабочий каталог — не файл".to_owned()));
        };
        let parent = self.open_dir(dir)?;
        let key = lock_key(&parent, name)?;
        self.with_locks(&[key], || replace_file(&parent, name, expect, &bytes))
    }

    /// Переносит файл под блокировкой обоих имён. Блокировки берутся в порядке ключей, поэтому два переноса
    /// навстречу друг другу и запись в любой из концов не взаимоблокируются.
    fn rename(&self, from: &str, to: &str, expect_from: [u8; 32], expect_to: Expect) -> Result<Attr, Error> {
        let from_parts = self.parts(from)?;
        let to_parts = self.parts(to)?;
        let (Some((from_name, from_dir)), Some((to_name, to_dir))) = (from_parts.split_last(), to_parts.split_last()) else {
            return Err(Error::Io("рабочий каталог не переносится".to_owned()));
        };
        let from_parent = self.open_dir(from_dir)?;
        let to_parent = self.open_dir(to_dir)?;
        let from_key = lock_key(&from_parent, from_name)?;
        let to_key = lock_key(&to_parent, to_name)?;
        if from_key == to_key {
            return Err(Error::Io("источник и цель — один и тот же путь".to_owned()));
        }
        self.with_locks(&[from_key, to_key], || {
            move_file(&from_parent, from_name, &to_parent, to_name, expect_from, expect_to)
        })
    }

    /// Выполняет `f` под блокировками ключей; запись в карте живёт, пока ключом кто-то пользуется.
    fn with_locks<T>(&self, keys: &[LockKey], f: impl FnOnce() -> T) -> T {
        let mut keys = keys.to_vec();
        keys.sort();
        keys.dedup();
        let locks: Vec<Arc<Mutex<()>>> = {
            let mut map = relock(&self.locks);
            keys.iter().map(|key| Arc::clone(map.entry(key.clone()).or_default())).collect()
        };
        let result = {
            let _guards: Vec<_> = locks.iter().map(|lock| relock(lock)).collect();
            f()
        };
        let mut map = relock(&self.locks);
        drop(locks);
        for key in &keys {
            if map.get(key).is_some_and(|entry| Arc::strong_count(entry) == 1) {
                map.remove(key);
            }
        }
        result
    }
}

/// Ключ блокировки имени `name` в каталоге `parent`.
fn lock_key(parent: &OwnedFd, name: &str) -> Result<LockKey, Error> {
    let st = rfs::fstat(parent).map_err(errno)?;
    Ok((st.st_dev, st.st_ino, name.to_owned()))
}

/// Открывает запись каталога для чтения, не идя по ссылке; `None` — записи нет. Ссылка — отказ: перенос или замена
/// заменили бы саму ссылку, а не цель.
fn open_nofollow(parent: &OwnedFd, name: &str) -> Result<Option<OwnedFd>, Error> {
    match rfs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(Error::Access),
        Err(e) => Err(errno(e)),
    }
}

/// Хеш содержимого обычного файла, открытого `fd`.
fn regular_hash(fd: &OwnedFd) -> Result<[u8; 32], Error> {
    let st = rfs::fstat(fd).map_err(errno)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(Error::Io("не обычный файл".to_owned()));
    }
    hash_file(File::from(fd.try_clone().map_err(|e| io_error(&e))?)).map_err(|e| io_error(&e))
}

/// Переносит `from_name` в `to_name`, если источник и цель такие, как ждёт сессия. Вызывается под блокировками обоих имён.
fn move_file(
    from_parent: &OwnedFd,
    from_name: &str,
    to_parent: &OwnedFd,
    to_name: &str,
    expect_from: [u8; 32],
    expect_to: Expect,
) -> Result<Attr, Error> {
    let source = open_nofollow(from_parent, from_name)?.ok_or(Error::NotFound)?;
    if regular_hash(&source)? != expect_from {
        return Err(Error::Conflict);
    }
    let flags = match expect_to {
        // Атомарно: цель, появившаяся после проверки, не затирается.
        Expect::Absent => RenameFlags::NOREPLACE,
        Expect::Hash(wanted) => {
            let target = open_nofollow(to_parent, to_name)?.ok_or(Error::Conflict)?;
            if regular_hash(&target)? != wanted {
                return Err(Error::Conflict);
            }
            RenameFlags::empty()
        }
    };
    rfs::renameat_with(from_parent, from_name, to_parent, to_name, flags).map_err(|e| match e {
        Errno::EXIST => Error::Conflict,
        Errno::XDEV => Error::Io("перенос между файловыми системами".to_owned()),
        other => errno(other),
    })?;
    rfs::fsync(to_parent).map_err(errno)?;
    rfs::fsync(from_parent).map_err(errno)?;
    rfs::fstat(&source).map(|st| attr(&st)).map_err(errno)
}

/// Заменяет файл `name` в каталоге `parent`, если он ещё такой, как ждёт `expect`. Вызывается под блокировкой имени.
fn replace_file(parent: &OwnedFd, name: &str, expect: Expect, bytes: &[u8]) -> FsResponse {
    // Запись по ссылке заменила бы саму ссылку, а не цель, поэтому ссылка — отказ.
    let existing = open_nofollow(parent, name)?;
    let mut mode = None;
    match (&expect, existing) {
        (Expect::Absent, None) => {}
        (Expect::Hash(_), None) | (Expect::Absent, Some(_)) => return Err(Error::Conflict),
        (Expect::Hash(wanted), Some(fd)) => {
            let st = rfs::fstat(&fd).map_err(errno)?;
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                return Err(Error::Io("не обычный файл".to_owned()));
            }
            if hash_file(File::from(fd)).map_err(|e| io_error(&e))? != *wanted {
                return Err(Error::Conflict);
            }
            mode = Some(Mode::from_raw_mode(st.st_mode & 0o7777));
        }
    }

    let (temp_name, temp) = create_temp(parent)?;
    let written = write_temp(&temp, bytes, mode).and_then(|()| {
        rfs::renameat(parent, temp_name.as_str(), parent, name).map_err(errno)?;
        rfs::fsync(parent).map_err(errno)
    });
    if let Err(e) = written {
        if let Err(cleanup) = rfs::unlinkat(parent, temp_name.as_str(), AtFlags::empty())
            && cleanup != Errno::NOENT
        {
            tracing::warn!(temp = temp_name, error = %cleanup, "временный файл не удалён");
        }
        return Err(e);
    }
    let st = rfs::fstat(&temp).map_err(errno)?;
    Ok(FsReply::Written {
        attr: attr(&st),
        sha256: Sha256::digest(bytes).into(),
    })
}

/// Создаёт временный файл рядом с целью. Права — по umask; имя не зависит от имени цели (оно может быть длиной в `NAME_MAX`).
fn create_temp(parent: &OwnedFd) -> Result<(String, OwnedFd), Error> {
    for _ in 0..TEMP_ATTEMPTS {
        let temp_name = format!(
            ".iapetus-tmp.{}.{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rfs::openat(parent, temp_name.as_str(), flags, Mode::from_raw_mode(0o666)) {
            Ok(fd) => return Ok((temp_name, fd)),
            Err(Errno::EXIST) => {}
            Err(e) => return Err(errno(e)),
        }
    }
    Err(Error::Io("не удалось выбрать имя временного файла".to_owned()))
}

/// Пишет содержимое во временный файл и сбрасывает его на диск. У существующего файла права сохраняются.
fn write_temp(temp: &OwnedFd, bytes: &[u8], mode: Option<Mode>) -> Result<(), Error> {
    let mut file = File::from(temp.try_clone().map_err(|e| io_error(&e))?);
    file.write_all(bytes).map_err(|e| io_error(&e))?;
    if let Some(mode) = mode {
        rfs::fchmod(temp, mode).map_err(errno)?;
    }
    rfs::fsync(temp).map_err(errno)
}

fn hash_file(mut file: File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..n]);
    }
}

/// Закрытый `.claude` в `home`: путь как есть и настоящий, если `.claude` — ссылка.
fn denied_paths(home: &Path) -> Vec<PathBuf> {
    let dir = home.join(CLAUDE_DIR);
    let mut denied = vec![dir.clone()];
    if let Ok(real) = std::fs::canonicalize(&dir)
        && real != dir
    {
        denied.push(real);
    }
    denied
}

/// Настоящий путь без требования, чтобы он существовал: ссылки разрешаются в существующей части, остаток — как есть.
/// Остаток с `..` разрешить нельзя (`Access`).
fn canonical(path: &Path) -> Result<PathBuf, Error> {
    let mut rest = Vec::new();
    let mut existing = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut real) => {
                real.extend(rest.iter().rev());
                return Ok(real);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
                    return Err(Error::Access);
                };
                rest.push(name);
                existing.pop();
            }
            Err(e) => return Err(io_error(&e)),
        }
    }
}

/// Разбирает путь сессии на компоненты. Абсолютный путь и `..` — выход из каталога.
fn components(path: &str) -> Result<Vec<&str>, Error> {
    if path.starts_with('/') {
        return Err(Error::OutsideWorkdir);
    }
    if path.contains('\0') {
        return Err(Error::Io("NUL в пути".to_owned()));
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(Error::OutsideWorkdir),
            _ => parts.push(part),
        }
    }
    Ok(parts)
}

fn attr(st: &Stat) -> Attr {
    let kind = match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    };
    let mtime_ns = st
        .st_mtime
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::try_from(st.st_mtime_nsec).unwrap_or(0));
    Attr {
        kind,
        size: u64::try_from(st.st_size).unwrap_or(0),
        mtime_ns,
        mode: st.st_mode & 0o7777,
    }
}

fn errno(e: Errno) -> Error {
    match e {
        Errno::NOENT => Error::NotFound,
        // openat2 с RESOLVE_BENEATH так сообщает о выходе из каталога.
        Errno::XDEV => Error::OutsideWorkdir,
        Errno::ACCESS | Errno::PERM => Error::Access,
        Errno::NOTEMPTY => Error::NotEmpty,
        _ => Error::Io(io::Error::from(e).to_string()),
    }
}

fn io_error(e: &io::Error) -> Error {
    match e.raw_os_error() {
        Some(code) => errno(Errno::from_raw_os_error(code)),
        None => Error::Io(e.to_string()),
    }
}

/// Блокировка, переживающая панику в чужом потоке: данные под ней — только `()` и карта ключей.
fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
