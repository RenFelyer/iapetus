//! Главное утверждение: после затишья кэш, применивший все пришедшие события к снимку `ReadDir`, равен диску. Пачки
//! собираются из очереди inotify напрямую: событие ядро ставит в очередь в момент системного вызова, поэтому «затишье»
//! наступает без пауз.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Command;

use insta::assert_snapshot;
use nerve::protocol::{Attr, Change, ChangeKind, Expect, FsReply, FsRequest, Kind};
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::{Workdir, collect, is_within, join, parent};

/// Рабочий каталог во временном месте и каталог `outside` рядом: через него файлы уходят из дерева и приходят в него.
struct Fixture {
    _base: TempDir,
    work: PathBuf,
    outside: PathBuf,
    workdir: Workdir,
}

impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(base.path()).unwrap();
        let work = root.join("work");
        let outside = root.join("outside");
        fs::create_dir(&work).unwrap();
        fs::create_dir(&outside).unwrap();
        let workdir = Workdir::open(&work).unwrap();
        Self {
            _base: base,
            work,
            outside,
            workdir,
        }
    }

    /// Всё, что runner накопил из очереди inotify, одной пачкой.
    fn flush(&self) -> Vec<Change> {
        self.workdir.watcher.drain().unwrap();
        let (notices, touched) = self.workdir.watcher.take_pending();
        collect(&self.workdir, notices, &touched)
    }

    fn read_dir(&self, path: &str) -> Option<Vec<(String, Attr)>> {
        match self.workdir.handle(FsRequest::ReadDir { path: path.into() }) {
            Ok(FsReply::ReadDir { entries, .. }) => Some(entries.into_iter().map(|e| (e.name, e.attr)).collect()),
            _ => None,
        }
    }

    fn stat(&self, path: &str) -> Attr {
        match self.workdir.handle(FsRequest::Stat { path: path.into() }) {
            Ok(FsReply::Stat(attr)) => attr,
            other => panic!("stat {path:?}: {other:?}"),
        }
    }
}

/// Кэш метаданных сессии: то, что FUSE строит из ответов `ReadDir` и событий watch.
#[derive(Default)]
struct Cache {
    attrs: BTreeMap<String, Attr>,
    /// Каталоги, чьим записям кэш верит.
    watched: BTreeSet<String>,
}

impl Cache {
    /// Сессия читает каталог; нет каталога — кэш не меняется.
    fn read_dir(&mut self, fx: &Fixture, dir: &str) {
        let Some(entries) = fx.read_dir(dir) else { return };
        let listed: BTreeSet<String> = entries.iter().map(|(name, _)| join(dir, name)).collect();
        let gone: Vec<String> = self.children(dir).into_keys().filter(|path| !listed.contains(path)).collect();
        for path in gone {
            self.remove_subtree(&path);
        }
        for (name, attr) in entries {
            self.attrs.insert(join(dir, &name), attr);
        }
        self.attrs.insert(dir.to_owned(), fx.stat(dir));
        self.watched.insert(dir.to_owned());
    }

    fn apply(&mut self, changes: &[Change]) {
        for change in changes {
            match change.kind {
                ChangeKind::Updated(attr) => {
                    self.attrs.insert(change.path.clone(), attr);
                }
                ChangeKind::Removed => self.remove_subtree(&change.path),
                ChangeKind::Unwatched => {
                    self.watched.remove(&change.path);
                }
            }
        }
    }

    fn remove_subtree(&mut self, path: &str) {
        self.attrs.retain(|p, _| !is_within(p, path));
        self.watched.retain(|p| !is_within(p, path));
    }

    fn children(&self, dir: &str) -> BTreeMap<String, Attr> {
        self.attrs
            .iter()
            .filter(|(path, _)| !path.is_empty() && parent(path) == dir)
            .map(|(path, attr)| (path.clone(), *attr))
            .collect()
    }

    /// Размер и mtime каталога, который сам не наблюдается, не обновляются: их меняют записи внутри него,
    /// а событий о них никто не получает. Для сравнения они обнуляются с обеих сторон.
    fn comparable(&self, path: &str, mut attr: Attr) -> Attr {
        if attr.kind == Kind::Dir && !self.watched.contains(path) {
            attr.size = 0;
            attr.mtime_ns = 0;
        }
        attr
    }

    #[track_caller]
    fn verify_invariants(&self, fx: &Fixture, history: &str) {
        for dir in &self.watched {
            let Ok(names) = fs::read_dir(fx.work.join(dir)) else {
                panic!("каталог {dir:?} удалён, но кэш считает его наблюдаемым; история: {history}");
            };
            let disk: BTreeMap<String, Attr> = names
                .map(|entry| {
                    let path = join(dir, entry.unwrap().file_name().to_str().unwrap());
                    let attr = self.comparable(&path, fx.stat(&path));
                    (path, attr)
                })
                .collect();
            let cached: BTreeMap<String, Attr> = self
                .children(dir)
                .into_iter()
                .map(|(path, attr)| {
                    let attr = self.comparable(&path, attr);
                    (path, attr)
                })
                .collect();
            assert_eq!(
                cached, disk,
                "записи наблюдаемого каталога {dir:?} в кэше расходятся с диском; история: {history}"
            );
            assert_eq!(
                self.attrs.get(dir.as_str()).copied(),
                Some(fx.stat(dir)),
                "атрибуты наблюдаемого каталога {dir:?} в кэше расходятся с диском; история: {history}"
            );
        }
    }
}

const PARENTS: [&str; 4] = ["", "d0", "d1", "d0/s"];
/// Имена `d0` и `s` повторяют каталоги: файл и каталог сталкиваются за одно имя.
const NAMES: [&str; 5] = ["a", "b", "c", "d0", "s"];
/// У всех есть бит исполнения для владельца: после `chmod` каталог остаётся проходимым.
const MODES: [u32; 4] = [0o700, 0o750, 0o755, 0o770];

#[derive(Debug, Clone, Copy, proptest_derive::Arbitrary)]
struct Pick {
    #[proptest(strategy = "0..PARENTS.len()")]
    parent: usize,
    #[proptest(strategy = "0..NAMES.len()")]
    name: usize,
}

impl Pick {
    fn path(self) -> String {
        join(PARENTS[self.parent], NAMES[self.name])
    }
}

/// Изменения на диске мимо протокола и действия сессии. Ошибки операций (нет файла, имя занято) — часть случайности.
#[derive(Debug, Clone, proptest_derive::Arbitrary)]
enum Op {
    Create {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
    },
    Write {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
        #[proptest(strategy = "0..4_usize")]
        len: usize,
    },
    MakeDir {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
    },
    Rename {
        #[proptest(strategy = "any::<Pick>()")]
        from: Pick,
        #[proptest(strategy = "any::<Pick>()")]
        to: Pick,
    },
    /// Перенос файла операцией `Rename` runner: хеши берутся с диска, так что перенос проходит, если есть что переносить.
    Move {
        #[proptest(strategy = "any::<Pick>()")]
        from: Pick,
        #[proptest(strategy = "any::<Pick>()")]
        to: Pick,
        #[proptest(strategy = "any::<bool>()")]
        replace: bool,
    },
    Remove {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
    },
    Chmod {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
        #[proptest(strategy = "0..MODES.len()")]
        mode: usize,
    },
    /// Файл уходит из дерева в `outside`.
    MoveOut {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
    },
    /// Файл приходит из `outside` в дерево.
    MoveIn {
        #[proptest(strategy = "any::<Pick>()")]
        at: Pick,
    },
    /// Сессия читает каталог заново, как после `Unwatched`.
    ReadDir {
        #[proptest(strategy = "0..PARENTS.len()")]
        dir: usize,
    },
    /// Затишье: runner отправил накопленное, кэш его применил.
    Flush,
}

impl Op {
    fn run(&self, fx: &Fixture, cache: &mut Cache) {
        let at = |pick: &Pick| fx.work.join(pick.path());
        // Ошибки игнорируются: случайная операция вполне может не иметь смысла на текущем диске.
        match self {
            Self::Create { at: p } => drop(fs::File::create_new(at(p))),
            Self::Write { at: p, len } => drop(fs::write(at(p), vec![b'x'; *len])),
            Self::MakeDir { at: p } => drop(fs::create_dir(at(p))),
            Self::Rename { from, to } => drop(fs::rename(at(from), at(to))),
            Self::Move { from, to, replace } => {
                if let Some(request) = move_request(fx, &from.path(), &to.path(), *replace) {
                    drop(fx.workdir.handle(request));
                }
            }
            Self::Remove { at: p } => {
                if fs::remove_dir_all(at(p)).is_err() {
                    drop(fs::remove_file(at(p)));
                }
            }
            Self::Chmod { at: p, mode } => drop(fs::set_permissions(at(p), fs::Permissions::from_mode(MODES[*mode]))),
            Self::MoveOut { at: p } => drop(fs::rename(at(p), fx.outside.join(NAMES[p.name]))),
            Self::MoveIn { at: p } => {
                let source = fx.outside.join(NAMES[p.name]);
                drop(fs::File::create_new(&source));
                drop(fs::rename(source, at(p)));
            }
            Self::ReadDir { dir } => cache.read_dir(fx, PARENTS[*dir]),
            Self::Flush => cache.apply(&fx.flush()),
        }
    }
}

/// Запрос `Rename` с верными ожиданиями по состоянию диска; `None`, если источник — не читаемый файл.
fn move_request(fx: &Fixture, from: &str, to: &str, replace: bool) -> Option<FsRequest> {
    let expect_from = sha(&fs::read(fx.work.join(from)).ok()?);
    let expect_to = match (replace, fs::read(fx.work.join(to))) {
        (true, Ok(bytes)) => Expect::Hash(sha(&bytes)),
        _ => Expect::Absent,
    };
    Some(FsRequest::Rename {
        from: from.into(),
        to: to.into(),
        expect_from,
        expect_to,
    })
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Начальное дерево, прочитанное сессией целиком.
fn start() -> (Fixture, Cache) {
    let fx = Fixture::new();
    for dir in ["d0", "d1", "d0/s"] {
        fs::create_dir(fx.work.join(dir)).unwrap();
    }
    for file in ["a", "d0/a", "d1/b"] {
        fs::write(fx.work.join(file), "x").unwrap();
    }
    let mut cache = Cache::default();
    for dir in PARENTS {
        cache.read_dir(&fx, dir);
    }
    (fx, cache)
}

#[track_caller]
fn check_ops(ops: &[Op]) {
    let (fx, mut cache) = start();
    for (i, op) in ops.iter().enumerate() {
        op.run(&fx, &mut cache);
        if matches!(op, Op::Flush) {
            cache.verify_invariants(&fx, &format!("{:?}", &ops[..=i]));
        }
    }
    cache.apply(&fx.flush());
    cache.verify_invariants(&fx, &format!("{ops:?}"));
}

proptest! {
    #[test]
    fn cache_equals_disk_after_quiet(ops in proptest::collection::vec(any::<Op>(), 1..40)) {
        check_ops(&ops);
    }
}

fn dump(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|change| match change.kind {
            ChangeKind::Updated(attr) => format!("Updated {:?} {:?}", change.path, attr.kind),
            ChangeKind::Removed => format!("Removed {:?}", change.path),
            ChangeKind::Unwatched => format!("Unwatched {:?}", change.path),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Каталог удалён и создан заново: сессия должна забыть всё, что знала под ним, хотя на его месте снова есть каталог.
#[test]
fn recreated_directory_is_removed_before_it_is_updated() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fs::write(fx.work.join("d/f"), "x").unwrap();
    fx.read_dir("");
    fx.read_dir("d");

    fs::remove_dir_all(fx.work.join("d")).unwrap();
    fs::create_dir(fx.work.join("d")).unwrap();

    assert_snapshot!(dump(&fx.flush()), @r#"
    Removed "d"
    Updated "" Dir
    Updated "d" Dir
    Removed "d/f"
    "#);
}

/// Переименованный каталог не должен держать подписку под старым именем: события шли бы под чужим путём.
#[test]
fn renamed_directory_drops_its_subscriptions() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.work.join("d/e")).unwrap();
    fx.read_dir("");
    fx.read_dir("d");
    fx.read_dir("d/e");
    fs::rename(fx.work.join("d"), fx.work.join("moved")).unwrap();
    fx.flush();
    assert_eq!(fx.subscriptions(), 1, "ядро держит подписки каталогов под старым именем");

    fs::write(fx.work.join("moved/e/f"), "x").unwrap();
    fs::write(fx.work.join("moved/g"), "x").unwrap();

    assert_eq!(
        dump(&fx.flush()),
        "",
        "изменения под переименованным каталогом пришли под старыми путями"
    );
}

/// Путь, у которого не получить атрибуты не по причине «нет такого», роняет доверие к записям его каталога.
#[test]
fn unreadable_path_makes_its_directory_unwatched() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fs::write(fx.work.join("d/x"), "x").unwrap();
    fx.read_dir("d");

    // Событие о `d/x` уже записано, а на месте `d` теперь файл: `d/x` не открыть.
    fs::remove_dir_all(fx.work.join("d")).unwrap();
    fs::write(fx.work.join("d"), "now a file").unwrap();

    let changes = fx.flush();
    assert!(
        changes.contains(&Change {
            path: "d".into(),
            kind: ChangeKind::Unwatched
        }),
        "путь с ошибкой доступа не снял доверие к каталогу: {}",
        dump(&changes)
    );
}

/// Каталог, достигнутый через ссылку внутри рабочего каталога, под наблюдение не встаёт: события шли бы под путём цели,
/// а кэш сессии хранит путь ссылки.
#[test]
fn directory_behind_a_symlink_is_reported_unwatched() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("real")).unwrap();
    symlink("real", fx.work.join("alias")).unwrap();

    fx.read_dir("alias");
    fs::write(fx.work.join("real/f"), "x").unwrap();

    assert_snapshot!(dump(&fx.flush()), @r#"Unwatched "alias""#);
}

/// Количество событий, после которого очередь inotify переполняется.
fn queue_limit() -> usize {
    let text = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events").unwrap();
    text.trim().parse().unwrap()
}

/// Создаёт файлы, пока очередь не переполнится: каждый файл — события `CREATE` и `CLOSE_WRITE`.
fn overflow_queue(dir: &std::path::Path) {
    for i in 0..=queue_limit() {
        fs::File::create(dir.join(format!("f{i}"))).unwrap();
    }
}

#[test]
fn queue_overflow_makes_every_directory_unwatched() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fx.read_dir("");
    fx.read_dir("d");

    overflow_queue(&fx.work.join("d"));
    let changes = fx.flush();
    let unwatched: Vec<&str> = changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Unwatched)
        .map(|c| c.path.as_str())
        .collect();
    assert_eq!(
        unwatched,
        ["", "d"],
        "после переполнения очереди не все каталоги потеряли наблюдение"
    );
    assert_eq!(
        changes.first().map(|c| c.kind),
        Some(ChangeKind::Unwatched),
        "потеря наблюдения идёт не первой: сессия применила бы устаревшие атрибуты поверх неё"
    );

    fs::write(fx.work.join("later"), "x").unwrap();
    assert_eq!(dump(&fx.flush()), "", "каталог без наблюдения продолжил слать события");
}

/// Лимит `max_user_watches` без прав root не понизить, поэтому проверка перезапускает себя в пользовательском
/// пространстве имён (`unshare -Ur`), где лимит свой и записываемый. Нет `unshare` или пространств имён
/// (например, в контейнере с seccomp) — проверить нечем, тест выходит.
#[test]
fn exhausted_watch_limit_is_reported_as_unwatched() {
    const CHILD: &str = "IAPETUS_WATCH_LIMIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let available = Command::new("unshare")
            .args(["-Ur", "true"])
            .status()
            .is_ok_and(|status| status.success());
        if !available {
            return;
        }
        let status = Command::new("unshare")
            .args(["-Ur", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runner::fs::watch::tests::exhausted_watch_limit_is_reported_as_unwatched",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "проверка в пространстве имён с лимитом подписок упала");
        return;
    }
    fs::write("/proc/sys/user/max_inotify_watches", "2").unwrap();
    let fx = Fixture::new();
    for dir in ["a", "b"] {
        fs::create_dir(fx.work.join(dir)).unwrap();
    }
    // Две подписки укладываются в лимит, третья — нет; чтение каталога при этом удаётся.
    for dir in ["", "a", "b"] {
        assert!(fx.read_dir(dir).is_some(), "чтение каталога {dir:?} не удалось");
    }
    assert_eq!(
        dump(&fx.flush()),
        r#"Unwatched "b""#,
        "каталог сверх лимита не получил «без наблюдения»"
    );
    fs::write(fx.work.join("b/f"), "x").unwrap();
    assert_eq!(dump(&fx.flush()), "", "каталог без подписки прислал событие");
}

impl Fixture {
    /// Сколько подписок держит ядро: считается по `fdinfo` очереди, а не по таблицам runner.
    fn subscriptions(&self) -> usize {
        let fd = self.workdir.watcher.inotify.as_raw_fd();
        let info = fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
        info.lines().filter(|line| line.starts_with("inotify wd:")).count()
    }
}

#[test]
fn resync_reports_every_watched_directory_unwatched_and_drops_their_subscriptions_and_pending_events() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fx.read_dir("");
    fx.read_dir("d");
    // Накопленное до resync относится к виду сессии, которого уже нет.
    fs::write(fx.work.join("before"), "x").unwrap();
    fx.workdir.watcher.drain().unwrap();

    fx.workdir.resync_watch();
    assert_snapshot!(dump(&fx.flush()), @r#"
    Unwatched ""
    Unwatched "d"
    "#);
    fs::write(fx.work.join("after"), "x").unwrap();
    fs::write(fx.work.join("d").join("after"), "x").unwrap();
    assert_eq!(fx.flush(), Vec::new(), "после resync каталоги остались под наблюдением");
}
