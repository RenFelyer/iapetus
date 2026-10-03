use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use nerve::protocol::{Content, Error, Expect, FsReply, FsRequest, FsResponse, Kind};
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::Workdir;

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Рабочий каталог рядом с каталогом `outside`, где лежит файл, которого трогать нельзя.
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
        fs::write(outside.join("secret"), "SECRET").unwrap();
        let workdir = Workdir::open(&work).unwrap();
        Self {
            _base: base,
            work,
            outside,
            workdir,
        }
    }

    fn stat(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::Stat { path: path.into() })
    }

    fn read_dir(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::ReadDir { path: path.into() })
    }

    fn read(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::Read { path: path.into() })
    }

    fn read_link(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::ReadLink { path: path.into() })
    }

    fn write(&self, path: &str, expect: Expect, bytes: &[u8]) -> FsResponse {
        self.workdir.handle(FsRequest::Write {
            path: path.into(),
            expect,
            content: Content(bytes.to_vec()),
        })
    }

    fn make_dir(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::MakeDir { path: path.into() })
    }

    fn hash(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::Hash { path: path.into() })
    }

    fn remove(&self, path: &str) -> FsResponse {
        self.workdir.handle(FsRequest::Remove { path: path.into() })
    }

    fn rename(&self, from: &str, to: &str, expect_from: [u8; 32], expect_to: Expect) -> FsResponse {
        self.workdir.handle(FsRequest::Rename {
            from: from.into(),
            to: to.into(),
            expect_from,
            expect_to,
        })
    }

    #[track_caller]
    fn assert_outside_is_untouched(&self) {
        let names: Vec<_> = fs::read_dir(&self.outside).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["secret"], "в каталоге вне рабочего появились или пропали файлы");
        assert_eq!(
            fs::read(self.outside.join("secret")).unwrap(),
            b"SECRET",
            "файл вне рабочего каталога изменён"
        );
    }
}

/// Все операции по пути, проходящему через ссылку или выход наружу, отказывают одинаково.
#[track_caller]
fn assert_every_operation_is_outside(fx: &Fixture, path: &str) {
    // Источник и цель по очереди: каждый конец проверяется при верном другом.
    fs::write(fx.work.join("inside"), "I").unwrap();
    let responses = [
        ("Stat", fx.stat(path)),
        ("ReadDir", fx.read_dir(path)),
        ("Read", fx.read(path)),
        ("ReadLink", fx.read_link(path)),
        ("Write Absent", fx.write(path, Expect::Absent, b"X")),
        ("Write Hash", fx.write(path, Expect::Hash(sha(b"SECRET")), b"X")),
        ("MakeDir", fx.make_dir(path)),
        ("Hash", fx.hash(path)),
        ("Remove", fx.remove(path)),
        ("Rename из", fx.rename(path, "moved", sha(b"SECRET"), Expect::Absent)),
        ("Rename в", fx.rename("inside", path, sha(b"I"), Expect::Absent)),
        (
            "Rename в поверх",
            fx.rename("inside", path, sha(b"I"), Expect::Hash(sha(b"SECRET"))),
        ),
    ];
    for (operation, response) in responses {
        assert_eq!(
            response,
            Err(Error::OutsideWorkdir),
            "{operation} по пути {path:?} не отказал с OutsideWorkdir"
        );
    }
    assert_eq!(
        fs::read(fx.work.join("inside")).unwrap(),
        b"I",
        "файл внутри перенесён по пути наружу"
    );
    assert!(!fx.work.join("moved").exists(), "файл появился в рабочем каталоге из пути наружу");
    fx.assert_outside_is_untouched();
}

#[test]
fn dotdot_out_of_workdir_is_rejected() {
    let fx = Fixture::new();
    for path in ["../outside/secret", "a/../../outside/secret", "..", "d/../.."] {
        assert_every_operation_is_outside(&fx, path);
    }
}

/// Ядро разрешило бы `a/../b` (он остаётся внутри), но сессия присылает только прямые пути.
#[test]
fn dotdot_staying_inside_is_rejected_too() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("a")).unwrap();
    fs::write(fx.work.join("b"), "B").unwrap();
    assert_eq!(fx.read("a/../b"), Err(Error::OutsideWorkdir));
}

/// Путь `/<рабочий каталог>/file` существует, но абсолютный путь не относительный: его нельзя читать как `<рабочий каталог>/file` внутри.
#[test]
fn absolute_path_is_rejected_even_when_it_points_inside() {
    let fx = Fixture::new();
    fs::write(fx.work.join("file"), "F").unwrap();
    let inside = format!("{}/file", fx.work.display());
    assert_eq!(fx.read(&inside), Err(Error::OutsideWorkdir));
    let secret = format!("{}/secret", fx.outside.display());
    assert_every_operation_is_outside(&fx, &secret);
}

#[test]
fn symlink_to_outside_directory_is_rejected_at_every_level() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.work.join("d/e")).unwrap();
    symlink("../outside", fx.work.join("l0")).unwrap();
    symlink("../../outside", fx.work.join("d/l1")).unwrap();
    symlink("../../../outside", fx.work.join("d/e/l2")).unwrap();
    symlink(&fx.outside, fx.work.join("abs")).unwrap();
    symlink("..", fx.work.join("up")).unwrap();
    for path in [
        "l0/secret",
        "d/l1/secret",
        "d/e/l2/secret",
        "abs/secret",
        "up/outside/secret",
        "l0/new",
        "d/l1/new",
        "abs/new",
    ] {
        assert_every_operation_is_outside(&fx, path);
    }
}

#[test]
fn symlink_to_outside_as_last_component_is_not_followed() {
    let fx = Fixture::new();
    symlink("../outside/secret", fx.work.join("leak")).unwrap();
    symlink("../outside", fx.work.join("leak_dir")).unwrap();
    assert_eq!(fx.read("leak"), Err(Error::OutsideWorkdir));
    assert_eq!(fx.read_dir("leak_dir"), Err(Error::OutsideWorkdir));
    // Запись по ссылке заменила бы саму ссылку, а не файл, на который она указывает: отказ.
    assert_eq!(fx.write("leak", Expect::Hash(sha(b"SECRET")), b"X"), Err(Error::Access));
    fx.assert_outside_is_untouched();
    // Атрибуты ссылки — её собственные: lstat наружу не ходит.
    let Ok(FsReply::Stat(attr)) = fx.stat("leak") else {
        panic!("lstat ссылки должен пройти")
    };
    assert_eq!(attr.kind, Kind::Symlink);
}

fn link_target(fx: &Fixture, path: &str) -> FsResponse {
    fx.read_link(path)
}

#[test]
fn read_link_returns_the_target_text_for_targets_inside_the_project() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.work.join("pkg/bin")).unwrap();
    fs::write(fx.work.join("pkg/tool.js"), "T").unwrap();
    symlink("../tool.js", fx.work.join("pkg/bin/tool")).unwrap();
    symlink(fx.work.join("pkg/tool.js"), fx.work.join("abs")).unwrap();
    symlink("missing/file", fx.work.join("dangling")).unwrap();
    symlink("pkg/bin/tool", fx.work.join("chain")).unwrap();
    for (path, target) in [
        ("pkg/bin/tool", "../tool.js".to_owned()),
        ("abs", fx.work.join("pkg/tool.js").display().to_string()),
        ("dangling", "missing/file".to_owned()),
        ("chain", "pkg/bin/tool".to_owned()),
    ] {
        assert_eq!(
            link_target(&fx, path),
            Ok(FsReply::Link(target)),
            "{path}: цель внутри проекта не отдана как есть"
        );
    }
}

#[test]
fn read_link_refuses_targets_that_resolve_outside_the_project_and_home() {
    let mut fx = Fixture::new();
    fx.workdir.home = Some(fx.outside.join("home"));
    fs::create_dir_all(fx.outside.join("home")).unwrap();
    symlink("../../etc/passwd", fx.work.join("up")).unwrap();
    symlink("/etc/passwd", fx.work.join("abs")).unwrap();
    symlink("../outside/secret", fx.work.join("sibling")).unwrap();
    symlink("../outside", fx.work.join("dir")).unwrap();
    // Первая ссылка внутри проекта, но её цель ведёт наружу: канонический путь цепочки снаружи.
    symlink("dir/secret", fx.work.join("via_link")).unwrap();
    symlink("missing/../../outside/secret", fx.work.join("dangling_dotdot")).unwrap();
    for path in ["up", "abs", "sibling", "dir", "via_link", "dangling_dotdot"] {
        assert_eq!(
            link_target(&fx, path),
            Err(Error::Access),
            "{path}: цель вне проекта и `$HOME` отдана"
        );
    }
    // Цель внутри `$HOME` отдаётся.
    symlink(fx.outside.join("home/.config"), fx.work.join("in_home")).unwrap();
    assert_eq!(
        link_target(&fx, "in_home"),
        Ok(FsReply::Link(fx.outside.join("home/.config").display().to_string())),
        "цель внутри `$HOME` не отдана"
    );
}

#[test]
fn read_link_refuses_regular_files_the_workdir_and_missing_paths() {
    let fx = Fixture::new();
    fs::write(fx.work.join("file"), "F").unwrap();
    assert!(
        matches!(link_target(&fx, "file"), Err(Error::Io(_))),
        "обычный файл прочитан как ссылка"
    );
    assert!(
        matches!(link_target(&fx, ""), Err(Error::Io(_))),
        "рабочий каталог прочитан как ссылка"
    );
    assert_eq!(link_target(&fx, "nope"), Err(Error::NotFound));
}

#[test]
fn symlink_loop_is_an_error_not_a_hang() {
    let fx = Fixture::new();
    symlink("b", fx.work.join("a")).unwrap();
    symlink("a", fx.work.join("b")).unwrap();
    for path in ["a", "a/x", "b"] {
        assert!(matches!(fx.read(path), Err(Error::Io(_))), "{path}: цикл ссылок не стал ошибкой");
    }
    assert!(matches!(fx.write("a/x", Expect::Absent, b"X"), Err(Error::Io(_))));
}

/// Каталог то каталог, то ссылка наружу: ядро проверяет выход при каждом открытии, так что файл не попадает наружу.
#[test]
fn directory_swapped_for_symlink_never_writes_outside() {
    let fx = Fixture::new();
    let dir = fx.work.join("d");
    fs::create_dir(&dir).unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                // Писатель мог положить файл в каталог посреди удаления: тогда пробуем снова.
                if fs::remove_dir_all(&dir).is_err() {
                    continue;
                }
                symlink(&fx.outside, &dir).unwrap();
                fs::remove_file(&dir).unwrap();
                fs::create_dir(&dir).unwrap();
            }
        });
        for round in 0..3000 {
            let expect = if round % 2 == 0 { Expect::Absent } else { Expect::Hash(sha(b"X")) };
            drop(fx.write("d/f", expect, b"X"));
            drop(fx.make_dir("d/sub"));
        }
        stop.store(true, Ordering::Relaxed);
    });
    fx.assert_outside_is_untouched();
}

#[test]
fn concurrent_writes_with_the_same_expectation_let_exactly_one_win() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("real")).unwrap();
    symlink("real", fx.work.join("alias")).unwrap();
    let original = vec![7u8; 256 * 1024];
    fs::write(fx.work.join("real/f"), &original).unwrap();
    for round in 0..40 {
        // Ожидание читается до запуска писателей: прочитанное между запусками могло бы уже быть содержимым победителя.
        let expect = sha(&fs::read(fx.work.join("real/f")).unwrap());
        let winners = thread::scope(|scope| {
            let handles: Vec<_> = (0..8u8)
                .map(|i| {
                    let fx = &fx;
                    // Часть писателей идёт к тому же файлу через ссылку на каталог: блокировка должна быть общей.
                    let path = if i % 2 == 0 { "real/f" } else { "alias/f" };
                    scope.spawn(move || {
                        let mut content = vec![i; 256 * 1024];
                        content[0] = round;
                        (fx.write(path, Expect::Hash(expect), &content), content)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        let wins: Vec<_> = winners.iter().filter(|(response, _)| response.is_ok()).collect();
        assert_eq!(
            wins.len(),
            1,
            "раунд {round}: записей с одним ожиданием прошло {}, должна пройти одна",
            wins.len()
        );
        assert_eq!(
            fs::read(fx.work.join("real/f")).unwrap(),
            wins[0].1,
            "раунд {round}: на диске не содержимое победителя"
        );
        for (response, _) in &winners {
            assert!(
                matches!(response, Ok(_) | Err(Error::Conflict)),
                "раунд {round}: проигравший получил {response:?}"
            );
        }
    }
}

/// Запись идёт через временный файл и `rename`: читатель видит старую или новую версию целиком, но не половину.
#[test]
fn concurrent_reader_never_sees_a_partial_file() {
    let fx = Fixture::new();
    let versions = [vec![b'A'; 256 * 1024], vec![b'B'; 256 * 1024]];
    fs::write(fx.work.join("f"), &versions[0]).unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let seen = fs::read(fx.work.join("f")).unwrap();
                assert!(
                    versions.contains(&seen),
                    "читатель увидел файл длиной {} байт, не равный ни одной версии",
                    seen.len()
                );
            }
        });
        // Результаты разбираем после остановки читателя: паника посреди цикла оставила бы его крутиться вечно.
        let results: Vec<_> = (0..30)
            .map(|round| {
                let (old, new) = (&versions[round % 2], &versions[(round + 1) % 2]);
                fx.write("f", Expect::Hash(sha(old)), new)
            })
            .collect();
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert!(results.iter().all(Result::is_ok), "запись не прошла: {results:?}");
    });
}

#[test]
fn existing_file_keeps_its_mode_on_write() {
    let fx = Fixture::new();
    let path = fx.work.join("script.sh");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    let Ok(FsReply::Written { attr, .. }) = fx.write("script.sh", Expect::Hash(sha(b"old")), b"new") else {
        panic!("запись не прошла")
    };
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        0o750,
        "права файла на диске изменились"
    );
    assert_eq!(attr.mode, 0o750, "в ответе не права файла");
}

#[test]
fn remove_refuses_non_empty_dir_and_the_workdir_itself() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fs::write(fx.work.join("d/f"), "F").unwrap();
    assert_eq!(fx.remove("d"), Err(Error::NotEmpty));
    assert!(fx.work.join("d/f").exists(), "файл в непустом каталоге пропал");
    assert!(matches!(fx.remove(""), Err(Error::Io(_))));
    assert!(fx.work.exists(), "рабочий каталог удалён");
}

#[test]
fn rename_moves_a_file_and_keeps_its_mode() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    let source = fx.work.join("script.sh");
    fs::write(&source, "body").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    let Ok(FsReply::Renamed(attr)) = fx.rename("script.sh", "d/moved.sh", sha(b"body"), Expect::Absent) else {
        panic!("перенос не прошёл")
    };
    assert_eq!(
        (attr.kind, attr.size, attr.mode),
        (Kind::File, 4, 0o750),
        "атрибуты в ответе — файла на новом месте"
    );
    assert!(!source.exists(), "источник остался");
    assert_eq!(fs::read(fx.work.join("d/moved.sh")).unwrap(), b"body");
    assert_eq!(
        fs::metadata(fx.work.join("d/moved.sh")).unwrap().permissions().mode() & 0o7777,
        0o750
    );
}

#[test]
fn rename_refuses_directories_symlinks_and_a_directory_in_the_way() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("d")).unwrap();
    fs::write(fx.work.join("f"), "F").unwrap();
    symlink("f", fx.work.join("l")).unwrap();
    assert!(
        matches!(fx.rename("d", "d2", sha(b""), Expect::Absent), Err(Error::Io(_))),
        "каталог перенесён"
    );
    assert_eq!(
        fx.rename("l", "l2", sha(b"F"), Expect::Absent),
        Err(Error::Access),
        "ссылка перенесена"
    );
    assert_eq!(
        fx.rename("f", "d", sha(b"F"), Expect::Absent),
        Err(Error::Conflict),
        "файл занял место каталога"
    );
    assert!(
        matches!(fx.rename("f", "d", sha(b"F"), Expect::Hash(sha(b""))), Err(Error::Io(_))),
        "файл заменил каталог"
    );
    assert_eq!(
        fx.rename("f", "l", sha(b"F"), Expect::Absent),
        Err(Error::Conflict),
        "файл затёр ссылку"
    );
    assert_eq!(
        fx.rename("f", "l", sha(b"F"), Expect::Hash(sha(b"F"))),
        Err(Error::Access),
        "файл заменил ссылку"
    );
    assert!(fx.work.join("d").is_dir() && fs::read(fx.work.join("f")).unwrap() == b"F" && fx.work.join("l").is_symlink());
    assert!(!fx.work.join("d2").exists() && !fx.work.join("l2").exists());
}

#[test]
fn rename_to_itself_and_of_the_workdir_are_refused() {
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("real")).unwrap();
    symlink("real", fx.work.join("alias")).unwrap();
    fs::write(fx.work.join("real/f"), "F").unwrap();
    // Тот же файл через ссылку на каталог: блокировка одна, и перенос сам на себя отказывает, а не виснет.
    for to in ["real/f", "alias/f"] {
        let response = fx.rename("real/f", to, sha(b"F"), Expect::Hash(sha(b"F")));
        assert!(matches!(response, Err(Error::Io(_))), "перенос на себя ({to}): {response:?}");
    }
    assert!(matches!(fx.rename("", "x", sha(b""), Expect::Absent), Err(Error::Io(_))));
    assert!(matches!(fx.rename("real/f", "", sha(b"F"), Expect::Absent), Err(Error::Io(_))));
    assert_eq!(fs::read(fx.work.join("real/f")).unwrap(), b"F");
}

/// Два переноса навстречу друг другу берут блокировки в одном порядке: без этого каждый ждал бы второго вечно.
#[test]
fn opposing_renames_do_not_deadlock() {
    // Тишина, после которой потоки считаются взаимоблокированными: ни один запрос ни в одном потоке не завершился.
    const STALL: std::time::Duration = std::time::Duration::from_secs(30);
    let fx = Fixture::new();
    fs::create_dir(fx.work.join("x")).unwrap();
    fs::create_dir(fx.work.join("y")).unwrap();
    fs::write(fx.work.join("x/f"), "A").unwrap();
    // Потоки не из области: взаимоблокированные, они не дали бы тесту упасть, а так отстают за ним.
    let workdir = Arc::new(Workdir::open(&fx.work).unwrap());
    let completed = Arc::new(AtomicUsize::new(0));
    let (done, finished) = std::sync::mpsc::channel();
    // Оба потока начинают вместе: иначе первый успевает закончить до старта второго.
    let start = Arc::new(std::sync::Barrier::new(2));
    for (from, to) in [("x/f", "y/f"), ("y/f", "x/f")] {
        let (workdir, completed, done, start) = (Arc::clone(&workdir), Arc::clone(&completed), done.clone(), Arc::clone(&start));
        thread::spawn(move || {
            start.wait();
            let outcomes: Vec<_> = (0..20_000)
                .map(|i| {
                    // Блокировки обоих имён перенос берёт до любых проверок, поэтому запрос с устаревшим ожиданием
                    // источника сталкивается со встречным так же, как настоящий, но отказывает без fsync каталогов.
                    // Настоящий перенос — каждый сотый: на файловой системе с дорогим fsync каталога (btrfs и
                    // overlayfs поверх неё — около 10 мс на перенос) 40 000 настоящих переносов шли бы минутами.
                    let expect_from = if i % 100 == 0 { sha(b"A") } else { sha(b"stale") };
                    let outcome = workdir.handle(FsRequest::Rename {
                        from: from.into(),
                        to: to.into(),
                        expect_from,
                        expect_to: Expect::Absent,
                    });
                    completed.fetch_add(1, Ordering::Relaxed);
                    outcome
                })
                .collect();
            done.send(outcomes).unwrap();
        });
    }
    let mut moved = 0;
    let mut seen = 0;
    for _ in 0..2 {
        // Медленный перенос — не взаимоблокировка: падение только если за STALL не завершился ни один запрос.
        let outcomes = loop {
            match finished.recv_timeout(STALL) {
                Ok(outcomes) => break outcomes,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let now = completed.load(Ordering::Relaxed);
                    assert!(
                        now != seen,
                        "переносы навстречу друг другу взаимоблокированы: за {STALL:?} ни один не завершился"
                    );
                    seen = now;
                }
                Err(e) => panic!("поток переносов упал: {e}"),
            }
        };
        assert!(
            outcomes.iter().all(|r| matches!(r, Ok(_) | Err(Error::NotFound | Error::Conflict))),
            "неожиданный ответ среди переносов навстречу"
        );
        moved += outcomes.iter().filter(|r| r.is_ok()).count();
    }
    // Без настоящих переносов тест проверял бы только отказы, а не встречное движение файла.
    assert!(moved > 0, "ни один перенос навстречу не прошёл");
    let left: Vec<_> = ["x/f", "y/f"].into_iter().filter(|path| fx.work.join(path).exists()).collect();
    assert_eq!(left.len(), 1, "файл размножился или пропал: {left:?}");
}

/// Несколько источников метят в одну цель с одним ожиданием: проходит ровно один, остальные видят чужую версию.
#[test]
fn concurrent_renames_onto_one_target_let_exactly_one_win() {
    let fx = Fixture::new();
    fs::write(fx.work.join("target"), "T").unwrap();
    for round in 0..40u8 {
        for i in 0..8u8 {
            fs::write(fx.work.join(format!("s{i}")), [round, i]).unwrap();
        }
        let current = fs::read(fx.work.join("target")).unwrap();
        let responses = thread::scope(|scope| {
            let handles: Vec<_> = (0..8u8)
                .map(|i| {
                    let (fx, current) = (&fx, &current);
                    scope.spawn(move || fx.rename(&format!("s{i}"), "target", sha(&[round, i]), Expect::Hash(sha(current))))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        let wins: Vec<_> = responses.iter().filter(|response| response.is_ok()).collect();
        assert_eq!(
            wins.len(),
            1,
            "раунд {round}: переносов с одним ожиданием прошло {}: {responses:?}",
            wins.len()
        );
        assert!(
            responses.iter().all(|r| matches!(r, Ok(_) | Err(Error::Conflict))),
            "раунд {round}: {responses:?}"
        );
        let survivors = (0..8u8).filter(|i| fx.work.join(format!("s{i}")).exists()).count();
        assert_eq!(survivors, 7, "раунд {round}: источник победителя остался или чужой пропал");
        for i in 0..8u8 {
            drop(fs::remove_file(fx.work.join(format!("s{i}"))));
        }
    }
}

/// Запись в источник и перенос источника с одним ожиданием: проходит ровно одно, и перенесённым оказывается файл,
/// который сессия видела, а не только что записанный.
#[test]
fn rename_and_write_of_the_same_source_with_one_expectation_let_exactly_one_win() {
    let fx = Fixture::new();
    for round in 0..200u32 {
        fs::write(fx.work.join("s"), "S").unwrap();
        let (renamed, written) = thread::scope(|scope| {
            let mover = scope.spawn(|| fx.rename("s", "t", sha(b"S"), Expect::Absent));
            let writer = scope.spawn(|| fx.write("s", Expect::Hash(sha(b"S")), b"W"));
            (mover.join().unwrap(), writer.join().unwrap())
        });
        assert!(
            renamed.is_ok() != written.is_ok(),
            "раунд {round}: перенос {renamed:?} и запись {written:?} прошли оба или ни один"
        );
        if renamed.is_ok() {
            assert_eq!(
                fs::read(fx.work.join("t")).unwrap(),
                b"S",
                "раунд {round}: перенесена не та версия, что видела сессия"
            );
        }
        drop(fs::remove_file(fx.work.join("s")));
        drop(fs::remove_file(fx.work.join("t")));
    }
}

/// `openat2` с `RESOLVE_BENEATH` отвечает `EAGAIN`, если во время разбора пути с `..` где-то в системе прошёл `rename`;
/// runner повторяет открытие.
#[test]
fn lookups_survive_renames_happening_elsewhere_in_the_system() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.work.join("d/e")).unwrap();
    fs::write(fx.work.join("d/e/f"), "F").unwrap();
    symlink("../d/e", fx.work.join("d/link")).unwrap();
    let noise = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        for i in 0..4 {
            let (from, to) = (noise.path().join(format!("x{i}")), noise.path().join(format!("y{i}")));
            fs::write(&from, "x").unwrap();
            let stop = &stop;
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    fs::rename(&from, &to).unwrap();
                    fs::rename(&to, &from).unwrap();
                }
            });
        }
        let failures: Vec<_> = std::iter::repeat_with(|| fx.stat("d/link/f"))
            .take(100_000)
            .filter(Result::is_err)
            .take(3)
            .collect();
        stop.store(true, Ordering::Relaxed);
        assert!(failures.is_empty(), "поиск файла споткнулся о rename в другом месте: {failures:?}");
    });
}

/// Перенос с «цели нет» одновременно с записью в ту же цель: файл, появившийся между проверкой и переносом, не затирается.
#[test]
fn rename_onto_an_absent_target_never_overwrites_a_concurrent_writer() {
    let fx = Fixture::new();
    for round in 0..200u32 {
        fs::write(fx.work.join("s"), "S").unwrap();
        let (renamed, written) = thread::scope(|scope| {
            let mover = scope.spawn(|| fx.rename("s", "t", sha(b"S"), Expect::Absent));
            let writer = scope.spawn(|| fx.write("t", Expect::Absent, b"W"));
            (mover.join().unwrap(), writer.join().unwrap())
        });
        assert!(
            renamed.is_ok() != written.is_ok(),
            "раунд {round}: перенос {renamed:?} и запись {written:?} прошли оба или ни один"
        );
        let on_disk = fs::read(fx.work.join("t")).unwrap();
        assert_eq!(
            on_disk,
            if renamed.is_ok() { b"S" } else { b"W" },
            "раунд {round}: на диске не победитель"
        );
        fs::remove_file(fx.work.join("t")).unwrap();
        drop(fs::remove_file(fx.work.join("s")));
    }
}

// ---- Эталонная модель «путь → байты» против настоящей файловой системы ----

const DIRS: [&str; 2] = ["d", "d/e"];
const FILES: [&str; 3] = ["a", "d/b", "d/e/c"];

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..6)
}

/// Что сессия считает версией файла.
#[derive(Debug, Clone, Copy, proptest_derive::Arbitrary)]
enum Belief {
    /// Ровно то, что сейчас на runner.
    Current,
    /// Файла нет.
    Absent,
    /// Версия, которой на runner нет.
    Stale,
}

#[derive(Debug, Clone, proptest_derive::Arbitrary)]
enum Op {
    MakeDir {
        #[proptest(strategy = "0..DIRS.len()")]
        dir: usize,
    },
    Write {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
        #[proptest(strategy = "any::<Belief>()")]
        belief: Belief,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    Read {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Кто-то другой на машине runner меняет файл мимо протокола.
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
    Hash {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    Remove {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Перенос файла; источник с `stale_source` — версия, которой на runner нет.
    Rename {
        #[proptest(strategy = "0..FILES.len()")]
        from: usize,
        #[proptest(strategy = "0..FILES.len()")]
        to: usize,
        #[proptest(strategy = "any::<bool>()")]
        stale_source: bool,
        #[proptest(strategy = "any::<Belief>()")]
        target: Belief,
    },
}

struct Harness {
    fx: Fixture,
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
}

impl Harness {
    fn new() -> Self {
        Self {
            fx: Fixture::new(),
            files: BTreeMap::new(),
            dirs: BTreeSet::new(),
        }
    }

    fn parent_exists(&self, path: &str) -> bool {
        let parent = parent_of(path);
        parent.is_empty() || self.dirs.contains(parent)
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::MakeDir { dir } => {
                let path = DIRS[*dir];
                let expected = if self.dirs.contains(path) {
                    Err(Error::Conflict)
                } else if !self.parent_exists(path) {
                    Err(Error::NotFound)
                } else {
                    self.dirs.insert(path.to_owned());
                    Ok(())
                };
                assert_eq!(self.fx.make_dir(path).map(|_| ()), expected, "MakeDir {path}");
            }
            Op::Write { file, belief, content } => {
                let path = FILES[*file];
                let current = self.files.get(path);
                let expect = match (belief, current) {
                    (Belief::Current, Some(bytes)) => Expect::Hash(sha(bytes)),
                    (Belief::Current | Belief::Absent, None) | (Belief::Absent, Some(_)) => Expect::Absent,
                    (Belief::Stale, bytes) => Expect::Hash(sha(&[bytes.map_or(&[][..], Vec::as_slice), &[0xFF]].concat())),
                };
                let response = self.fx.write(path, expect, content);
                let may_write = match belief {
                    Belief::Current => true,
                    Belief::Absent => current.is_none(),
                    Belief::Stale => false,
                };
                if !self.parent_exists(path) {
                    assert_eq!(response, Err(Error::NotFound), "Write {path}: родителя нет");
                } else if may_write {
                    assert!(
                        matches!(&response, Ok(FsReply::Written { sha256, .. }) if *sha256 == sha(content)),
                        "Write {path}: {response:?}"
                    );
                    self.files.insert(path.to_owned(), content.clone());
                } else {
                    assert_eq!(
                        response,
                        Err(Error::Conflict),
                        "Write {path} с ожиданием {expect:?} при {current:?}"
                    );
                }
            }
            Op::Read { file } => {
                let path = FILES[*file];
                match self.files.get(path) {
                    Some(bytes) => {
                        let Ok(FsReply::Read {
                            content: Content(got),
                            sha256,
                            ..
                        }) = self.fx.read(path)
                        else {
                            panic!("Read {path}: файл в модели есть, а чтение не удалось")
                        };
                        assert_eq!((&got, sha256), (bytes, sha(bytes)), "Read {path}");
                    }
                    None => assert_eq!(self.fx.read(path), Err(Error::NotFound), "Read {path}: файла нет"),
                }
            }
            Op::Hash { file } => {
                let path = FILES[*file];
                match self.files.get(path) {
                    Some(bytes) => {
                        let Ok(FsReply::Hashed { sha256, .. }) = self.fx.hash(path) else {
                            panic!("Hash {path}: файл в модели есть, а хеш не получен")
                        };
                        assert_eq!(sha256, sha(bytes), "Hash {path}");
                    }
                    None => assert!(self.fx.hash(path).is_err(), "Hash {path}: файла нет, а хеш получен"),
                }
            }
            Op::Remove { file } => {
                let path = FILES[*file];
                let expected = if self.files.remove(path).is_some() {
                    Ok(FsReply::Removed)
                } else {
                    Err(Error::NotFound)
                };
                let response = self.fx.remove(path);
                // Нет родителя — тоже NotFound, поэтому для отсутствующего файла ответ один.
                assert_eq!(response, expected, "Remove {path}");
            }
            Op::Rename { .. } => self.rename(op),
            Op::ExternalWrite { file, content } => {
                let path = FILES[*file];
                if self.parent_exists(path) {
                    fs::write(self.fx.work.join(path), content).unwrap();
                    self.files.insert(path.to_owned(), content.clone());
                }
            }
            Op::ExternalRemove { file } => {
                let path = FILES[*file];
                if self.files.remove(path).is_some() {
                    fs::remove_file(self.fx.work.join(path)).unwrap();
                }
            }
        }
    }

    fn rename(&mut self, op: &Op) {
        let Op::Rename {
            from,
            to,
            stale_source,
            target,
        } = *op
        else {
            return;
        };
        let (from, to) = (FILES[from], FILES[to]);
        let source = self.files.get(from).cloned();
        let existing = self.files.get(to).cloned();
        let expect_from = match (&source, stale_source) {
            (Some(bytes), false) => sha(bytes),
            (bytes, _) => sha(&[bytes.as_deref().unwrap_or(&[]), &[0xFF]].concat()),
        };
        let expect_to = match (target, &existing) {
            (Belief::Current, Some(bytes)) => Expect::Hash(sha(bytes)),
            (Belief::Current | Belief::Absent, None) | (Belief::Absent, Some(_)) => Expect::Absent,
            (Belief::Stale, bytes) => Expect::Hash(sha(&[bytes.as_deref().unwrap_or(&[]), &[0xFF]].concat())),
        };
        let response = self.fx.rename(from, to, expect_from, expect_to);
        let target_matches = match target {
            Belief::Current => true,
            Belief::Absent => existing.is_none(),
            Belief::Stale => false,
        };
        let expected = if !self.parent_exists(from) || !self.parent_exists(to) {
            Err(Error::NotFound)
        } else if from == to {
            Err(Error::Io(String::new()))
        } else if source.is_none() {
            Err(Error::NotFound)
        } else if stale_source || !target_matches {
            Err(Error::Conflict)
        } else {
            Ok(())
        };
        match (&response, &expected) {
            (Err(Error::Io(_)), Err(Error::Io(_))) => {}
            (Ok(FsReply::Renamed(attr)), Ok(())) => {
                let bytes = self.files.remove(from).unwrap();
                assert_eq!(attr.size, bytes.len() as u64, "Rename {from} -> {to}: размер в ответе");
                self.files.insert(to.to_owned(), bytes);
            }
            _ => assert_eq!(
                response.map(|_| ()),
                expected,
                "Rename {from} -> {to} (источник {source:?}, цель {existing:?}, ожидания {expect_from:?} / {expect_to:?})"
            ),
        }
    }

    /// Диск равен модели: ни лишних файлов, ни временных, ни потерянных или испорченных.
    #[track_caller]
    fn verify_invariants(&self) {
        let mut on_disk_files = BTreeMap::new();
        let mut on_disk_dirs = BTreeSet::new();
        walk(&self.fx.work, "", &mut on_disk_files, &mut on_disk_dirs);
        assert_eq!(
            on_disk_files, self.files,
            "файлы на диске не равны модели (временные файлы не должны оставаться)"
        );
        assert_eq!(on_disk_dirs, self.dirs, "каталоги на диске не равны модели");
        self.fx.assert_outside_is_untouched();
    }
}

fn walk(root: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>, dirs: &mut BTreeSet<String>) {
    for entry in fs::read_dir(root.join(prefix)).unwrap() {
        let entry = entry.unwrap();
        let path = if prefix.is_empty() {
            entry.file_name().into_string().unwrap()
        } else {
            format!("{prefix}/{}", entry.file_name().to_str().unwrap())
        };
        if entry.file_type().unwrap().is_dir() {
            dirs.insert(path.clone());
            walk(root, &path, files, dirs);
        } else {
            files.insert(path.clone(), fs::read(root.join(&path)).unwrap());
        }
    }
}

#[track_caller]
fn check_ops(ops: &[Op]) {
    let mut harness = Harness::new();
    for op in ops {
        harness.apply(op);
        harness.verify_invariants();
    }
}

proptest! {
    #[test]
    fn disk_always_equals_the_model(ops in proptest::collection::vec(any::<Op>(), 1..30)) {
        check_ops(&ops);
    }
}

/// Конфликт не трогает файл, даже если новое содержимое равно старому по длине и файл только что менял кто-то другой.
#[test]
fn write_after_external_change_conflicts_and_keeps_the_external_version() {
    check_ops(&[
        Op::Write {
            file: 0,
            belief: Belief::Absent,
            content: vec![1],
        },
        Op::ExternalWrite { file: 0, content: vec![2] },
        Op::Write {
            file: 0,
            belief: Belief::Stale,
            content: vec![3],
        },
        Op::Write {
            file: 0,
            belief: Belief::Absent,
            content: vec![4],
        },
        Op::Read { file: 0 },
        Op::ExternalRemove { file: 0 },
        Op::Write {
            file: 0,
            belief: Belief::Stale,
            content: vec![5],
        },
    ]);
}

/// `$HOME` runner рядом с проектом: в нём `.claude` с секретом, ссылки на него и обычные файлы.
struct HomeFixture {
    _base: TempDir,
    home: PathBuf,
    workdir: Workdir,
}

impl HomeFixture {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(base.path()).unwrap();
        let (home, project) = (root.join("home"), root.join("project"));
        fs::create_dir_all(home.join(".claude/sub")).unwrap();
        fs::create_dir(&project).unwrap();
        fs::write(home.join(".claude/secret"), "TOKEN").unwrap();
        fs::write(home.join("note"), "NOTE").unwrap();
        symlink(".claude", home.join("via_dir")).unwrap();
        symlink(".claude/secret", home.join("via_file")).unwrap();
        symlink("../home/.claude/secret", home.join("via_dotdot")).unwrap();
        symlink(home.join(".claude"), home.join("via_abs")).unwrap();
        let workdir = Workdir::open_home(&home, &project).unwrap();
        Self {
            _base: base,
            home,
            workdir,
        }
    }

    fn handle(&self, request: FsRequest) -> FsResponse {
        self.workdir.handle(request)
    }
}

fn path_request(kind: &str, path: &str) -> FsRequest {
    let path = path.to_owned();
    match kind {
        "stat" => FsRequest::Stat { path },
        "read_dir" => FsRequest::ReadDir { path },
        "read" => FsRequest::Read { path },
        "hash" => FsRequest::Hash { path },
        "read_link" => FsRequest::ReadLink { path },
        "make_dir" => FsRequest::MakeDir { path },
        "remove" => FsRequest::Remove { path },
        "write" => FsRequest::Write {
            path,
            expect: Expect::Absent,
            content: Content(b"x".to_vec()),
        },
        other => panic!("неизвестный вид запроса {other}"),
    }
}

#[test]
fn home_claude_directory_is_closed_by_path_text() {
    let fx = HomeFixture::new();
    for path in [".claude", ".claude/secret", ".claude/sub", "./.claude/secret", ".claude/new"] {
        for kind in ["stat", "read_dir", "read", "hash", "make_dir", "remove", "write"] {
            assert_eq!(
                fx.handle(path_request(kind, path)),
                Err(Error::Access),
                "{kind} {path}: путь в ~/.claude не закрыт"
            );
        }
    }
    let rename = fx.handle(FsRequest::Rename {
        from: "note".into(),
        to: ".claude/note".into(),
        expect_from: sha(b"NOTE"),
        expect_to: Expect::Absent,
    });
    assert_eq!(rename, Err(Error::Access), "перенос в ~/.claude не закрыт");
    assert!(!fx.home.join(".claude/note").exists(), "файл попал в ~/.claude");
    assert!(fx.home.join("note").exists(), "источник отказанного переноса исчез");
}

#[test]
fn home_claude_directory_is_closed_through_links_to_it() {
    let fx = HomeFixture::new();
    for link in ["via_dir", "via_abs"] {
        for (kind, rest) in [
            ("read", "secret"),
            ("hash", "secret"),
            ("read_dir", ""),
            ("read_dir", "sub"),
            ("write", "new"),
            ("make_dir", "d"),
        ] {
            let path = if rest.is_empty() {
                link.to_owned()
            } else {
                format!("{link}/{rest}")
            };
            let response = fx.handle(path_request(kind, &path));
            // Абсолютную ссылку отклоняет само ядро (`OutsideWorkdir`), относительную — проверка пути (`Access`).
            assert!(
                matches!(response, Err(Error::Access | Error::OutsideWorkdir)),
                "{kind} {path}: ссылка на ~/.claude открыла каталог: {response:?}"
            );
        }
    }
    assert!(!fx.home.join(".claude/new").exists(), "запись через ссылку попала в ~/.claude");
    assert!(!fx.home.join(".claude/d").exists(), "каталог через ссылку создан в ~/.claude");
    for link in ["via_dir", "via_file", "via_dotdot", "via_abs"] {
        assert_eq!(
            fx.handle(path_request("read_link", link)),
            Err(Error::Access),
            "{link}: цель в ~/.claude отдана"
        );
    }
    // Сама ссылка видна и удаляется как ссылка: цель не трогается.
    assert!(matches!(fx.handle(path_request("stat", "via_dir")), Ok(FsReply::Stat(attr)) if attr.kind == Kind::Symlink));
    assert_eq!(fx.handle(path_request("remove", "via_dir")), Ok(FsReply::Removed));
    assert!(fx.home.join(".claude/secret").exists(), "удаление ссылки тронуло ~/.claude");
}

#[test]
fn home_listing_has_no_claude_entry_and_other_files_work() {
    let fx = HomeFixture::new();
    let Ok(FsReply::ReadDir { entries, .. }) = fx.handle(path_request("read_dir", "")) else {
        panic!("список $HOME не прочитан");
    };
    let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(!names.contains(&".claude"), "в списке $HOME есть .claude: {names:?}");
    assert!(
        names.contains(&"note") && names.contains(&"via_dir"),
        "из списка пропали обычные записи: {names:?}"
    );

    let written = fx.handle(FsRequest::Write {
        path: "fresh".into(),
        expect: Expect::Absent,
        content: Content(b"data".to_vec()),
    });
    assert!(written.is_ok(), "запись в $HOME: {written:?}");
    assert_eq!(fs::read(fx.home.join("fresh")).unwrap(), b"data");
    let stale = fx.handle(FsRequest::Write {
        path: "fresh".into(),
        expect: Expect::Hash(sha(b"other")),
        content: Content(b"x".to_vec()),
    });
    assert_eq!(stale, Err(Error::Conflict), "запись в $HOME не ждёт хеш");
    assert_eq!(fx.handle(path_request("make_dir", "dir")).map(|_| ()), Ok(()));
    assert_eq!(fx.handle(path_request("remove", "dir")), Ok(FsReply::Removed));
    // Ссылка наружу из $HOME по-прежнему отклоняется ядром.
    symlink("/etc", fx.home.join("etc")).unwrap();
    assert!(
        fx.handle(path_request("read_dir", "etc")).is_err(),
        "ссылка из $HOME наружу открыла /etc"
    );
}
