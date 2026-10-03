//! ФС `$HOME` runner: корень запросов, чтение каталога целиком при первом обращении, записи изоляции профиля, выбор
//! ФС предзагрузкой и разбор пачек watch по корню.

use std::path::Path;

use super::*;

const HOME_DIR: &str = "/home/agent";
const PROJECT_DIR: &str = "/home/agent/proj";
const STATE_LINK: &str = "/var/lib/state/.gemini";

/// ФС `$HOME` с моделью своего runner; запись изоляции `.gemini` отдаёт сама ФС.
fn home_fixture() -> Fixture {
    let runner = ModelRunner::default();
    let wire = runner.serve();
    Fixture::over_with(runner, wire, Profile::Rename, &[], |config| {
        config.root_kind = Root::Home;
        config.root = HOME_DIR.into();
        config.links = vec![Symlink {
            name: ".gemini".to_owned(),
            target: STATE_LINK.to_owned(),
        }];
    })
}

#[test]
fn requests_of_the_home_filesystem_name_the_home_root() {
    run(async {
        let home = home_fixture();
        home.runner.put_file("note", b"x");
        assert_eq!(home.read_file("note").await.unwrap(), b"x");
        let roots = home.runner.roots();
        assert!(
            !roots.is_empty() && roots.iter().all(|root| *root == Root::Home),
            "запросы $HOME ушли не с корнем Home: {roots:?}"
        );

        let project = Fixture::new(Profile::Rename);
        project.runner.put_file("note", b"x");
        project.read_file("note").await.unwrap();
        assert!(
            project.runner.roots().iter().all(|root| *root == Root::Project),
            "запросы проекта ушли с корнем Home"
        );
    });
}

/// Агент ищет файлы настроек в десятке мест: первое обращение к каталогу читает его целиком, а отрицательные
/// `lookup` дальше отвечает кэш эпохи. В проекте так не делается: там обращение — один `Stat`.
#[test]
fn first_lookup_in_a_home_directory_reads_it_whole_and_misses_are_answered_from_the_cache() {
    run(async {
        let fx = home_fixture();
        fx.runner.put_file("present", b"x");
        let root = fx.vfs.getattr(ROOT, None).await.unwrap();
        for name in ["missing-a", "missing-b", "missing-c"] {
            assert_eq!(fx.vfs.lookup(root.ino, name).await.map(|_| ()), Err(Errno::NOENT), "{name}");
        }
        assert!(fx.vfs.lookup(root.ino, "present").await.is_ok(), "существующий файл не найден");
        assert_eq!(fx.runner.count("ReadDir"), 1, "каталог прочитан не один раз");
        assert_eq!(
            fx.runner.count("Stat"),
            1,
            "запросы Stat ушли на runner, а не в кэш: {:?}",
            fx.runner.log()
        );

        // Новая эпоха — новая проверка каталога.
        fx.epoch.advance();
        assert_eq!(fx.vfs.lookup(root.ino, "missing-a").await.map(|_| ()), Err(Errno::NOENT));
        assert_eq!(fx.runner.count("ReadDir"), 2, "каталог не перечитан в новой эпохе");

        let project = Fixture::new(Profile::Rename);
        let root = project.vfs.getattr(ROOT, None).await.unwrap();
        assert_eq!(project.vfs.lookup(root.ino, "missing").await.map(|_| ()), Err(Errno::NOENT));
        assert_eq!(project.runner.count("ReadDir"), 0, "проект читает каталог ради одного lookup");
    });
}

#[test]
fn directory_without_watch_falls_back_to_a_single_stat() {
    run(async {
        let fx = home_fixture();
        fx.runner.set_unwatchable("");
        let root = fx.vfs.getattr(ROOT, None).await.unwrap();
        assert_eq!(fx.vfs.lookup(root.ino, "missing").await.map(|_| ()), Err(Errno::NOENT));
        assert_eq!(fx.vfs.lookup(root.ino, "missing").await.map(|_| ()), Err(Errno::NOENT));
        assert_eq!(fx.runner.count("Stat"), 2, "ответ из каталога без наблюдения закэширован");
    });
}

/// Запись изоляции профиля — ссылка в каталог состояния: её отдаёт сама ФС поверх одноимённой записи runner, без
/// запроса к runner; изменить её через ФС нельзя.
#[test]
fn isolation_entry_is_a_link_to_the_state_directory_served_without_the_runner() {
    run(async {
        let fx = home_fixture();
        fx.runner.put_dir(".gemini");
        let root = fx.vfs.getattr(ROOT, None).await.unwrap();
        fx.runner.clear_log();

        let entry = fx.vfs.lookup(root.ino, ".gemini").await.unwrap();
        assert_eq!(entry.attr.kind, Kind::Symlink, "запись runner заслонила ссылку изоляции");
        assert_eq!(fx.vfs.readlink(entry.ino).await.unwrap(), STATE_LINK);
        assert_eq!(fx.vfs.getattr(entry.ino, None).await.unwrap().attr.size, STATE_LINK.len() as u64);
        assert_eq!(fx.runner.log(), Vec::new(), "запись изоляции спросила runner");

        let fh = fx.vfs.open_dir(root.ino).await.unwrap();
        let items = fx.vfs.read_dir(fh, 0).unwrap();
        let listed = items
            .iter()
            .find(|item| item.name == ".gemini")
            .expect("записи изоляции нет в списке");
        assert_eq!(listed.kind, Kind::Symlink, "в списке запись изоляции — не ссылка");

        assert_eq!(fx.vfs.unlink(root.ino, ".gemini").await, Err(Errno::IO));
        assert_eq!(fx.vfs.rmdir(root.ino, ".gemini").await, Err(Errno::IO));
        assert_eq!(fx.vfs.mkdir(root.ino, ".gemini", CALLER).await.map(|_| ()), Err(Errno::IO));
        let created = fx
            .vfs
            .create(root.ino, ".gemini", 0o644, 0o022, OFlags::WRONLY | OFlags::CREATE, CALLER)
            .await;
        assert_eq!(created.map(|_| ()), Err(Errno::IO));
        assert_eq!(fx.vfs.rename(root.ino, "other", root.ino, ".gemini", 0).await, Err(Errno::IO));
        assert!(
            !fx.runner.log().iter().any(|request| format!("{request:?}").contains(".gemini")),
            "изменение дошло до runner"
        );
    });
}

/// Событие watch относится к одному из каталогов runner: ФС проекта не принимает пачку `$HOME` и наоборот, иначе
/// путь `a` проекта получил бы атрибуты `a` из `$HOME`.
#[test]
fn watch_batches_are_applied_only_by_the_filesystem_of_their_root() {
    run(async {
        let attr = |size| Attr {
            kind: Kind::File,
            size,
            mtime_ns: 1,
            mode: 0o644,
        };
        let batch = |root| WatchBatch {
            root,
            changes: vec![Change {
                path: "a".to_owned(),
                kind: ChangeKind::Updated(attr(99)),
            }],
        };
        for (fx, own, foreign) in [
            (home_fixture(), Root::Home, Root::Project),
            (Fixture::new(Profile::Rename), Root::Project, Root::Home),
        ] {
            fx.runner.put_file("a", b"one");
            let entry = fx.walk("a").await.unwrap();
            fx.vfs.apply_watch(&batch(foreign));
            assert_eq!(
                fx.vfs.getattr(entry.ino, None).await.unwrap().attr.size,
                3,
                "{own:?}: пачка чужого корня применена"
            );
            fx.vfs.apply_watch(&batch(own));
            assert_eq!(
                fx.vfs.getattr(entry.ino, None).await.unwrap().attr.size,
                99,
                "{own:?}: пачка своего корня не применена"
            );
        }
    });
}

/// Предзагрузка идёт через ту ФС, чья точка монтирования — самый длинный префикс пути; не смонтированная и
/// непричастная не трогаются.
#[test]
fn preload_goes_through_the_filesystem_with_the_longest_mount_prefix() {
    run(async {
        let home = home_fixture();
        let project = Fixture::new(Profile::Rename);
        home.runner.put_file("notes.txt", b"h");
        home.runner.put_file("proj/code.rs", b"shadowed");
        project.runner.put_file("code.rs", b"p");
        let vfses = [Arc::new(home.vfs), Arc::new(project.vfs)];
        let ask = |path: &str| {
            let vfses = vfses.clone();
            let path = path.to_owned();
            async move { preload(&vfses, Path::new(&path)).await }
        };

        // Ни одна ФС не смонтирована: предзагрузка никого не выбирает.
        ask(&format!("{HOME_DIR}/notes.txt")).await;
        ask(&format!("{PROJECT_DIR}/code.rs")).await;
        assert_eq!(
            (home.runner.log(), project.runner.log()),
            (Vec::new(), Vec::new()),
            "предзагрузка пошла в несмонтированную ФС"
        );

        vfses[0].set_root(Path::new(HOME_DIR));
        vfses[1].set_root(Path::new(PROJECT_DIR));
        ask(&format!("{HOME_DIR}/notes.txt")).await;
        assert_eq!(
            (home.runner.count("Read"), project.runner.count("Read")),
            (1, 0),
            "файл $HOME не через ФС $HOME"
        );
        ask(&format!("{PROJECT_DIR}/code.rs")).await;
        assert_eq!(
            (home.runner.count("Read"), project.runner.count("Read")),
            (1, 1),
            "файл проекта не через ФС проекта"
        );
        ask("/etc/passwd").await;
        ask(&format!("{HOME_DIR}x/notes.txt")).await;
        assert_eq!(
            (home.runner.count("Read"), project.runner.count("Read")),
            (1, 1),
            "путь вне точек монтирования предзагружен"
        );

        vfses[0].deactivate();
        ask(&format!("{HOME_DIR}/notes.txt")).await;
        assert_eq!(home.runner.count("Read"), 1, "размонтированная ФС $HOME предзагрузила файл");
    });
}
