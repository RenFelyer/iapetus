//! Тесты ядра ФС против модели runner по каналу данных (мультиплексор над `tokio::io::duplex`).
//!
//! Агент действует через операции ядра так же, как ядро Linux через fuser: проход по пути — `lookup` каждой
//! составляющей, чтение — `open`, `read`, `release`, запись по профилю. Состояние runner меняется ещё и извне: так
//! ведут себя команды и другие люди на машине runner.

use std::collections::{BTreeMap, BTreeSet};

use nerve::protocol::{Attr, Change, ChangeKind, Kind, Root, WatchBatch};
use proptest::prelude::*;
use rustix::fs::OFlags;
use rustix::io::Errno;
use tempfile::TempDir;

use super::*;
use crate::session::fs::inodes::{ROOT, join, name_of, parent_of};
use crate::session::fs::model::{Loss, ModelRunner, TEST_MAX_FILE, Wire, sha, test_config};

#[path = "vfs/home.rs"]
mod home;
#[path = "vfs/scenarios.rs"]
mod scenarios;

const CALLER: Owner = Owner { uid: 1234, gid: 5678 };
const DIRS: [&str; 2] = ["d", "d/e"];
const FILES: [&str; 3] = ["a", "d/b", "d/e/c"];
const LISTED: [&str; 3] = ["", "d", "d/e"];

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Ядро ФС с моделью runner и каталогом кэша.
struct Fixture {
    wire: Wire,
    vfs: Vfs,
    runner: ModelRunner,
    epoch: Epoch,
    cache_dir: TempDir,
    /// Ответы на `lookup`: путь и есть ли он. Так агент узнаёт, что файла нет или он есть.
    observed: Mutex<Vec<(String, bool)>>,
    temp_counter: Mutex<u32>,
}

impl Fixture {
    fn new(profile: Profile) -> Self {
        Self::hiding(profile, &[])
    }

    /// Ядро, которое прячет от агента пути `hidden`.
    fn hiding(profile: Profile, hidden: &[&str]) -> Self {
        let runner = ModelRunner::default();
        let wire = runner.serve();
        Self::over(runner, wire, profile, hidden)
    }

    /// Runner переподключается после каждого обрыва, назначенного [`ModelRunner::break_on`].
    fn reconnecting(profile: Profile) -> Self {
        let runner = ModelRunner::default();
        let wire = runner.serve_reconnecting();
        Self::over(runner, wire, profile, &[])
    }

    /// Агент пишет по шаблону `Rename`, но профиль не называет имён временных файлов.
    fn without_temp_name() -> Self {
        let runner = ModelRunner::default();
        let wire = runner.serve();
        Self::over_with(runner, wire, Profile::Rename, &[], |config| config.temp_name = None)
    }

    fn over(runner: ModelRunner, wire: Wire, profile: Profile, hidden: &[&str]) -> Self {
        Self::over_with(runner, wire, profile, hidden, |_| ())
    }

    fn over_with(runner: ModelRunner, wire: Wire, profile: Profile, hidden: &[&str], adjust: impl FnOnce(&mut Config)) -> Self {
        let cache_dir = tempfile::tempdir().unwrap();
        let epoch = Epoch::new();
        let mut config = test_config(cache_dir.path().join("work"), profile, Duration::from_secs(30));
        config.hidden = hidden.iter().map(|&path| path.to_owned()).collect();
        adjust(&mut config);
        let vfs = Vfs::new(wire.link.clone(), config, epoch.clone()).unwrap();
        Self {
            wire,
            vfs,
            runner,
            epoch,
            cache_dir,
            observed: Mutex::new(Vec::new()),
            temp_counter: Mutex::new(0),
        }
    }

    /// Проход по пути, как делает ядро: `lookup` каждой составляющей.
    async fn walk(&self, path: &str) -> FsResult<Entry> {
        let mut entry = self.vfs.getattr(ROOT, None).await?;
        let mut so_far = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            so_far = join(&so_far, part);
            let found = self.vfs.lookup(entry.ino, part).await;
            self.observed.lock().unwrap().push((so_far.clone(), found.is_ok()));
            entry = found?;
        }
        Ok(entry)
    }

    async fn read_file(&self, path: &str) -> FsResult<Vec<u8>> {
        let entry = self.walk(path).await?;
        let fh = self.vfs.open(entry.ino, OFlags::RDONLY).await?;
        let mut out = Vec::new();
        let read = loop {
            match self.vfs.read(fh, out.len() as u64, 4096).await {
                Ok(chunk) if chunk.is_empty() => break Ok(()),
                Ok(chunk) => out.extend(chunk),
                Err(error) => break Err(error),
            }
        };
        self.vfs.release(fh).await;
        read.map(|()| out)
    }

    fn next_temp(&self, name: &str) -> String {
        let mut counter = self.temp_counter.lock().unwrap();
        *counter += 1;
        format!("{name}.tmp.{}.{:012x}", std::process::id(), *counter)
    }

    /// Профиль `Rename`: временный файл рядом с целью и `rename` поверх неё; при отказе временный файл удаляется.
    async fn save_by_rename(&self, path: &str, content: &[u8]) -> FsResult<()> {
        let parent = self.walk(parent_of(path)).await?;
        let tmp = self.next_temp(name_of(path));
        let (entry, fh) = self
            .vfs
            .create(
                parent.ino,
                &tmp,
                0o644,
                0o022,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                CALLER,
            )
            .await?;
        self.vfs.write(fh, 0, content).await?;
        self.vfs.flush(fh).await?;
        self.vfs.release(fh).await;
        match self.vfs.rename(parent.ino, &tmp, parent.ino, name_of(path), 0).await {
            Ok(()) => {
                let after = self.vfs.lookup(parent.ino, name_of(path)).await?;
                assert_eq!(after.ino, entry.ino, "inode временного файла не перенесён на путь цели при rename");
                Ok(())
            }
            Err(error) => {
                self.vfs.unlink(parent.ino, &tmp).await?;
                Err(error)
            }
        }
    }

    /// Профиль `Flush`: открытие с обрезкой (или создание) и запись на месте.
    async fn save_in_place(&self, path: &str, content: &[u8]) -> FsResult<()> {
        let parent = self.walk(parent_of(path)).await?;
        let found = self.vfs.lookup(parent.ino, name_of(path)).await;
        self.observed.lock().unwrap().push((path.to_owned(), found.is_ok()));
        let fh = match found {
            Ok(entry) => self.vfs.open(entry.ino, OFlags::WRONLY | OFlags::TRUNC).await?,
            Err(Errno::NOENT) => {
                let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC;
                self.vfs.create(parent.ino, name_of(path), 0o644, 0o022, flags, CALLER).await?.1
            }
            Err(error) => return Err(error),
        };
        let written = match self.vfs.write(fh, 0, content).await {
            Ok(_) => self.vfs.flush(fh).await,
            Err(error) => Err(error),
        };
        self.vfs.release(fh).await;
        written
    }

    /// Перенос файла, как его ведёт ядро: каталоги и оба имени разбираются `lookup`, затем `rename`.
    async fn rename(&self, from: &str, to: &str) -> FsResult<()> {
        let from_parent = self.walk(parent_of(from)).await?;
        let to_parent = self.walk(parent_of(to)).await?;
        self.walk(from).await?;
        let target = self.vfs.lookup(to_parent.ino, name_of(to)).await;
        self.observed.lock().unwrap().push((to.to_owned(), target.is_ok()));
        if let Err(error) = target
            && error != Errno::NOENT
        {
            return Err(error);
        }
        self.vfs.rename(from_parent.ino, name_of(from), to_parent.ino, name_of(to), 0).await
    }

    /// Сколько файлов лежит в `/work`.
    fn cached_files(&self) -> usize {
        std::fs::read_dir(self.cache_dir.path().join("work")).unwrap().count()
    }

    fn drain_observed(&self) -> Vec<(String, bool)> {
        std::mem::take(&mut *self.observed.lock().unwrap())
    }
}

#[derive(Debug, Clone, proptest_derive::Arbitrary)]
enum Op {
    /// Агент читает файл.
    Read {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Агент проверяет, есть ли файл.
    Stat {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    List {
        #[proptest(strategy = "0..LISTED.len()")]
        dir: usize,
    },
    /// Агент записывает файл, не читая его.
    Save {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    /// Агент читает файл и записывает правку.
    Edit {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    Mkdir {
        #[proptest(strategy = "0..DIRS.len()")]
        dir: usize,
    },
    Remove {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Агент переносит файл runner (`apply_patch` с `Move to`); `read_source` — читал ли он файл до этого.
    Rename {
        #[proptest(strategy = "0..FILES.len()")]
        from: usize,
        #[proptest(strategy = "0..FILES.len()")]
        to: usize,
        #[proptest(strategy = "any::<bool>()")]
        read_source: bool,
    },
    /// Кто-то на машине runner меняет файл мимо сессии.
    ExternalWrite {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    ExternalRemove {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Команда на runner или конец хода.
    NewTurn,
    /// Runner сообщил о файле событием watch.
    Watch {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Runner перестал наблюдать каталог.
    Unwatch {
        #[proptest(strategy = "0..LISTED.len()")]
        dir: usize,
    },
}

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..5)
}

/// Эталонная модель «путь → байты» и то, что агент знает о файлах.
struct Harness {
    fx: Fixture,
    profile: Profile,
    expected: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    /// Версия файла, на которую агент будет рассчитывать при записи.
    seen: BTreeMap<String, Seen>,
    /// Последнее содержимое, которое агент получил; `None` — файла нет.
    known: BTreeMap<String, Option<Vec<u8>>>,
    /// Файлы, которые изменили извне в этой эпохе и о которых сессия ещё не узнала: кэш вправе отвечать прежним.
    changed: BTreeSet<String>,
}

impl Harness {
    fn new(profile: Profile) -> Self {
        Self {
            fx: Fixture::new(profile),
            profile,
            expected: BTreeMap::new(),
            dirs: BTreeSet::new(),
            seen: BTreeMap::new(),
            known: BTreeMap::new(),
            changed: BTreeSet::new(),
        }
    }

    fn parent_exists(&self, path: &str) -> bool {
        let parent = parent_of(path);
        parent.is_empty() || self.dirs.contains(parent)
    }

    /// Ответы `lookup` меняют то, что агент знает о наличии файла.
    fn apply_observed(&mut self) {
        for (path, present) in self.fx.drain_observed() {
            if !present {
                self.seen.insert(path, Seen::Absent);
            } else if self.seen.get(&path) == Some(&Seen::Absent) {
                self.seen.remove(&path);
            }
        }
    }

    async fn apply(&mut self, op: &Op) {
        match op {
            Op::Read { file } => self.read(FILES[*file]).await,
            Op::Stat { file } => self.stat(FILES[*file]).await,
            Op::List { dir } => self.list(LISTED[*dir]).await,
            Op::Save { file, content } => {
                let path = FILES[*file];
                self.stat(path).await;
                self.save(path, content).await;
            }
            Op::Edit { file, content } => {
                let path = FILES[*file];
                self.read(path).await;
                self.save(path, content).await;
            }
            Op::Mkdir { dir } => self.mkdir(DIRS[*dir]).await,
            Op::Remove { file } => self.remove(FILES[*file]).await,
            Op::Rename { from, to, read_source } => {
                let (from, to) = (FILES[*from], FILES[*to]);
                if *read_source {
                    self.read(from).await;
                } else {
                    self.stat(from).await;
                }
                self.stat(to).await;
                self.rename(from, to).await;
                // Ядро после переноса видит оба имени заново: сессия и модель сходятся в том, что агенту известно.
                self.stat(from).await;
                self.stat(to).await;
            }
            Op::ExternalWrite { file, content } => {
                let path = FILES[*file];
                if self.parent_exists(path) {
                    self.fx.runner.external_write(path, content);
                    self.expected.insert(path.to_owned(), content.clone());
                    self.changed.insert(path.to_owned());
                }
            }
            Op::ExternalRemove { file } => {
                let path = FILES[*file];
                if self.expected.remove(path).is_some() {
                    self.fx.runner.external_remove(path);
                    self.changed.insert(path.to_owned());
                }
            }
            Op::NewTurn => {
                self.fx.epoch.advance();
                self.changed.clear();
            }
            Op::Watch { file } => {
                let path = FILES[*file];
                let kind = match self.fx.runner.attr_of(path) {
                    Some(attr) => ChangeKind::Updated(attr),
                    None => ChangeKind::Removed,
                };
                self.fx.vfs.apply_watch(&WatchBatch {
                    root: Root::Project,
                    changes: vec![Change {
                        path: path.to_owned(),
                        kind,
                    }],
                });
                self.changed.remove(path);
            }
            Op::Unwatch { dir } => {
                let dir = LISTED[*dir];
                self.fx.vfs.apply_watch(&WatchBatch {
                    root: Root::Project,
                    changes: vec![Change {
                        path: dir.to_owned(),
                        kind: ChangeKind::Unwatched,
                    }],
                });
                // Записям ненаблюдаемого каталога сессия не верит и спрашивает runner.
                self.changed.retain(|path| parent_of(path) != dir);
            }
        }
        self.apply_observed();
    }

    async fn read(&mut self, path: &str) {
        let result = self.fx.read_file(path).await;
        self.apply_observed();
        let truth = self.expected.get(path).cloned();
        let may_be_stale = self.changed.contains(path);
        match result {
            Ok(bytes) => {
                let fresh = Some(&bytes) == truth.as_ref();
                let stale = may_be_stale && self.known.get(path) == Some(&Some(bytes.clone()));
                assert!(
                    fresh || stale,
                    "Read {path}: агент получил {bytes:?}, на runner {truth:?}, прежняя версия {:?}",
                    self.known.get(path)
                );
                self.seen.insert(path.to_owned(), Seen::Hash(sha(&bytes)));
                self.known.insert(path.to_owned(), Some(bytes));
                if fresh {
                    self.changed.remove(path);
                }
            }
            Err(error) => {
                assert_eq!(error, Errno::NOENT, "Read {path}: ошибка не ENOENT");
                assert!(
                    truth.is_none() || may_be_stale,
                    "Read {path}: файл на runner есть ({truth:?}) и не менялся извне, а ответ ENOENT"
                );
                self.seen.insert(path.to_owned(), Seen::Absent);
                self.known.insert(path.to_owned(), None);
                if truth.is_none() {
                    self.changed.remove(path);
                }
            }
        }
    }

    async fn stat(&mut self, path: &str) {
        let result = self.fx.walk(path).await;
        self.apply_observed();
        let truth = self.expected.get(path).cloned();
        match result {
            Ok(entry) => {
                assert_eq!(entry.attr.kind, Kind::File, "Stat {path}: не файл");
                assert!(
                    truth.as_ref().is_some_and(|bytes| bytes.len() as u64 == entry.attr.size) || self.changed.contains(path),
                    "Stat {path}: размер {} при содержимом на runner {truth:?}",
                    entry.attr.size
                );
            }
            Err(error) => {
                assert_eq!(error, Errno::NOENT, "Stat {path}: ошибка не ENOENT");
                assert!(
                    truth.is_none() || self.changed.contains(path),
                    "Stat {path}: файл на runner есть ({truth:?}), а ответ ENOENT"
                );
            }
        }
    }

    async fn list(&mut self, dir: &str) {
        let result = self.fx.list_dir(dir).await;
        self.apply_observed();
        if !dir.is_empty() && !self.dirs.contains(dir) {
            assert_eq!(result, Err(Errno::NOENT), "List {dir}: каталога нет");
            return;
        }
        let names = result.unwrap_or_else(|error| panic!("List {dir}: {error:?}"));
        let expected: BTreeSet<String> = self
            .expected
            .keys()
            .chain(self.dirs.iter())
            .filter(|path| parent_of(path) == dir && !path.is_empty())
            .map(|path| name_of(path).to_owned())
            .collect();
        // Файл появился или исчез извне, а сессия ещё не знает: список вправе быть прежним, про остальные имена он точен.
        let certain = |name: &&String| !self.changed.contains(&join(dir, name));
        assert_eq!(
            names.iter().filter(certain).cloned().collect::<BTreeSet<_>>(),
            expected.iter().filter(certain).cloned().collect::<BTreeSet<_>>(),
            "List {dir:?}: имена в каталоге не равны модели"
        );
    }

    async fn save(&mut self, path: &str, content: &[u8]) {
        if !self.parent_exists(path) {
            let result = self.fx.save(self.profile, path, content).await;
            self.apply_observed();
            assert_eq!(result, Err(Errno::NOENT), "Save {path}: родителя нет");
            return;
        }
        let seen = self.seen.get(path).copied();
        let truth = self.expected.get(path).cloned();
        let must_conflict = match seen {
            Some(Seen::Hash(hash)) => truth.as_deref().map(sha) != Some(hash),
            Some(Seen::Absent) => truth.is_some(),
            None => false,
        };
        let stale_lookup = self.changed.contains(path);
        let result = self.fx.save(self.profile, path, content).await;
        self.apply_observed();
        match result {
            Ok(()) => {
                assert!(
                    !must_conflict,
                    "Save {path}: запись прошла поверх версии, которой агент не видел (видел {seen:?}, на runner {truth:?})"
                );
                self.expected.insert(path.to_owned(), content.to_vec());
                self.seen.insert(path.to_owned(), Seen::Hash(sha(content)));
                self.known.insert(path.to_owned(), Some(content.to_vec()));
                self.changed.remove(path);
            }
            Err(Errno::CANCELED) => {
                assert!(
                    must_conflict,
                    "Save {path}: конфликт без причины (видел {seen:?}, на runner {truth:?}, профиль {:?})",
                    self.profile
                );
                // После конфликта файл надо перечитать, даже в той же эпохе.
                self.changed.remove(path);
            }
            // Профиль `Flush`: кэш ещё считал файл существующим, а на runner его уже нет, и `open` отказывает.
            Err(Errno::NOENT) if self.profile == Profile::Flush && stale_lookup => {
                // Агент получил `ENOENT`: теперь он знает, что файла нет.
                self.seen.insert(path.to_owned(), Seen::Absent);
                self.known.insert(path.to_owned(), None);
                self.changed.remove(path);
            }
            Err(error) => panic!("Save {path}: недопустимая ошибка записи {error:?}"),
        }
        if let Err(error) = result {
            assert_write_errno_allowed(error);
        }
    }

    async fn rename(&mut self, from: &str, to: &str) {
        let (seen_from, seen_to) = (self.seen.get(from).copied(), self.seen.get(to).copied());
        let (truth_from, truth_to) = (self.expected.get(from).cloned(), self.expected.get(to).cloned());
        let stale_from = self.changed.contains(from);
        let result = self.fx.rename(from, to).await;
        self.apply_observed();
        // Версия, которой агент не видел, переносу не годится: ни для источника, ни для цели.
        let mismatch = |seen: Option<Seen>, truth: &Option<Vec<u8>>| match seen {
            Some(Seen::Hash(hash)) => truth.as_deref().map(sha) != Some(hash),
            Some(Seen::Absent) => truth.is_some(),
            None => false,
        };
        let must_conflict = mismatch(seen_from, &truth_from) || mismatch(seen_to, &truth_to);
        let parents_exist = self.parent_exists(from) && self.parent_exists(to);
        match result {
            Ok(()) if from == to => assert!(parents_exist, "Rename {from} на себя прошёл без каталога"),
            Ok(()) => {
                assert!(
                    !must_conflict,
                    "Rename {from} -> {to}: перенос прошёл поверх версии, которой агент не видел (источник: видел {seen_from:?}, на \
                     runner {truth_from:?}; цель: видел {seen_to:?}, на runner {truth_to:?})"
                );
                let bytes = self
                    .expected
                    .remove(from)
                    .unwrap_or_else(|| panic!("Rename {from} -> {to}: перенесён файл, которого на runner нет"));
                self.expected.insert(to.to_owned(), bytes.clone());
                self.seen.remove(from);
                self.known.insert(from.to_owned(), None);
                self.seen.insert(to.to_owned(), Seen::Hash(sha(&bytes)));
                self.known.insert(to.to_owned(), Some(bytes));
                self.changed.remove(from);
                self.changed.remove(to);
            }
            Err(Errno::CANCELED) => {
                assert!(
                    must_conflict && truth_from.is_some(),
                    "Rename {from} -> {to}: конфликт без причины (источник: видел {seen_from:?}, на runner {truth_from:?}; цель: видел {seen_to:?}, на runner {truth_to:?})"
                );
                // После конфликта оба файла надо перечитать, даже в той же эпохе.
                self.changed.remove(from);
                self.changed.remove(to);
            }
            Err(Errno::NOENT) => {
                assert!(
                    !parents_exist || truth_from.is_none() || stale_from,
                    "Rename {from} -> {to}: источник и каталоги на runner есть, а ответ ENOENT"
                );
                // Каталога цели нет: ядро отказало на `lookup`, и сессия о файле ничего нового не узнала.
                if parents_exist && truth_from.is_none() {
                    self.changed.remove(from);
                }
            }
            Err(error) => panic!("Rename {from} -> {to}: недопустимая ошибка {error:?}"),
        }
        if let Err(error) = result {
            assert_write_errno_allowed(error);
        }
    }

    async fn mkdir(&mut self, path: &str) {
        let result = self.fx.mkdir(path).await;
        self.apply_observed();
        let expected = if !self.parent_exists(path) {
            Err(Errno::NOENT)
        } else if self.dirs.contains(path) {
            Err(Errno::EXIST)
        } else {
            self.dirs.insert(path.to_owned());
            Ok(())
        };
        assert_eq!(result, expected, "Mkdir {path}");
    }

    async fn remove(&mut self, path: &str) {
        let walked = self.fx.walk(path).await;
        self.apply_observed();
        let truth = self.expected.contains_key(path);
        let entry = match walked {
            Ok(entry) => entry,
            Err(error) => {
                assert_eq!(error, Errno::NOENT, "Remove {path}: ошибка не ENOENT");
                assert!(
                    !truth || self.changed.contains(path),
                    "Remove {path}: файл на runner есть, а ответ ENOENT"
                );
                return;
            }
        };
        let parent = self.fx.walk(parent_of(path)).await.unwrap();
        let result = self.fx.vfs.unlink(parent.ino, name_of(path)).await;
        self.apply_observed();
        let _ = entry;
        match result {
            Ok(()) => {
                assert!(truth, "Remove {path}: удалён файл, которого на runner нет");
                self.expected.remove(path);
            }
            Err(error) => {
                assert_eq!(error, Errno::NOENT, "Remove {path}: ошибка не ENOENT");
                assert!(!truth, "Remove {path}: файл на runner есть, а удаление не прошло");
            }
        }
        self.seen.insert(path.to_owned(), Seen::Absent);
        self.known.insert(path.to_owned(), None);
        self.changed.remove(path);
    }

    /// Runner равен модели; временные файлы на него не уходят и в сессии не остаются.
    #[track_caller]
    fn verify_invariants(&self) {
        self.fx.vfs.verify_invariants();
        assert_eq!(
            self.fx.runner.files(),
            self.expected,
            "файлы на runner не равны модели: файл меняется только записью с совпавшим хешем"
        );
        assert_eq!(self.fx.runner.dirs(), self.dirs, "каталоги на runner не равны модели");
        for request in self.fx.runner.log() {
            if let FsRequest::Write { path, .. } | FsRequest::Read { path } = &request {
                assert!(!path.contains(".tmp."), "временный файл {path} ушёл на runner");
            }
            if let FsRequest::Rename { from, to, .. } = &request {
                assert!(
                    !from.contains(".tmp.") && !to.contains(".tmp."),
                    "перенос временного файла {from} -> {to} ушёл на runner"
                );
            }
        }
        assert_eq!(self.fx.vfs.local_files(), 0, "временные файлы остались в сессии после операции");
    }
}

impl Fixture {
    async fn save(&self, profile: Profile, path: &str, content: &[u8]) -> FsResult<()> {
        match profile {
            Profile::Rename => self.save_by_rename(path, content).await,
            Profile::Flush => self.save_in_place(path, content).await,
        }
    }

    async fn mkdir(&self, path: &str) -> FsResult<()> {
        let parent = self.walk(parent_of(path)).await?;
        self.vfs.mkdir(parent.ino, name_of(path), CALLER).await.map(|_| ())
    }

    async fn list_dir(&self, path: &str) -> FsResult<Vec<String>> {
        let entry = self.walk(path).await?;
        let fh = self.vfs.open_dir(entry.ino).await?;
        let items = self.vfs.read_dir(fh, 0);
        self.vfs.release_dir(fh);
        Ok(items?
            .into_iter()
            .map(|item| item.name)
            .filter(|name| name != "." && name != "..")
            .collect())
    }
}

/// Ни одна ошибка записи не должна быть той, на которую Claude Code молча перезаписывает цель на месте или
/// повторяет операцию бесконечно.
#[track_caller]
fn assert_write_errno_allowed(error: Errno) {
    for forbidden in [Errno::PERM, Errno::BUSY, Errno::EXIST, Errno::XDEV, Errno::ACCESS, Errno::INTR] {
        assert_ne!(
            error, forbidden,
            "запись вернула {forbidden:?}: Claude Code на него перезапишет цель на месте или зациклится"
        );
    }
}

#[track_caller]
fn check_ops(profile: Profile, ops: &[Op]) {
    run(async {
        let mut harness = Harness::new(profile);
        for op in ops {
            harness.apply(op).await;
            harness.verify_invariants();
        }
    });
}

fn profile_of(flush: bool) -> Profile {
    if flush { Profile::Flush } else { Profile::Rename }
}

proptest! {
    #[test]
    fn core_matches_the_model_on_random_operations(
        flush: bool,
        ops in proptest::collection::vec(any::<Op>(), 1..40),
    ) {
        check_ops(profile_of(flush), &ops);
    }
}

impl Vfs {
    /// Сколько файлов живёт только в сессии.
    pub(crate) fn local_files(&self) -> usize {
        self.lock().inodes.local_count()
    }

    #[track_caller]
    pub(crate) fn verify_invariants(&self) {
        let state = self.lock();
        state.inodes.verify_invariants();
        state.cache.verify_invariants();
    }
}
