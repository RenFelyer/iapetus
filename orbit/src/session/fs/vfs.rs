//! Ядро файловой системы: состояние и решения без fuser.
//!
//! Операции `async`: кэш не отвечает на всё, и тогда ядро спрашивает runner по каналу данных. Состояние лежит под одним
//! мьютексом, который не держится через `await`.
//!
//! Чтение идёт из кэша метаданных и кэша содержимого. Запись по профилю агента:
//! - [`Profile::Rename`] — агент пишет временный файл (имя по шаблону профиля, `Config::temp_name`) и делает
//!   `rename` поверх цели. Временный файл живёт только в сессии; на `rename` runner получает содержимое и ожидание
//!   «хеш версии, которую видел агент» или «файла нет». Файл, созданный сразу под своим именем, пишется на месте,
//!   как в `Flush`.
//! - [`Profile::Flush`] — агент пишет на месте. Версию проверяют на `open` на запись и на `setattr size`, а на
//!   runner файл уходит на `flush` тем же запросом записи.
//!
//! Если на runner не та версия, запись отвечает `ECANCELED`, и файл на runner не меняется.

mod data;
mod namespace;
mod preload;
mod rename;
mod settle;
#[cfg(test)]
#[path = "../../../tests/session/fs/vfs.rs"]
mod tests;

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Attr, Change, ChangeKind, Content, Error, Expect, FsReply, FsRequest, Kind, Root, WatchBatch};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::cache::{ContentCache, Seen};
use super::inodes::{Ino, Inodes, LocalFile, Owner, parent_of};
use super::meta::{Known, Meta};
use super::remote::Remote;
use crate::session::data::Link;

use namespace::DirItem;
pub(crate) use namespace::SetAttr;
pub(crate) use preload::preload;
use settle::Intent;

/// Как агент пишет файлы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// Временный файл и `rename` поверх цели.
    Rename,
    /// На месте: `O_TRUNC` и запись (агент без временных файлов).
    Flush,
}

/// Символическая ссылка, которую ФС отдаёт сама.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symlink {
    /// Имя в корне ФС.
    pub name: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub profile: Profile,
    /// Владелец файлов с runner: на runner их владелец другой, а агенту нужен пользователь, от имени которого он работает.
    pub owner: Owner,
    pub fingerprint: Fingerprint,
    /// Каталог runner, который показывает эта ФС: проект или `$HOME`. Пути запросов и пачки watch — его.
    pub root_kind: Root,
    /// Записи корня, которые ФС отдаёт сама, не спрашивая runner: символические ссылки поверх одноимённых записей
    /// runner. Создать, удалить или переименовать их нельзя.
    pub links: Vec<Symlink>,
    /// Абсолютный путь точки монтирования, он же рабочий каталог runner; до назначения runner — любой путь, по
    /// которому ФС не смонтирована, затем его меняет `set_root`.
    pub root: PathBuf,
    pub cache_dir: PathBuf,
    /// Наибольший файл: столько вмещает tmpfs `/work`. Больший файл не читается (`EIO`) и не записывается (`EFBIG`).
    pub max_file: usize,
    /// Сколько ждать ответа runner на один запрос.
    pub request_timeout: Duration,
    /// Пути относительно точки монтирования, которых агент не видит: `lookup` отвечает `ENOENT`, в списке каталога
    /// их нет, создать их или переименовать во что-то из них нельзя (`ENOENT`).
    pub hidden: Vec<String>,
    /// Имя временного файла агента до `rename`. Без шаблона `Rename` пишет всё на runner.
    pub temp_name: Option<fn(&str) -> bool>,
}

/// Счётчик эпох: «новое обращение» к файлу — первый `open` после смены эпохи. Эпоху меняют события «команда
/// выполнена» и «конец хода»: после них файлы на runner могли измениться.
#[derive(Debug, Clone)]
pub(crate) struct Epoch(Arc<AtomicU64>);

impl Epoch {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }

    /// Начинает новую эпоху.
    pub(crate) fn advance(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Default for Epoch {
    fn default() -> Self {
        Self::new()
    }
}

/// Запись для ответа ядру.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Entry {
    pub ino: Ino,
    pub attr: Attr,
    pub owner: Owner,
}

pub(crate) type Fh = u64;

pub(crate) type FsResult<T> = Result<T, Errno>;

/// Состояние открытого файла.
#[derive(Debug)]
enum Open {
    /// Чтение из кэш-файла.
    Cached(Arc<File>),
    /// Временный файл сессии: содержимое в таблице inode.
    Local,
    /// Запись на месте: правки копятся в буфере до `flush`.
    Pending(Pending),
}

#[derive(Debug)]
struct Pending {
    path: String,
    /// Версия на runner, которую агент перезапишет.
    base: Expect,
    buf: Vec<u8>,
    dirty: bool,
}

#[derive(Debug)]
struct Handle {
    ino: Ino,
    kind: tokio::sync::Mutex<Open>,
    /// Размер буфера [`Pending`]: пока правки не ушли на runner, `getattr` должен показывать их.
    pending_size: Option<AtomicU64>,
}

#[derive(Debug)]
struct State {
    inodes: Inodes,
    meta: Meta,
    cache: ContentCache,
    handles: HashMap<Fh, Arc<Handle>>,
    dirs: HashMap<Fh, Arc<[DirItem]>>,
    next_fh: Fh,
    last_sweep: Instant,
    /// ФС смонтирована по `root`: до первого `set_root` и после `deactivate` она никого не обслуживает.
    active: bool,
}

/// Как часто запускать очистку кэша: она обходит записи, а не каталог, но каждый вызов её не оправдывает.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Сколько раз сверять состояние и повторять изменение, ответ на которое потерян: каждый новый обрыв или чужая правка
/// между сверкой и повтором тратит попытку.
const SETTLE_ATTEMPTS: usize = 3;

#[derive(Debug)]
pub(crate) struct Vfs {
    remote: Remote,
    config: Config,
    epoch: Epoch,
    state: Mutex<State>,
}

impl Vfs {
    /// Файловые запросы идут runner по каналу данных `link`.
    pub(crate) fn new(link: Link, config: Config, epoch: Epoch) -> io::Result<Self> {
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            // `/work` не для агента: он видит только пути FUSE. Каталог мог уже быть смонтирован с чужими правами.
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&config.cache_dir)?;
            std::fs::set_permissions(&config.cache_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let cache = ContentCache::new(config.cache_dir.clone(), config.fingerprint.to_string(), config.root.clone());
        Ok(Self {
            remote: Remote::new(link, config.request_timeout, config.root_kind),
            epoch,
            state: Mutex::new(State {
                inodes: Inodes::new(),
                meta: Meta::default(),
                cache,
                handles: HashMap::new(),
                dirs: HashMap::new(),
                next_fh: 1,
                last_sweep: Instant::now(),
                active: false,
            }),
            config,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Паника в другом колбэке не должна останавливать остальные: под замком только таблицы.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Запрос к runner. Ошибка связи для агента — такая же ошибка ввода-вывода, как отказ runner.
    async fn call(&self, request: FsRequest) -> Result<FsReply, Error> {
        self.call_expecting(request, 0).await
    }

    /// То же, когда в ответе ждут около `reply_bytes` байт содержимого.
    async fn call_expecting(&self, request: FsRequest, reply_bytes: u64) -> Result<FsReply, Error> {
        match self.remote.call_expecting(request, reply_bytes).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, "запрос к runner не удался");
                Err(Error::Io(error.to_string()))
            }
        }
    }

    fn is_hidden(&self, path: &str) -> bool {
        self.config.hidden.iter().any(|hidden| hidden == path)
    }

    fn path_of(&self, ino: Ino) -> FsResult<String> {
        self.lock().inodes.path(ino).map(str::to_owned).ok_or(Errno::STALE)
    }

    fn entry(&self, state: &State, ino: Ino, attr: Attr) -> Entry {
        let owner = state.inodes.node(ino).and_then(|node| node.owner).unwrap_or(self.config.owner);
        Entry { ino, attr, owner }
    }

    /// Цель ссылки, которую ФС отдаёт сама.
    fn own_link(&self, path: &str) -> Option<&str> {
        self.config
            .links
            .iter()
            .find(|link| link.name == path)
            .map(|link| link.target.as_str())
    }

    /// Изменить запись, которую ФС отдаёт сама, нельзя: runner о ней не знает.
    fn reject_own_link(&self, path: &str) -> FsResult<()> {
        if self.own_link(path).is_some() {
            tracing::warn!(path, "запись отдаёт сама ФС, изменить её нельзя");
            return Err(Errno::IO);
        }
        Ok(())
    }

    /// Атрибуты пути: из таблицы временных файлов, из кэша метаданных или от runner. `None` — пути нет.
    ///
    /// В `$HOME` первое обращение к каталогу читает его целиком: отрицательные `lookup` (агент ищет файлы настроек
    /// в десятке мест) дальше отвечает кэш эпохи, а не runner.
    async fn resolve(&self, path: &str) -> FsResult<Option<Attr>> {
        if let Some(target) = self.own_link(path) {
            return Ok(Some(link_attr(target)));
        }
        let epoch = self.epoch.current();
        {
            let state = self.lock();
            if let Some(local) = state.inodes.at(path).and_then(|node| node.local.as_ref()) {
                return Ok(Some(local_attr(local)));
            }
            if let Some(known) = state.meta.get(path, epoch) {
                return Ok(known.attr());
            }
        }
        if self.config.root_kind == Root::Home && !path.is_empty() && self.read_parent(path, epoch).await {
            let known = self.lock().meta.get(path, epoch);
            if let Some(known) = known {
                return Ok(known.attr());
            }
        }
        match self.call(FsRequest::Stat { path: path.to_owned() }).await {
            Ok(FsReply::Stat(attr)) => {
                self.lock().meta.put(path, Some(attr), epoch);
                Ok(Some(attr))
            }
            Err(Error::NotFound) => {
                self.lock().meta.put(path, None, epoch);
                Ok(None)
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(errno(&error)),
        }
    }

    /// Читает каталог пути целиком, если его список в эпохе неизвестен; `true` — каталог под наблюдением и его список
    /// в кэше. Любая неудача — `false`: путь спросят у runner отдельно.
    async fn read_parent(&self, path: &str, epoch: u64) -> bool {
        let dir = parent_of(path);
        if self.lock().meta.listing_epoch(dir) == Some(epoch) {
            return true;
        }
        match self.call(FsRequest::ReadDir { path: dir.to_owned() }).await {
            Ok(FsReply::ReadDir { entries, watched: true }) => {
                self.lock().meta.put_listing(dir, &entries, epoch, true);
                true
            }
            _ => false,
        }
    }

    /// Агент узнал, есть ли путь: запоминается как версия, на которую он будет рассчитывать при записи.
    fn observe(&self, path: &str, attr: Option<&Attr>) {
        let epoch = self.epoch.current();
        let mut state = self.lock();
        if state.inodes.at(path).is_some_and(|node| node.local.is_some()) {
            return;
        }
        match attr {
            None => state.cache.mark_absent(path, epoch),
            Some(_) if state.cache.get(path).is_some_and(|entry| entry.seen == Seen::Absent) => state.cache.forget(path),
            Some(_) => {}
        }
    }

    /// Хеш и атрибуты файла на runner; `None` — файла нет.
    async fn runner_hash(&self, path: &str) -> FsResult<Option<(Attr, [u8; 32])>> {
        let epoch = self.epoch.current();
        match self.call(FsRequest::Hash { path: path.to_owned() }).await {
            Ok(FsReply::Hashed { attr, sha256 }) => {
                self.lock().meta.put(path, Some(attr), epoch);
                Ok(Some((attr, sha256)))
            }
            Err(Error::NotFound) => {
                let mut state = self.lock();
                state.meta.put(path, None, epoch);
                state.cache.mark_absent(path, epoch);
                Ok(None)
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(write_errno(&error)),
        }
    }

    /// Содержимое файла с runner целиком, сверенное с хешем из ответа. Файл больше [`Config::max_file`] не читается:
    /// в сессии ему негде лежать.
    async fn fetch(&self, path: &str) -> FsResult<(Attr, [u8; 32], Vec<u8>)> {
        let epoch = self.epoch.current();
        let known_size = match self.lock().meta.get(path, epoch) {
            Some(Known::Present(attr)) => Some(attr.size),
            _ => None,
        };
        if known_size.is_some_and(|size| size > self.config.max_file as u64) {
            tracing::warn!(path, max_file = self.config.max_file, "файл больше /work сессии не читается");
            return Err(Errno::IO);
        }
        let reply = self
            .call_expecting(FsRequest::Read { path: path.to_owned() }, known_size.unwrap_or(0))
            .await;
        match reply {
            Ok(FsReply::Read {
                content: Content(bytes), ..
            }) if bytes.len() > self.config.max_file => {
                tracing::warn!(path, max_file = self.config.max_file, "файл больше /work сессии не читается");
                Err(Errno::IO)
            }
            Ok(FsReply::Read {
                attr,
                sha256,
                content: Content(bytes),
            }) => {
                let actual: [u8; 32] = Sha256::digest(&bytes).into();
                if actual != sha256 {
                    tracing::error!(path, "содержимое от runner не совпало с его хешем");
                    return Err(Errno::IO);
                }
                self.lock().meta.put(path, Some(attr), epoch);
                Ok((attr, sha256, bytes))
            }
            Ok(other) => Err(unexpected(&other)),
            Err(Error::NotFound) => {
                let mut state = self.lock();
                state.meta.put(path, None, epoch);
                state.cache.mark_absent(path, epoch);
                Err(Errno::NOENT)
            }
            Err(error) => Err(errno(&error)),
        }
    }

    /// Отправляет содержимое на runner с ожиданием `base`; при успехе кэш и метаданные получают новую версию.
    async fn push(&self, path: &str, base: Expect, content: &[u8]) -> FsResult<(Attr, [u8; 32])> {
        let epoch = self.epoch.current();
        // Запрос собирается заново при каждой отправке: копия содержимого живёт только пока оно уходит.
        let request = || FsRequest::Write {
            path: path.to_owned(),
            expect: base,
            content: Content(content.to_vec()),
        };
        let written: [u8; 32] = Sha256::digest(content).into();
        let response = self.call_settled(request, Intent::Write { path, base, written }).await;
        match response {
            Ok(FsReply::Written { attr, sha256 }) => {
                let mut state = self.lock();
                state.meta.put(path, Some(attr), epoch);
                Self::remember_written(&mut state, path, sha256, content, attr, epoch);
                Ok((attr, sha256))
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => {
                if error == Error::Conflict {
                    self.stale(path);
                }
                Err(write_errno(&error))
            }
        }
    }

    /// Файл на runner не такой, каким его знает сессия: следующее обращение сверит версию и, если надо, перечитает
    /// файл, даже в той же эпохе. Иначе агент после `ECANCELED` перечитывал бы ту же устаревшую копию.
    fn stale(&self, path: &str) {
        let mut state = self.lock();
        state.cache.invalidate_check(path);
        state.meta.invalidate(path);
    }

    /// Агент записал эту версию: он будет рассчитывать именно на неё.
    fn remember_written(state: &mut State, path: &str, sha256: [u8; 32], content: &[u8], attr: Attr, epoch: u64) {
        if let Err(error) = state.cache.store(path, Seen::Hash(sha256), Some(content), attr, epoch) {
            // Копия нужна только чтению: без неё следующий `open` скачает файл заново.
            tracing::warn!(path, %error, "кэш-файл не записан");
            state.cache.forget(path);
        }
    }

    /// Канал данных открылся или оборвался: события watch за это время могли не дойти, и всё, что сессия знала о
    /// runner, нужно спросить заново. Новая эпоха делает недействительными кэш метаданных и сверку копий файлов.
    pub(crate) fn link_changed(&self) {
        self.epoch.advance();
    }

    /// Файловая система монтируется по другому пути: всё, что сессия знала о прежнем, сбрасывается, таблица имён
    /// начинается заново. Тот же путь ничего не меняет.
    pub(crate) fn set_root(&self, root: &std::path::Path) {
        {
            let mut state = self.lock();
            state.active = true;
            if state.cache.root() == root {
                return;
            }
            state.cache.set_root(root.to_owned());
            state.inodes = Inodes::new();
            state.meta = Meta::default();
            state.handles.clear();
            state.dirs.clear();
        }
        self.epoch.advance();
    }

    /// ФС больше не смонтирована: предзагрузка её не выбирает.
    pub(crate) fn deactivate(&self) {
        self.lock().active = false;
    }

    /// Точка монтирования, пока ФС смонтирована.
    pub(crate) fn mounted_root(&self) -> Option<PathBuf> {
        let state = self.lock();
        state.active.then(|| state.cache.root().to_owned())
    }

    /// Файлы берутся с другого runner: копии содержимого и метаданные прежнего сбрасываются, начинается новая
    /// эпоха. Точка монтирования и таблица имён остаются. Запись, рассчитанная на версию файла прежнего runner, на
    /// новом получит `ECANCELED`, если там другой файл.
    pub(crate) fn switch_runner(&self, runner: &Fingerprint) {
        {
            let mut state = self.lock();
            state.cache.switch_runner(runner.to_string());
            state.meta = Meta::default();
        }
        self.epoch.advance();
    }

    /// Применяет изменения, о которых сообщил watch runner.
    pub(crate) fn apply_watch(&self, batch: &WatchBatch) {
        if batch.root != self.config.root_kind {
            return;
        }
        let epoch = self.epoch.current();
        let mut state = self.lock();
        for Change { path, kind } in &batch.changes {
            let path = path.as_str();
            match kind {
                // Копия файла в кэше помнит прежние атрибуты: если они другие, следующий `open` сверит хеш.
                ChangeKind::Updated(attr) => state.meta.put(path, Some(*attr), epoch),
                ChangeKind::Removed => {
                    state.meta.remove_tree(path, epoch);
                    state.cache.invalidate_check(path);
                }
                ChangeKind::Unwatched => {
                    state.meta.unwatch(path);
                    state.cache.invalidate_children(path);
                }
            }
        }
    }

    /// Пора ли чистить кэш. Каждый колбэк спрашивает об этом: очистка запускается не чаще раза в минуту.
    pub(crate) fn sweep_due(&self) -> bool {
        let mut state = self.lock();
        if state.last_sweep.elapsed() < SWEEP_INTERVAL {
            return false;
        }
        state.last_sweep = Instant::now();
        true
    }

    /// Удаляет кэш-файлы, к которым не обращались больше двух часов и которые никто не держит открытыми.
    pub(crate) fn sweep(&self) -> usize {
        self.lock().cache.sweep()
    }
}

/// Атрибуты ссылки, которую ФС отдаёт сама.
fn link_attr(target: &str) -> Attr {
    Attr {
        kind: Kind::Symlink,
        size: target.len() as u64,
        mtime_ns: 0,
        mode: 0o777,
    }
}

fn local_attr(local: &LocalFile) -> Attr {
    Attr {
        kind: Kind::File,
        size: local.content.len() as u64,
        mtime_ns: local.mtime_ns,
        mode: local.mode,
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Ошибка runner для чтения. Отказ в доступе — `EACCES`: для чтения это правда, и агент не ищет обходов.
fn errno(error: &Error) -> Errno {
    match error {
        Error::OutsideWorkdir | Error::Access => Errno::ACCESS,
        other => write_errno(other),
    }
}

/// Ошибка runner для записи, `rename`, `unlink` и `mkdir`. Конфликт — это `ECANCELED`: файл на runner изменился,
/// правку надо повторить. Ни `EPERM`, ни `EBUSY`, ни `EEXIST`, ни `EXDEV`, ни `EACCES` для записи не годятся: агент
/// на них молча перезаписывает цель на месте. Отказ в доступе — `EIO`: запись не удалась, и повтор не поможет.
fn write_errno(error: &Error) -> Errno {
    match error {
        Error::NotFound => Errno::NOENT,
        Error::OutsideWorkdir | Error::Access => {
            tracing::warn!(?error, "runner отказал в записи");
            Errno::IO
        }
        Error::Conflict => Errno::CANCELED,
        Error::NotEmpty => Errno::NOTEMPTY,
        Error::Io(text) => {
            tracing::debug!(text, "ошибка ввода-вывода на runner");
            Errno::IO
        }
    }
}

/// Вид ответа без содержимого: ответ на чтение несёт файл целиком, и в журнал ему не место.
fn reply_name(reply: &FsReply) -> &'static str {
    match reply {
        FsReply::Stat(_) => "Stat",
        FsReply::ReadDir { .. } => "ReadDir",
        FsReply::Read { .. } => "Read",
        FsReply::Link(_) => "Link",
        FsReply::Written { .. } => "Written",
        FsReply::MadeDir(_) => "MadeDir",
        FsReply::Hashed { .. } => "Hashed",
        FsReply::Removed => "Removed",
        FsReply::Renamed(_) => "Renamed",
    }
}

fn unexpected(reply: &FsReply) -> Errno {
    tracing::error!(reply = reply_name(reply), "runner ответил не на тот запрос");
    Errno::IO
}
