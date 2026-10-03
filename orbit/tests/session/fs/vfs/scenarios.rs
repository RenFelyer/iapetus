//! Сценарии: каждый проверяет одно обещание.

use nerve::protocol::{self, DataRequest, FsResponse};

use super::*;

/// Профиль `Flush` отказывает уже на `open`, до записи: агент не успевает потерять чужую правку.
#[test]
fn flush_profile_refuses_open_for_write_on_a_changed_file() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        fx.runner.external_write("a", b"two");
        let entry = fx.walk("a").await.unwrap();
        let opened = fx.vfs.open(entry.ino, OFlags::WRONLY | OFlags::TRUNC).await;
        assert_eq!(opened, Err(Errno::CANCELED), "open с обрезкой не отказал при изменённом файле");
        assert_eq!(fx.runner.files()["a"], b"two", "файл на runner изменён при отказе");
    });
}

/// То же для `setattr size`: обрезка файла, который изменился, — отказ.
#[test]
fn truncate_of_a_changed_file_is_canceled() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        fx.runner.external_write("a", b"two");
        let entry = fx.walk("a").await.unwrap();
        let change = SetAttr {
            size: Some(0),
            ..SetAttr::default()
        };
        assert_eq!(fx.vfs.setattr(entry.ino, None, change).await.map(|_| ()), Err(Errno::CANCELED));
        assert_eq!(fx.runner.files()["a"], b"two", "файл на runner обрезан при отказе");
    });
}

/// Обрезка через открытый на запись дескриптор проверяет версию тоже.
#[test]
fn ftruncate_of_a_changed_file_is_canceled_and_of_an_unchanged_one_reaches_the_runner_on_flush() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        let entry = fx.walk("a").await.unwrap();
        let fh = fx.vfs.open(entry.ino, OFlags::WRONLY).await.unwrap();
        let change = SetAttr {
            size: Some(1),
            ..SetAttr::default()
        };
        fx.vfs.setattr(entry.ino, Some(fh), change).await.unwrap();
        fx.vfs.flush(fh).await.unwrap();
        assert_eq!(fx.runner.files()["a"], b"o", "ftruncate не дошёл до runner на flush");
        fx.runner.external_write("a", b"zzz");
        assert_eq!(fx.vfs.setattr(entry.ino, Some(fh), change).await.map(|_| ()), Err(Errno::CANCELED));
        fx.vfs.release(fh).await;
        assert_eq!(fx.runner.files()["a"], b"zzz");
    });
}

/// После смены runner файл читается с нового runner: копия прежнего удалена, метаданные забыты.
#[test]
fn after_switching_the_runner_files_come_from_the_new_one_and_the_old_copies_are_gone() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("a", b"one");
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        let cached = || std::fs::read_dir(fx.cache_dir.path().join("work")).unwrap().count();
        assert_eq!(cached(), 1, "прочитанный файл не лёг в кэш");
        // Другой runner отдаёт под тем же путём другой файл; события watch об этом не приходили.
        fx.runner.external_write("a", b"two");
        assert_eq!(
            fx.read_file("a").await.unwrap(),
            b"one",
            "в пределах эпохи без смены runner файл читается из кэша"
        );

        fx.vfs.switch_runner(&"runner-new".parse().unwrap());

        assert_eq!(cached(), 0, "копия файла прежнего runner осталась в кэше");
        assert_eq!(fx.read_file("a").await.unwrap(), b"two", "прочитан файл прежнего runner");
    });
}

/// Агент читал файл на прежнем runner; запись по этой версии на новом runner, где файл другой, — отказ.
#[test]
fn write_based_on_a_file_of_the_old_runner_is_canceled_when_the_new_one_has_another() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        fx.runner.external_write("a", b"two");

        fx.vfs.switch_runner(&"runner-new".parse().unwrap());

        let entry = fx.walk("a").await.unwrap();
        let opened = fx.vfs.open(entry.ino, OFlags::WRONLY | OFlags::TRUNC).await;
        assert_eq!(opened, Err(Errno::CANCELED), "запись по версии прежнего runner прошла на новом");
        assert_eq!(fx.runner.files()["a"], b"two", "файл нового runner изменён при отказе");
    });
}

#[test]
fn first_open_after_a_new_epoch_checks_the_hash_and_downloads_only_a_changed_file() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("a", b"one");
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        fx.runner.clear_log();
        // Edit открывает файл до шести раз: в пределах эпохи проверок нет.
        for _ in 0..6 {
            assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        }
        assert_eq!(
            (fx.runner.count("Hash"), fx.runner.count("Read")),
            (0, 0),
            "повторные open ходят на runner"
        );

        fx.epoch.advance();
        assert_eq!(fx.read_file("a").await.unwrap(), b"one");
        assert_eq!(
            (fx.runner.count("Hash"), fx.runner.count("Read")),
            (1, 0),
            "файл не менялся: нужен только хеш"
        );

        fx.epoch.advance();
        fx.runner.external_write("a", b"two");
        assert_eq!(fx.read_file("a").await.unwrap(), b"two");
        assert_eq!(
            (fx.runner.count("Hash"), fx.runner.count("Read")),
            (2, 1),
            "файл изменился: нужно скачать"
        );
    });
}

#[test]
fn files_and_directories_created_through_the_filesystem_belong_to_the_caller() {
    for profile in [Profile::Rename, Profile::Flush] {
        run(async {
            let fx = Fixture::new(profile);
            let root = fx.walk("").await.unwrap();
            let dir = fx.vfs.mkdir(root.ino, "d", CALLER).await.unwrap();
            assert_eq!(dir.owner, CALLER, "каталог принадлежит не вызывающему");
            let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
            let (file, fh) = fx.vfs.create(root.ino, "f", 0o640, 0o022, flags, CALLER).await.unwrap();
            assert_eq!(
                (file.owner, file.attr.mode),
                (CALLER, 0o640 & !0o022),
                "владелец или права нового файла ({profile:?})"
            );
            fx.vfs.release(fh).await;
            // Файл runner принадлежит пользователю по умолчанию.
            fx.runner.put_file("old", b"x");
            let old = fx.walk("old").await.unwrap();
            assert_eq!(old.owner, Owner { uid: 1000, gid: 1000 });
        });
    }
}

#[test]
fn temp_file_lives_only_in_the_session_until_rename() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        let root = fx.walk("").await.unwrap();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
        let (temp, fh) = fx
            .vfs
            .create(root.ino, "a.tmp.1.0123456789ab", 0o644, 0, flags, CALLER)
            .await
            .unwrap();
        fx.vfs.write(fh, 0, b"draft").await.unwrap();
        fx.vfs.flush(fh).await.unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(
            fx.list_dir("").await.unwrap(),
            ["a.tmp.1.0123456789ab"],
            "временного файла нет в каталоге"
        );
        assert_eq!(fx.read_file("a.tmp.1.0123456789ab").await.unwrap(), b"draft");
        assert_eq!(fx.vfs.getattr(temp.ino, None).await.unwrap().attr.size, 5);
        fx.vfs.unlink(root.ino, "a.tmp.1.0123456789ab").await.unwrap();
        assert!(fx.runner.files().is_empty());
        assert_eq!(
            fx.runner.count("Write") + fx.runner.count("Remove"),
            0,
            "временный файл дошёл до runner: {:?}",
            fx.runner.log()
        );
    });
}

/// Временный файл Claude Code — только имя `<имя>.tmp.<pid>.<12 hex>`, в любом каталоге: он ждёт `rename` в сессии.
/// Любое другое имя с `.tmp.` — обычный файл, и запись в него без `rename` доходит до runner на `flush`: принятый за
/// временный, он пропал бы вместе с контейнером. Каждое обычное имя нарушает ровно одно условие шаблона.
#[test]
fn only_the_claude_temp_name_stays_in_the_session_and_other_tmp_names_reach_the_runner() {
    let cases = [
        ("a.txt.tmp.4242.0123456789ab", true),
        (".claude/.cc-writes/settings.json.tmp.7.abcdef012345", true),
        ("a.tmp.yaml", false),
        ("x.tmp.123.abc", false),
        ("x.tmp.123.0123456789abc", false),
        ("x.tmp.123.0123456789AB", false),
        ("x.tmp.123.0123456789ag", false),
        ("x.tmp.12a.0123456789ab", false),
        ("x.tmp..0123456789ab", false),
        ("x.tmq.123.0123456789ab", false),
        ("tmp.123.0123456789ab", false),
    ];
    for (path, temp) in cases {
        run(async {
            let fx = Fixture::new(Profile::Rename);
            fx.runner.put_dir(".claude");
            fx.runner.put_dir(".claude/.cc-writes");
            let parent = fx.walk(parent_of(path)).await.unwrap();
            let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
            let (_, fh) = fx.vfs.create(parent.ino, name_of(path), 0o644, 0, flags, CALLER).await.unwrap();
            fx.vfs.write(fh, 0, b"body").await.unwrap();
            fx.vfs.flush(fh).await.unwrap();
            fx.vfs.release(fh).await;
            let on_runner = fx.runner.files().get(path).cloned();
            if temp {
                assert_eq!(on_runner, None, "временный файл {path} ушёл на runner без rename");
            } else {
                assert_eq!(on_runner.as_deref(), Some(&b"body"[..]), "обычный файл {path} не дошёл до runner");
            }
        });
    }
}

/// Профиль без шаблона временных файлов: даже имя, похожее на временное файл Claude Code, — обычный файл, и запись
/// в него доходит до runner на `flush`, а не остаётся в сессии.
#[test]
fn without_a_temp_name_template_every_file_reaches_the_runner() {
    run(async {
        let fx = Fixture::without_temp_name();
        let root = fx.walk("").await.unwrap();
        let name = "a.txt.tmp.4242.0123456789ab";
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
        let (_, fh) = fx.vfs.create(root.ino, name, 0o644, 0, flags, CALLER).await.unwrap();
        fx.vfs.write(fh, 0, b"body").await.unwrap();
        fx.vfs.flush(fh).await.unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(
            fx.runner.files().get(name).map(Vec::as_slice),
            Some(&b"body"[..]),
            "без шаблона временный файл остался в сессии"
        );
    });
}

/// Гонка внутри самого переноса: изменение приходит после проверок сессии, но до `Rename` на runner.
#[test]
fn change_racing_with_the_rename_request_is_a_conflict_not_a_lost_update() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("a", b"A");
        fx.runner.put_file("b", b"B");
        assert_eq!(fx.read_file("a").await.unwrap(), b"A");
        assert_eq!(fx.read_file("b").await.unwrap(), b"B");
        fx.runner.change_before("Rename", "b", b"RACE");
        assert_eq!(fx.rename("a", "b").await, Err(Errno::CANCELED));
        assert_eq!(fx.runner.files()["b"], b"RACE");
        assert_eq!(
            fx.read_file("b").await.unwrap(),
            b"RACE",
            "после конфликта прочитана устаревшая копия"
        );
    });
}

#[test]
fn renaming_a_directory_or_onto_one_is_refused_without_reaching_the_runner() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_dir("d");
        fx.runner.put_dir("e");
        fx.runner.put_file("f", b"F");
        assert_eq!(fx.rename("d", "d2").await, Err(Errno::NOTSUP), "каталог переименован");
        assert_eq!(fx.rename("f", "e").await, Err(Errno::ISDIR), "файл занял место каталога");
        assert_eq!(fx.rename("missing", "g").await, Err(Errno::NOENT));
        assert_eq!(fx.runner.count("Rename"), 0, "отказ дошёл до runner: {:?}", fx.runner.log());
        assert_eq!(fx.runner.dirs().len(), 2);
    });
}

/// Временный файл сессии поверх цели остаётся записью, а не переносом: на runner уходит содержимое.
#[test]
fn renaming_a_temp_file_over_a_runner_file_is_still_a_write() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("a", b"old");
        fx.read_file("a").await.unwrap();
        fx.save_by_rename("a", b"new").await.unwrap();
        assert_eq!(fx.runner.files()["a"], b"new");
        assert_eq!((fx.runner.count("Write"), fx.runner.count("Rename")), (1, 0));
    });
}

fn change(path: &str, kind: ChangeKind) -> Change {
    Change {
        path: path.to_owned(),
        kind,
    }
}

#[test]
fn removed_directory_event_drops_the_whole_subtree() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_dir("d");
        fx.runner.put_file("d/f", b"x");
        let dir = fx.walk("d").await.unwrap();
        assert!(fx.vfs.lookup(dir.ino, "f").await.is_ok());
        fx.runner.external_remove("d/f");
        fx.runner.external_remove_dir("d");
        fx.vfs.apply_watch(&WatchBatch {
            root: Root::Project,
            changes: vec![change("d", ChangeKind::Removed)],
        });
        // Ядро могло держать каталог и спросить файл в нём напрямую, мимо проверки самого каталога.
        assert_eq!(
            fx.vfs.lookup(dir.ino, "f").await.map(|_| ()),
            Err(Errno::NOENT),
            "запись под удалённым каталогом осталась"
        );
        assert_eq!(fx.walk("d").await.map(|_| ()), Err(Errno::NOENT));
    });
}

/// Файл больше полумебибайта идёт кадрами канала данных и в ту, и в другую сторону.
#[test]
fn files_larger_than_half_a_megabyte_are_read_and_written_intact() {
    let big: Vec<u8> = (0..600 * 1024u32).map(|i| (i % 251) as u8).collect();
    for profile in [Profile::Rename, Profile::Flush] {
        run(async {
            let fx = Fixture::new(profile);
            fx.runner.put_file("big", &big);
            assert_eq!(fx.read_file("big").await.unwrap(), big, "большой файл на чтении ({profile:?})");
            fx.save(profile, "new", &big).await.unwrap();
            assert_eq!(
                fx.runner.files().get("new"),
                Some(&big),
                "большое содержимое дошло до runner не целиком ({profile:?})"
            );
        });
    }
}

/// Запись за пределом `/work` отклоняется `EFBIG` до выделения памяти под неё: в сессии файлу негде лежать.
#[test]
fn write_beyond_the_work_directory_is_too_big() {
    for profile in [Profile::Rename, Profile::Flush] {
        run(async {
            let fx = Fixture::new(profile);
            let root = fx.walk("").await.unwrap();
            let name = if profile == Profile::Rename { "f.tmp.1.0123456789ab" } else { "f" };
            let (_, fh) = fx
                .vfs
                .create(root.ino, name, 0o644, 0, OFlags::WRONLY | OFlags::CREATE, CALLER)
                .await
                .unwrap();
            let edge = fx.vfs.write(fh, TEST_MAX_FILE as u64 - 1, b"x").await;
            assert_eq!(edge.map(|_| ()), Ok(()), "файл ровно по пределу отклонён ({profile:?})");
            let beyond = fx.vfs.write(fh, TEST_MAX_FILE as u64, b"x").await;
            assert_eq!(beyond.map(|_| ()), Err(Errno::FBIG), "запись за пределом /work ({profile:?})");
        });
    }
}

/// Файл runner больше `/work` не читается: `EIO`, и в кэш он не попадает.
#[test]
fn file_larger_than_the_work_directory_is_not_read() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("huge", &vec![7; TEST_MAX_FILE + 1]);
        assert_eq!(fx.read_file("huge").await.map(|_| ()), Err(Errno::IO), "файл больше /work прочитан");
        assert_eq!(fx.cached_files(), 0, "файл больше /work попал в кэш");
    });
    // Ровно по пределу файл читается.
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("edge", &vec![7; TEST_MAX_FILE]);
        assert_eq!(
            fx.read_file("edge").await.map(|bytes| bytes.len()),
            Ok(TEST_MAX_FILE),
            "файл ровно по пределу не прочитан"
        );
    });
}

/// ФС поверх канала `wire`; кэш во временном каталоге.
fn vfs_on(wire: &Wire, cache: &TempDir) -> Vfs {
    let config = test_config(cache.path().join("work"), Profile::Rename, Duration::from_secs(30));
    Vfs::new(wire.link.clone(), config, Epoch::new()).unwrap()
}

/// Ошибка связи с runner — любая — для агента остаётся `EIO`: не конфликт (его принимают за «перечитай и повтори»), не
/// отказ в доступе и не `EINTR`, на который Claude Code повторяет операцию бесконечно.
#[test]
fn every_bad_reply_from_the_runner_is_an_io_error() {
    run(async {
        let cache = tempfile::tempdir().unwrap();
        let broken = Wire::answering(|_| None);
        broken.switch.borrow().as_ref().unwrap().close();
        let wires = [
            ("другая версия протокола у runner", Wire::foreign_version()),
            ("мусор вместо ответа", Wire::answering(|_| Some(vec![0xFF; 16]))),
            ("канал оборван", broken),
            ("runner не подключён", Wire::disconnected()),
        ];
        for (name, wire) in wires {
            let vfs = vfs_on(&wire, &cache);
            let error = vfs.lookup(ROOT, "a").await.map(|_| ()).unwrap_err();
            assert_eq!(error, Errno::IO, "{name}");
            assert_write_errno_allowed(error);
        }
    });
}

#[test]
fn silent_runner_is_an_io_error_when_the_request_times_out() {
    run(async {
        let cache = tempfile::tempdir().unwrap();
        // Канал есть, ответа нет.
        let wire = Wire::answering(|_| None);
        let vfs = vfs_on(&wire, &cache);
        // Остановленное время идёт вперёд само, когда делать нечего, и срок запроса проходит сразу.
        tokio::time::pause();
        let lookup = tokio::time::timeout(Duration::from_secs(60), vfs.lookup(ROOT, "a")).await;
        let error = lookup
            .expect("запрос к молчащему runner не ограничен таймаутом")
            .map(|_| ())
            .unwrap_err();
        assert_eq!(error, Errno::IO);
        assert_write_errno_allowed(error);
    });
}

/// Файл, который агент создал сразу под своим именем, а не временным, не пропадает: он уходит на runner на `flush`.
#[test]
fn file_created_under_its_own_name_reaches_the_runner_even_in_the_rename_profile() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        let root = fx.walk("").await.unwrap();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC;
        let (_, fh) = fx.vfs.create(root.ino, "plain.txt", 0o644, 0, flags, CALLER).await.unwrap();
        fx.vfs.write(fh, 0, b"direct").await.unwrap();
        fx.vfs.flush(fh).await.unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(
            fx.runner.files().get("plain.txt").map(Vec::as_slice),
            Some(&b"direct"[..]),
            "файл остался только в сессии"
        );
    });
}

/// Содержимое, которое не совпало с хешем в ответе runner, — повреждённая передача: агент получает ошибку, а не файл.
#[test]
fn content_that_does_not_match_the_hash_from_the_runner_is_an_io_error() {
    run(async {
        // Этот runner на любой запрос отвечает чтением, чей хеш не равен хешу присланного содержимого.
        let wire = Wire::answering(|_| {
            let response: FsResponse = Ok(FsReply::Read {
                attr: Attr {
                    kind: Kind::File,
                    size: 3,
                    mtime_ns: 1,
                    mode: 0o644,
                },
                sha256: [0; 32],
                content: Content(b"abc".to_vec()),
            });
            Some(protocol::encode(&response).unwrap())
        });
        let cache = tempfile::tempdir().unwrap();
        let config = test_config(cache.path().join("work"), Profile::Rename, Duration::from_secs(5));
        let vfs = Vfs::new(wire.link.clone(), config, Epoch::new()).unwrap();
        let result = vfs.open(ROOT, OFlags::RDONLY).await;
        assert_eq!(result, Err(Errno::IO), "повреждённое содержимое принято за файл");
    });
}

#[test]
fn cache_directory_and_files_are_private_to_the_session() {
    use std::os::unix::fs::PermissionsExt as _;
    run(async {
        let fx = Fixture::new(Profile::Rename);
        // Каталог, смонтированный tmpfs с правами по умолчанию, открыт всем: сессия закрывает его сама.
        let shared = fx.cache_dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let config = test_config(shared.clone(), Profile::Rename, Duration::from_secs(5));
        Vfs::new(fx.wire.link.clone(), config, Epoch::new()).unwrap();
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
            0o700,
            "готовый каталог кэша остался открытым"
        );
        fx.runner.put_file("a", b"secret");
        fx.read_file("a").await.unwrap();
        let work = fx.cache_dir.path().join("work");
        let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&work), 0o700, "каталог кэша доступен не только сессии");
        for entry in std::fs::read_dir(&work).unwrap() {
            let path = entry.unwrap().path();
            assert_eq!(mode(&path), 0o600, "кэш-файл {} доступен не только сессии", path.display());
        }
    });
}

/// Открытие с обрезкой опустошает файл на runner, даже если агент ничего не записал.
#[test]
fn open_with_truncate_empties_the_file_even_without_a_write() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"content");
        let entry = fx.walk("a").await.unwrap();
        let fh = fx.vfs.open(entry.ino, OFlags::WRONLY | OFlags::TRUNC).await.unwrap();
        fx.vfs.flush(fh).await.unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(fx.runner.files()["a"], b"", "обрезка не дошла до runner");
    });
}

#[test]
fn rename_with_flags_is_not_supported() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        let root = fx.walk("").await.unwrap();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
        let (_, fh) = fx
            .vfs
            .create(root.ino, "a.tmp.1.0123456789ab", 0o644, 0, flags, CALLER)
            .await
            .unwrap();
        fx.vfs.release(fh).await;
        // RENAME_EXCHANGE: обмен местами этой ФС не умеет, и тихо переименовать нельзя.
        assert_eq!(
            fx.vfs.rename(root.ino, "a.tmp.1.0123456789ab", root.ino, "a", 2).await,
            Err(Errno::NOTSUP)
        );
        assert!(fx.runner.files().is_empty());
    });
}

#[test]
fn rename_of_a_temp_file_over_a_directory_is_refused() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_dir("d");
        let root = fx.walk("").await.unwrap();
        fx.walk("d").await.unwrap();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
        let (_, fh) = fx
            .vfs
            .create(root.ino, "d.tmp.1.0123456789ab", 0o644, 0, flags, CALLER)
            .await
            .unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(
            fx.vfs.rename(root.ino, "d.tmp.1.0123456789ab", root.ino, "d", 0).await,
            Err(Errno::ISDIR)
        );
        assert_eq!(fx.runner.dirs().len(), 1, "каталог заменён файлом");
    });
}

/// Запись на месте без обрезки: правки поверх прежнего содержимого уходят на runner на `flush`.
#[test]
fn write_without_truncate_patches_the_file_and_is_sent_on_flush() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        let entry = fx.walk("a").await.unwrap();
        let fh = fx.vfs.open(entry.ino, OFlags::RDWR).await.unwrap();
        fx.vfs.write(fh, 1, b"X").await.unwrap();
        assert_eq!(
            fx.vfs.read(fh, 0, 10).await.unwrap(),
            b"oXe",
            "запись не видна открывшему файл до flush"
        );
        assert_eq!(fx.runner.files()["a"], b"one", "правка ушла на runner раньше flush");
        fx.vfs.flush(fh).await.unwrap();
        fx.vfs.release(fh).await;
        assert_eq!(fx.runner.files()["a"], b"oXe");
    });
}

/// Между сверкой версии и скачиванием содержимого файл изменился: правка рассчитана на устаревшее, а не на скачанное.
#[test]
fn file_changed_between_the_version_check_and_the_download_cancels_the_open_for_write() {
    run(async {
        let fx = Fixture::new(Profile::Flush);
        fx.runner.put_file("a", b"one");
        let entry = fx.walk("a").await.unwrap();
        fx.runner.change_before("Read", "a", b"two");
        let opened = fx.vfs.open(entry.ino, OFlags::RDWR).await;
        assert_eq!(opened, Err(Errno::CANCELED), "open на запись принял содержимое другой версии");
        assert_eq!(fx.runner.files()["a"], b"two");
    });
}

// ---------------------------------------------------------------- запись, ответ на которую потерян

/// Сохраняет `new` поверх прочитанного `old` в обоих профилях, обрывая канал на записи, и проверяет исход по файлу
/// на runner. `before` готовит гонку до сохранения; `writes` — сколько записей дошло до runner, `None` — не важно.
#[track_caller]
fn lost_write(loss: Loss, before: impl Fn(&ModelRunner), expected: FsResult<()>, file_after: &[u8], writes: Option<usize>, why: &str) {
    for profile in [Profile::Rename, Profile::Flush] {
        run(async {
            let fx = Fixture::reconnecting(profile);
            fx.runner.put_file("a", b"old");
            assert_eq!(fx.read_file("a").await.unwrap(), b"old");
            fx.runner.break_on("Write", loss);
            before(&fx.runner);
            fx.runner.clear_log();
            assert_eq!(fx.save(profile, "a", b"new").await, expected, "{why} ({profile:?}): исход записи");
            assert_eq!(fx.runner.files()["a"], file_after, "{why} ({profile:?}): файл на runner");
            if let Some(writes) = writes {
                assert_eq!(
                    fx.runner.count("Write"),
                    writes,
                    "{why} ({profile:?}): сколько записей дошло до runner"
                );
            }
        });
    }
}

/// runner записал файл, но ответ потерян: хеш файла — новое содержимое, запись засчитана и не повторена.
#[test]
fn write_applied_before_its_reply_was_lost_counts_as_written_without_a_repeat() {
    lost_write(Loss::AfterApply, |_| {}, Ok(()), b"new", Some(1), "выполненная запись");
}

/// Запись до runner не дошла: хеш файла — ожидаемое старое содержимое, запись повторяется один раз.
#[test]
fn write_lost_before_it_reached_the_runner_is_repeated_once() {
    lost_write(Loss::BeforeApply, |_| {}, Ok(()), b"new", Some(1), "не дошедшая запись");
}

/// За время обрыва файл сменил кто-то другой: ни новое, ни ожидаемое старое содержимое — конфликт, чужая версия цела.
#[test]
fn file_changed_by_someone_else_while_the_reply_was_lost_is_a_conflict() {
    let other = |runner: &ModelRunner| runner.change_before("Hash", "a", b"other");
    // Профиль Flush после отказа ещё раз пробует записать правку при закрытии файла: число записей не показательно.
    lost_write(Loss::BeforeApply, other, Err(Errno::CANCELED), b"other", None, "чужая правка");
}

/// Повтор упёрся в `Conflict`, потому что файл между сверкой и повтором сменил кто-то другой: успехом это не
/// считается.
#[test]
fn conflict_on_the_repeated_write_is_not_taken_for_success() {
    let other = |runner: &ModelRunner| runner.change_before("Write", "a", b"other");
    lost_write(
        Loss::BeforeApply,
        other,
        Err(Errno::CANCELED),
        b"other",
        // Профиль Flush после отказа ещё раз пробует записать правку при закрытии файла: число записей не показательно.
        None,
        "чужая правка перед повтором",
    );
}

/// Повтор упёрся в `Conflict`, а на runner уже ровно новое содержимое: сверка хеша после отказа засчитывает запись.
#[test]
fn conflict_on_the_repeated_write_is_settled_by_the_hash_again() {
    let same = |runner: &ModelRunner| runner.change_before("Write", "a", b"new");
    lost_write(Loss::BeforeApply, same, Ok(()), b"new", Some(1), "та же правка перед повтором");
}

/// Ответ на запись нового файла потерян до runner: файла всё ещё нет, и запись повторяется с ожиданием «не
/// существует».
#[test]
fn lost_write_of_a_new_file_is_repeated_while_the_file_is_still_absent() {
    run(async {
        let fx = Fixture::reconnecting(Profile::Flush);
        fx.runner.break_on("Write", Loss::BeforeApply);
        fx.save(Profile::Flush, "fresh", b"new").await.unwrap();
        assert_eq!(fx.runner.files()["fresh"], b"new", "новый файл не записан повтором");
        assert_eq!(fx.runner.count("Write"), 1, "новый файл записан не ровно один раз");
    });
}

/// MCP-конфиг проекта, который FUSE прячет от Antigravity CLI: агент не находит его ни по имени, ни в списке
/// каталога и не может записать ни на месте, ни переименованием; соседний файл виден и пишется.
#[test]
fn hidden_project_mcp_config_is_absent_for_the_agent_and_cannot_be_written() {
    const HIDDEN: &str = ".agents/mcp_config.json";
    const PROJECT_CONFIG: &[u8] = br#"{"mcpServers":{"project":{"command":"sh"}}}"#;
    for profile in [Profile::Flush, Profile::Rename] {
        run(async {
            let fx = Fixture::hiding(profile, &[HIDDEN]);
            fx.runner.put_dir(".agents");
            fx.runner.put_file(HIDDEN, PROJECT_CONFIG);
            fx.runner.put_file(".agents/rules.md", b"rule");

            assert_eq!(
                fx.read_file(HIDDEN).await,
                Err(Errno::NOENT),
                "{profile:?}: спрятанный файл прочитан"
            );
            assert_eq!(
                fx.list_dir(".agents").await.unwrap(),
                ["rules.md"],
                "{profile:?}: спрятанный файл в списке каталога"
            );
            assert_eq!(
                fx.read_file(".agents/rules.md").await.unwrap(),
                b"rule",
                "{profile:?}: сосед не виден"
            );
            assert_eq!(
                fx.save(profile, HIDDEN, b"{}").await,
                Err(Errno::NOENT),
                "{profile:?}: запись спрятанного файла не отказана"
            );
            fx.save(profile, ".agents/rules.md", b"new rule").await.unwrap();

            let files = fx.runner.files();
            assert_eq!(files[HIDDEN], PROJECT_CONFIG, "{profile:?}: спрятанный файл на runner изменён");
            assert_eq!(files[".agents/rules.md"], b"new rule", "{profile:?}: запись соседа не дошла");
        });
    }
}

/// `create` существующего файла с `O_EXCL` и `create` спрятанного пути отвечают допустимым errno: на `EEXIST` и
/// `EACCES` Claude Code перезаписал бы цель на месте.
#[test]
fn create_of_an_existing_or_hidden_path_returns_no_forbidden_errno() {
    const HIDDEN: &str = ".agents/mcp_config.json";
    for profile in [Profile::Flush, Profile::Rename] {
        run(async {
            let fx = Fixture::hiding(profile, &[HIDDEN]);
            fx.runner.put_dir(".agents");
            fx.runner.put_file(HIDDEN, b"{}");
            fx.runner.put_file("exists", b"old");
            let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL;
            let root = fx.walk("").await.unwrap();
            let agents = fx.walk(".agents").await.unwrap();

            let existing = fx.vfs.create(root.ino, "exists", 0o644, 0o022, flags, CALLER).await;
            let hidden = fx.vfs.create(agents.ino, "mcp_config.json", 0o644, 0o022, flags, CALLER).await;
            for (what, result) in [("существующий файл", existing), ("спрятанный путь", hidden)] {
                let error = result.err().unwrap_or_else(|| panic!("{profile:?}: create ({what}) прошёл"));
                for forbidden in [Errno::PERM, Errno::BUSY, Errno::EXIST, Errno::XDEV, Errno::ACCESS, Errno::INTR] {
                    assert_ne!(error, forbidden, "{profile:?}: create ({what}) вернул запрещённый {forbidden:?}");
                }
            }
            assert_eq!(fx.runner.files()["exists"], b"old", "{profile:?}: файл runner изменён");
        });
    }
}

/// Runner отказывает в доступе (`Access`, `OutsideWorkdir`) на любое изменение: `write`, `rename`, `unlink`, `rmdir` и
/// `mkdir` отвечают не `EACCES` и не другим errno, на которые Claude Code перезаписывает цель на месте.
#[test]
fn runner_access_denial_on_write_paths_returns_no_forbidden_errno() {
    let file_attr = Attr {
        kind: Kind::File,
        size: 3,
        mtime_ns: 1,
        mode: 0o644,
    };
    let dir_attr = Attr {
        kind: Kind::Dir,
        size: 4096,
        mtime_ns: 1,
        mode: 0o755,
    };
    for denial in [Error::Access, Error::OutsideWorkdir] {
        for profile in [Profile::Flush, Profile::Rename] {
            run(async {
                let reply_denial = denial.clone();
                let wire = Wire::answering(move |body| {
                    let Ok(DataRequest::Fs { request, .. }) = protocol::decode_body::<DataRequest>(&body) else {
                        return None;
                    };
                    let response: FsResponse = match request {
                        FsRequest::Stat { path } if path.is_empty() => Ok(FsReply::Stat(dir_attr)),
                        FsRequest::Stat { path } if path == "f" => Ok(FsReply::Stat(file_attr)),
                        FsRequest::Hash { path } if path == "f" => Ok(FsReply::Hashed {
                            attr: file_attr,
                            sha256: sha(b"old"),
                        }),
                        FsRequest::Read { path } if path == "f" => Ok(FsReply::Read {
                            attr: file_attr,
                            sha256: sha(b"old"),
                            content: Content(b"old".to_vec()),
                        }),
                        FsRequest::Stat { .. } | FsRequest::Hash { .. } => Err(Error::NotFound),
                        _ => Err(reply_denial.clone()),
                    };
                    Some(protocol::encode(&response).unwrap())
                });
                let fx = Fixture::over(ModelRunner::default(), wire, profile, &[]);
                let root = fx.walk("").await.unwrap();
                let forbidden = [Errno::PERM, Errno::BUSY, Errno::EXIST, Errno::XDEV, Errno::ACCESS, Errno::INTR];
                let mut results = vec![
                    ("save существующего", fx.save(profile, "f", b"new").await),
                    ("save нового", fx.save(profile, "g", b"new").await),
                    ("rename", fx.rename("f", "g").await),
                    ("mkdir", fx.mkdir("dir").await),
                ];
                fx.walk("f").await.unwrap();
                results.push(("unlink", fx.vfs.unlink(root.ino, "f").await));
                results.push(("rmdir", fx.vfs.rmdir(root.ino, "f").await));
                for (what, result) in results {
                    let error = result.err().unwrap_or_else(|| panic!("{profile:?} {denial:?}: {what} прошёл"));
                    assert!(
                        !forbidden.contains(&error),
                        "{profile:?} {denial:?}: {what} вернул запрещённый {error:?}"
                    );
                }
            });
        }
    }
}

// ---------------------------------------------------------------- перенос и удаление, ответ на которые потерян

/// Переносит прочитанный `a` в прочитанный `b`, обрывая канал на `Rename`; `before` готовит гонку.
#[track_caller]
fn lost_rename(loss: Loss, before: impl Fn(&ModelRunner), expected: FsResult<()>, files_after: &[(&str, &[u8])], renames: Option<usize>) {
    run(async {
        let fx = Fixture::reconnecting(Profile::Rename);
        fx.runner.put_file("a", b"A");
        fx.runner.put_file("b", b"B");
        assert_eq!(fx.read_file("a").await.unwrap(), b"A");
        assert_eq!(fx.read_file("b").await.unwrap(), b"B");
        fx.runner.break_on("Rename", loss);
        before(&fx.runner);
        fx.runner.clear_log();
        assert_eq!(fx.rename("a", "b").await, expected, "исход переноса");
        let expected_files: BTreeMap<String, Vec<u8>> = files_after
            .iter()
            .map(|(path, content)| ((*path).to_owned(), content.to_vec()))
            .collect();
        assert_eq!(fx.runner.files(), expected_files, "файлы на runner");
        if let Some(renames) = renames {
            assert_eq!(fx.runner.count("Rename"), renames, "сколько переносов дошло до runner");
        }
    });
}

/// runner перенёс файл, но ответ потерян: источника нет, на цели его содержимое — успех без повтора.
#[test]
fn rename_applied_before_its_reply_was_lost_counts_as_done_without_a_repeat() {
    lost_rename(Loss::AfterApply, |_| {}, Ok(()), &[("b", b"A")], Some(1));
}

/// Перенос до runner не дошёл: всё как было, перенос повторяется один раз.
#[test]
fn rename_lost_before_it_reached_the_runner_is_repeated_once() {
    lost_rename(Loss::BeforeApply, |_| {}, Ok(()), &[("b", b"A")], Some(1));
}

/// За время обрыва источник сменил кто-то другой: перенос — конфликт, чужая версия цела.
#[test]
fn rename_source_changed_by_someone_else_while_the_reply_was_lost_is_a_conflict() {
    let other = |runner: &ModelRunner| runner.change_before("Hash", "a", b"other");
    lost_rename(
        Loss::BeforeApply,
        other,
        Err(Errno::CANCELED),
        &[("a", b"other"), ("b", b"B")],
        Some(0),
    );
}

/// Удаляет прочитанный `a`, обрывая канал на `Remove`.
#[track_caller]
fn lost_remove(loss: Loss, before: impl Fn(&ModelRunner), expected: FsResult<()>, files_after: &[(&str, &[u8])], removes: Option<usize>) {
    run(async {
        let fx = Fixture::reconnecting(Profile::Rename);
        fx.runner.put_file("a", b"A");
        assert_eq!(fx.read_file("a").await.unwrap(), b"A");
        fx.runner.break_on("Remove", loss);
        before(&fx.runner);
        fx.runner.clear_log();
        let root = fx.walk("").await.unwrap();
        assert_eq!(fx.vfs.unlink(root.ino, "a").await, expected, "исход удаления");
        let expected_files: BTreeMap<String, Vec<u8>> = files_after
            .iter()
            .map(|(path, content)| ((*path).to_owned(), content.to_vec()))
            .collect();
        assert_eq!(fx.runner.files(), expected_files, "файлы на runner");
        if let Some(removes) = removes {
            assert_eq!(fx.runner.count("Remove"), removes, "сколько удалений дошло до runner");
        }
    });
}

/// runner удалил файл, но ответ потерян: файла нет — успех, а не `EIO`, и повтора нет.
#[test]
fn remove_applied_before_its_reply_was_lost_counts_as_done_without_a_repeat() {
    lost_remove(Loss::AfterApply, |_| {}, Ok(()), &[], Some(1));
}

/// Удаление до runner не дошло: файл прежний, удаление повторяется один раз.
#[test]
fn remove_lost_before_it_reached_the_runner_is_repeated_once() {
    lost_remove(Loss::BeforeApply, |_| {}, Ok(()), &[], Some(1));
}

/// За время обрыва файл сменил кто-то другой: чужое содержимое не удаляется, ответ — конфликт.
#[test]
fn file_changed_by_someone_else_while_the_remove_reply_was_lost_is_a_conflict() {
    let other = |runner: &ModelRunner| runner.change_before("Hash", "a", b"other");
    lost_remove(Loss::BeforeApply, other, Err(Errno::CANCELED), &[("a", b"other")], Some(0));
}
/// Корень рабочего каталога в `test_config`.
const ROOT_DIR: &str = "/home/agent/proj";

#[test]
fn preloaded_file_is_opened_without_a_download_and_is_checked_like_any_other() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_dir("d");
        fx.runner.put_file("d/b", b"one");
        fx.vfs.preload(std::path::Path::new(&format!("{ROOT_DIR}/d/b"))).await;
        assert_eq!(fx.runner.count("Read"), 1, "предзагрузка не скачала файл");
        fx.runner.clear_log();
        assert_eq!(fx.read_file("d/b").await.unwrap(), b"one");
        assert_eq!(fx.runner.count("Read"), 0, "открытие скачало файл второй раз");

        // Предзагрузка не отменяет сверку: в новой эпохе изменённый на runner файл скачивается заново.
        fx.epoch.advance();
        fx.runner.external_write("d/b", b"two");
        assert_eq!(fx.read_file("d/b").await.unwrap(), b"two");
    });
}

#[test]
fn preload_ignores_paths_outside_the_working_directory() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("a", b"one");
        for path in [
            "/home/agent/other/a".to_owned(),
            format!("{ROOT_DIR}/../a"),
            format!("{ROOT_DIR}x/a"),
            ROOT_DIR.to_owned(),
            "a".to_owned(),
        ] {
            fx.vfs.preload(std::path::Path::new(&path)).await;
        }
        assert_eq!(fx.runner.log(), Vec::new(), "предзагрузка обратилась к runner за чужим путём");
    });
}

/// Ядро идёт по ссылке само: `readlink` отдаёт цель, и путь от каталога ссылки читается как обычный файл runner.
#[test]
fn file_behind_a_relative_link_is_read_by_following_the_target() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_dir("pkg");
        fx.runner.put_dir("pkg/bin");
        fx.runner.put_file("pkg/tool.js", b"tool");
        fx.runner.put_link("pkg/bin/tool", "../tool.js");
        let link = fx.walk("pkg/bin/tool").await.unwrap();
        assert_eq!(link.attr.kind, Kind::Symlink);
        let target = fx.vfs.readlink(link.ino).await.unwrap();
        assert_eq!(target, "../tool.js", "цель ссылки искажена");
        assert_eq!(fx.read_file("pkg/tool.js").await.unwrap(), b"tool", "файл за ссылкой не прочитан");
    });
}

/// Цель кэшируется на эпоху; заменённая ссылка (событие watch с новыми атрибутами) и новая эпоха заставляют
/// спросить runner снова.
#[test]
fn link_target_is_cached_for_the_epoch_and_dropped_when_the_link_changes() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_link("l", "a");
        let link = fx.walk("l").await.unwrap();
        assert_eq!(fx.vfs.readlink(link.ino).await.unwrap(), "a");
        assert_eq!(fx.vfs.readlink(link.ino).await.unwrap(), "a");
        assert_eq!(fx.runner.count("ReadLink"), 1, "цель в той же эпохе спрошена повторно");

        fx.runner.put_link("l", "bb");
        let attr = fx.runner.link_attr_of("l").unwrap();
        fx.vfs.apply_watch(&WatchBatch {
            root: Root::Project,
            changes: vec![change("l", ChangeKind::Updated(attr))],
        });
        assert_eq!(
            fx.vfs.readlink(link.ino).await.unwrap(),
            "bb",
            "заменённая ссылка отдала прежнюю цель"
        );

        fx.runner.put_link("l", "ccc");
        fx.epoch.advance();
        assert_eq!(fx.vfs.readlink(link.ino).await.unwrap(), "ccc", "новая эпоха отдала цель из кэша");
    });
}

#[test]
fn link_with_a_target_runner_refuses_is_eacces_and_a_regular_file_is_einval() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_link("out", "/etc/passwd");
        fx.runner.put_file("f", b"x");
        let out = fx.walk("out").await.unwrap();
        assert_eq!(fx.vfs.readlink(out.ino).await, Err(Errno::ACCESS), "цель вне проекта не отказана");
        let file = fx.walk("f").await.unwrap();
        assert_eq!(
            fx.vfs.readlink(file.ino).await,
            Err(Errno::INVAL),
            "обычный файл прочитан как ссылка"
        );
        assert_eq!(fx.runner.count("ReadLink"), 1, "запрос ушёл на runner для обычного файла");
    });
}

/// Пачка о домашнем каталоге не трогает кэш проекта: пути в ней — от другого корня.
#[test]
fn watch_batch_of_another_root_does_not_touch_the_project_cache() {
    run(async {
        let fx = Fixture::new(Profile::Rename);
        fx.runner.put_file("f", b"x");
        fx.walk("f").await.unwrap();
        fx.runner.external_remove("f");
        fx.vfs.apply_watch(&WatchBatch {
            root: Root::Home,
            changes: vec![change("f", ChangeKind::Removed)],
        });
        assert!(fx.walk("f").await.is_ok(), "пачка корня Home сбросила запись проекта");
    });
}
