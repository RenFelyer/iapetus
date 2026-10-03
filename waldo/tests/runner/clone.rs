//! Клоны на настоящих временных каталогах и репозиториях git.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command as StdCommand;

use proptest::prelude::{any, prop};
use proptest::test_runner::TestRunner;
use proptest_derive::Arbitrary;
use tempfile::TempDir;

use super::*;

fn author() -> Author {
    Author {
        name: "Ren Felyer".to_owned(),
        email: "renfelyer@gmail.com".to_owned(),
    }
}

/// Жизнь клона, к которому подключилась сессия: автор коммитов известен.
fn assigned() -> Lifetime {
    let lifetime = Lifetime::new();
    lifetime.start(&author());
    lifetime
}

/// Синхронный git для подготовки и проверки: окружение не зависит от настроек пользователя.
#[track_caller]
fn sh_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} упал: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[track_caller]
fn sh(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(sh_bytes(dir, args)).unwrap()
}

/// Запись дерева каталогов: что в ней важно сохранить.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>, u32),
    Dir(u32),
    Link(PathBuf),
}

/// Всё дерево под `root` без разрешения ссылок: содержимое, права и цели ссылок.
fn tree(root: &Path) -> BTreeMap<String, Entry> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Entry>) {
        for item in fs::read_dir(dir).unwrap() {
            let path = item.unwrap().path();
            let name = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let meta = fs::symlink_metadata(&path).unwrap();
            let mode = meta.permissions().mode() & 0o7777;
            if meta.file_type().is_symlink() {
                out.insert(name, Entry::Link(fs::read_link(&path).unwrap()));
            } else if meta.is_dir() {
                out.insert(name, Entry::Dir(mode));
                walk(root, &path, out);
            } else {
                out.insert(name, Entry::File(fs::read(&path).unwrap(), mode));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Обычные файлы дерева: «путь → байты».
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    tree(root)
        .into_iter()
        .filter_map(|(path, entry)| match entry {
            Entry::File(bytes, _) => Some((path, bytes)),
            Entry::Dir(_) | Entry::Link(_) => None,
        })
        .collect()
}

/// Файлы ветки: «путь → байты».
fn branch_files(repo: &Path, branch: &str) -> BTreeMap<String, Vec<u8>> {
    sh(repo, &["ls-tree", "-r", "--name-only", branch])
        .lines()
        .map(|path| (path.to_owned(), sh_bytes(repo, &["show", &format!("{branch}:{path}")])))
        .collect()
}

/// Рабочая копия числится у репозитория.
fn registered(repo: &Path, dir: &Path) -> bool {
    let wanted = format!("worktree {}", dir.display());
    sh(repo, &["worktree", "list", "--porcelain"]).lines().any(|line| line == wanted)
}

fn branch_exists_in(repo: &Path, branch: &str) -> bool {
    !sh(repo, &["branch", "--list", branch]).trim().is_empty()
}

/// Каталог для испытаний: рабочий каталог родителя `work`, репозиторий `proj` с зафиксированными файлами и
/// несохранёнными правками, обычный каталог `plain` с файлами.
struct World {
    _tmp: TempDir,
    root: PathBuf,
    work: PathBuf,
    repo: PathBuf,
    plain: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = crate::testing::workdir();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (work, repo, plain) = (root.join("work"), root.join("proj"), root.join("plain"));
        for dir in [&work, &repo, &plain] {
            fs::create_dir(dir).unwrap();
            fs::create_dir(dir.join("d")).unwrap();
        }
        fs::write(work.join("w.txt"), "w").unwrap();
        for dir in [&repo, &plain] {
            fs::write(dir.join("readme.txt"), "readme").unwrap();
            fs::write(dir.join("d/c.txt"), "c").unwrap();
        }
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-q", "-m", "initial"]);
        fs::write(repo.join("readme.txt"), "правка репозитория").unwrap();
        fs::write(repo.join("dirty.txt"), "не закоммичено").unwrap();
        Self {
            _tmp: tmp,
            root,
            work,
            repo,
            plain,
        }
    }

    /// Всё, что клон не должен менять в исходном репозитории.
    fn repo_snapshot(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}",
            sh(&self.repo, &["rev-parse", "HEAD"]),
            sh(&self.repo, &["for-each-ref", "refs/heads/main"]),
            sh(&self.repo, &["status", "--porcelain"]),
            fs::read_to_string(self.repo.join("readme.txt")).unwrap(),
            fs::read_to_string(self.repo.join("dirty.txt")).unwrap(),
        )
    }

    /// Сколько записей рядом с родителем, репозиторием и обычным каталогом: клоны в `git worktree` добавляют свои.
    fn entries(&self) -> usize {
        fs::read_dir(&self.root).unwrap().count()
    }

    async fn clone_repo(&self) -> Created {
        create(&self.work, path_str(&self.repo)).await.unwrap()
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[tokio::test]
async fn repository_clone_is_a_sibling_worktree_on_its_own_branch() {
    let world = World::new();
    let created = world.clone_repo().await;
    let branch = created.branch.clone().expect("у клона репозитория нет ветки");
    assert_eq!(created.parent, world.work, "родитель в ответе не тот");
    assert_eq!(created.source, world.repo, "указанный каталог в ответе не тот");
    assert_eq!(created.dir.parent(), world.repo.parent(), "клон лежит не рядом с репозиторием");
    assert!(
        !created.dir.starts_with(&world.work) && !world.work.starts_with(&created.dir),
        "каталог клона {} совпадает с родителем или вложен в него",
        created.dir.display()
    );
    assert_eq!(
        sh(&created.dir, &["branch", "--show-current"]).trim(),
        branch,
        "клон не на своей ветке"
    );
    assert_ne!(branch, "main", "ветка клона совпала с веткой репозитория");
    assert!(registered(&world.repo, &created.dir), "клон не числится рабочей копией репозитория");
    assert!(
        !created.dir.join("dirty.txt").exists(),
        "в клон попали несохранённые файлы репозитория"
    );
    assert_eq!(
        fs::read_to_string(created.dir.join("readme.txt")).unwrap(),
        "readme",
        "клон не от HEAD репозитория"
    );
}

#[tokio::test]
async fn clone_skips_a_name_whose_branch_is_left_over() {
    let world = World::new();
    let first = world.clone_repo().await;
    // Каталог клона удалили, ветка осталась: имя занято ею.
    fs::remove_dir_all(&first.dir).unwrap();
    sh(&world.repo, &["worktree", "prune"]);
    let second = world.clone_repo().await;
    assert_ne!(second.branch, first.branch, "новый клон занял ветку прежнего");
}

#[tokio::test]
async fn directory_without_a_repository_is_used_in_place_and_stays_untouched() {
    let world = World::new();
    // Состав, который легко испортить чисткой: скрытые файлы, пустой каталог, исполняемый файл, ссылка наружу.
    let target = world.plain.join("etc");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("hosts"), "127.0.0.1 localhost").unwrap();
    fs::write(target.join(".hidden"), "h").unwrap();
    fs::create_dir(target.join("empty")).unwrap();
    fs::write(target.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(target.join("run.sh"), PermissionsExt::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("/nonexistent", target.join("dangling")).unwrap();
    let before = tree(&target);
    let entries_before = world.entries();

    let created = create(&world.work, path_str(&target)).await.unwrap();
    assert_eq!(created.dir, target, "клон без git работает не в указанном каталоге");
    assert_eq!(created.branch, None, "у клона без git появилась ветка");

    // Все причины гибели: ни одна не должна менять каталог, с автором и без.
    for lifetime in [Lifetime::new(), assigned()] {
        assert!(finish(&created.dir, &lifetime).await.is_empty(), "клон без git сообщил о потерях");
        assert_eq!(tree(&target), before, "гибель клона без git изменила или удалила файлы каталога");
        assert_eq!(world.entries(), entries_before, "клон без git оставил записи рядом");
    }
}

#[tokio::test]
async fn subdirectory_of_a_repository_is_used_in_place_without_commits() {
    let world = World::new();
    let sub = world.repo.join("d");
    let before = (world.repo_snapshot(), tree(&world.repo));
    let created = create(&world.work, path_str(&sub)).await.unwrap();
    assert_eq!(
        (created.dir.as_path(), created.branch.as_deref()),
        (sub.as_path(), None),
        "подкаталог не принят как есть"
    );
    assert!(finish(&created.dir, &assigned()).await.is_empty());
    assert_eq!(
        (world.repo_snapshot(), tree(&world.repo)),
        before,
        "гибель клона в подкаталоге изменила репозиторий"
    );
    assert_eq!(sh(&world.repo, &["worktree", "list"]).lines().count(), 1, "появилась рабочая копия");
}

#[tokio::test]
async fn overlapping_or_invalid_directories_are_refused_without_leftovers() {
    let world = World::new();
    fs::create_dir(world.work.join("sub")).unwrap();
    fs::write(world.root.join("file.txt"), "f").unwrap();
    for (link, target) in [
        ("link_work", &world.work),
        ("link_child", &world.work.join("sub")),
        ("link_up", &world.root),
    ] {
        std::os::unix::fs::symlink(target, world.root.join(link)).unwrap();
    }
    let at = |relative: &str| world.root.join(relative).to_string_lossy().into_owned();
    let cases = [
        ("тот же каталог", at("work")),
        ("вложенный в родителя", at("work/sub")),
        ("объемлющий родителя", world.root.to_string_lossy().into_owned()),
        ("ссылка на родителя", at("link_work")),
        ("ссылка на вложенный", at("link_child")),
        ("ссылка на объемлющий", at("link_up")),
        ("родитель через ..", at("plain/../work")),
        ("объемлющий через ..", at("plain/..")),
        // Текущий каталог теста существует, но не абсолютный путь.
        ("относительный путь", ".".to_owned()),
        ("несуществующий", at("missing")),
        ("файл", at("file.txt")),
    ];
    let (entries, snapshot, work) = (world.entries(), world.repo_snapshot(), tree(&world.work));
    for (name, requested) in cases {
        assert!(
            create(&world.work, &requested).await.is_err(),
            "принят каталог: {name} ({requested})"
        );
        assert_eq!(world.entries(), entries, "после отказа ({name}) остались каталоги");
        assert_eq!(world.repo_snapshot(), snapshot, "отказ ({name}) изменил репозиторий");
        assert_eq!(tree(&world.work), work, "отказ ({name}) изменил каталог родителя");
    }
}

/// Родитель с настоящими каталогами личности; `exe` — вместо waldo.
fn parent_of(world: &World, exe: &Path) -> Parent {
    Parent {
        workdir: world.work.clone(),
        identity_dir: world.root.join("identity"),
        exe: exe.to_owned(),
        fingerprint: "parent-fp".parse().unwrap(),
        orbit_url: "orbit.example:2222".to_owned(),
        orbit_ca: PathBuf::from("/etc/ca.pub"),
        orbit_key: "secret-key".to_owned(),
        home: true,
    }
}

#[tokio::test]
async fn failed_launch_leaves_no_directory_no_branch_and_no_identity() {
    let world = World::new();
    let parent = parent_of(&world, Path::new("/nonexistent/waldo"));
    let (entries, plain) = (world.entries(), tree(&world.plain));
    for requested in [&world.repo, &world.plain] {
        assert!(
            start_clone(&parent, path_str(requested)).await.is_err(),
            "запуск несуществующего файла прошёл"
        );
    }
    // Новая запись рядом — каталог личности родителя; каталога клона нет.
    assert_eq!(world.entries(), entries + 1, "остался каталог клона");
    assert_eq!(
        fs::read_dir(world.root.join("identity/clones")).unwrap().count(),
        0,
        "личность неудавшегося клона осталась"
    );
    assert_eq!(
        sh(&world.repo, &["branch", "--list", "iapetus/*"]).trim(),
        "",
        "ветка клона осталась"
    );
    assert_eq!(
        sh(&world.repo, &["worktree", "list"]).lines().count(),
        1,
        "клон остался в списке рабочих копий"
    );
    assert_eq!(tree(&world.plain), plain, "отказ запуска изменил каталог клона без git");
}

#[tokio::test]
async fn clone_process_gets_the_parent_its_own_identity_and_the_key_only_from_the_identity_dir() {
    let world = World::new();
    // Вместо waldo — скрипт, который записывает свои аргументы, окружение и каталог.
    let exe = world.root.join("fake-waldo");
    let dump = world.root.join("dump");
    fs::write(
        &exe,
        format!(
            "#!/bin/sh\n{{ pwd; env; for a in \"$@\"; do echo \"arg:$a\"; done; }} > {d}.tmp && mv {d}.tmp {d}\n",
            d = dump.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let parent = parent_of(&world, &exe);

    let reply = start_clone(&parent, path_str(&world.repo)).await.unwrap();

    let clone_dir = world.repo.parent().unwrap().join("proj-clone-1");
    assert_eq!(reply.name, "proj-clone-1");
    let identity_dir = world.root.join("identity/clones/proj-clone-1-1");
    let identity = Identity::load_or_create(&identity_dir).unwrap();
    assert_eq!(&reply.fingerprint, identity.fingerprint(), "в ответе не fingerprint личности клона");
    assert_eq!(
        identity.orbit_key().unwrap().as_deref(),
        Some("secret-key"),
        "ключ не лежит в личности клона"
    );
    let mut dumped = String::new();
    for _ in 0..1000 {
        if let Ok(text) = fs::read_to_string(&dump) {
            dumped = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!dumped.is_empty(), "запущенный процесс клона ничего не записал");
    assert!(!dumped.contains("secret-key"), "ключ попал в аргументы или окружение клона");
    assert_eq!(dumped.lines().next(), clone_dir.to_str(), "клон запущен не в своём каталоге");
    for expected in [
        "arg:runner".to_owned(),
        format!("arg:{}", clone_dir.display()),
        "arg:--temporary".to_owned(),
        "arg:--parent".to_owned(),
        "arg:parent-fp".to_owned(),
        "arg:--home".to_owned(),
        "arg:on".to_owned(),
        format!("arg:{}", identity_dir.display()),
        format!("arg:{}", world.repo.join(".env").display()),
    ] {
        assert!(
            dumped.lines().any(|line| line == expected),
            "в аргументах клона нет {expected}:\n{dumped}"
        );
    }
}

#[test]
fn restarted_clone_keeps_the_author_of_its_session() {
    let tmp = crate::testing::workdir();
    let dir = tmp.path().join("identity");
    let first = Arc::new(Identity::load_or_create(&dir).unwrap());
    let lifetime = Lifetime::restored(Arc::clone(&first));
    assert_eq!(lifetime.author(), None, "у клона без сессии есть автор");
    lifetime.start(&author());
    drop((lifetime, first));

    let restarted = Lifetime::restored(Arc::new(Identity::load_or_create(&dir).unwrap()));

    assert_eq!(restarted.author(), Some(author()), "перезапущенный клон потерял автора");
}

#[tokio::test]
async fn autocommit_uses_the_session_author_and_marks_the_commit() {
    let world = World::new();
    let created = world.clone_repo().await;
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    assert!(autocommit(&created.dir, &author()).await.unwrap(), "коммит не создан");
    insta::assert_snapshot!(
        sh(&created.dir, &["log", "-1", "--format=%an <%ae> | %cn <%ce> | %s"]).trim(),
        @"Ren Felyer <renfelyer@gmail.com> | Ren Felyer <renfelyer@gmail.com> | autocommit: изменения клона при отключении сессии"
    );
}

#[tokio::test]
async fn failing_pre_commit_hook_does_not_block_autocommit() {
    let world = World::new();
    let created = world.clone_repo().await;
    let hook = world.repo.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, PermissionsExt::from_mode(0o755)).unwrap();
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    assert!(autocommit(&created.dir, &author()).await.unwrap(), "хук помешал автокоммиту");
}

#[tokio::test]
async fn commit_signing_setting_does_not_block_autocommit() {
    let world = World::new();
    let created = world.clone_repo().await;
    sh(&world.repo, &["config", "commit.gpgsign", "true"]);
    sh(&world.repo, &["config", "gpg.program", "false"]);
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    assert!(autocommit(&created.dir, &author()).await.unwrap(), "подпись помешала автокоммиту");
}

#[tokio::test]
async fn finish_commits_then_removes_the_worktree_and_keeps_the_branch() {
    let world = World::new();
    let created = world.clone_repo().await;
    let branch = created.branch.clone().unwrap();
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    fs::write(created.dir.join("readme.txt"), "правка клона").unwrap();
    fs::remove_file(created.dir.join("d/c.txt")).unwrap();
    assert!(
        finish(&created.dir, &assigned()).await.is_empty(),
        "потери при успешном автокоммите"
    );
    assert!(!created.dir.exists(), "рабочая копия клона осталась");
    assert!(!registered(&world.repo, &created.dir), "клон остался в списке рабочих копий");
    assert!(branch_exists_in(&world.repo, &branch), "ветка клона пропала");
    let expected = BTreeMap::from([
        ("new.txt".to_owned(), b"n".to_vec()),
        ("readme.txt".to_owned(), "правка клона".as_bytes().to_vec()),
    ]);
    assert_eq!(
        branch_files(&world.repo, &branch),
        expected,
        "ветка клона не содержит всех его изменений"
    );
}

/// Клон без единой сессии после гибели: рабочая копия числится у репозитория, ветка не сдвинулась, файлы и
/// несохранённые правки на месте.
#[track_caller]
fn assert_left_for_people(world: &World, created: &Created, files_before: &BTreeMap<String, Vec<u8>>) {
    let branch = created.branch.as_deref().unwrap();
    assert!(registered(&world.repo, &created.dir), "рабочая копия клона без сессии удалена");
    assert!(branch_exists_in(&world.repo, branch), "ветка клона без сессии пропала");
    assert_eq!(
        sh(&world.repo, &["rev-parse", branch]),
        sh(&world.repo, &["rev-parse", "main"]),
        "клон без сессии закоммитил: автора коммитов у него нет"
    );
    assert_eq!(&files(&created.dir), files_before, "файлы клона без сессии изменились");
}

#[tokio::test]
async fn clone_without_a_session_keeps_its_worktree_branch_and_uncommitted_changes() {
    let world = World::new();
    let clean = world.clone_repo().await;
    let clean_files = files(&clean.dir);
    assert!(
        finish(&clean.dir, &Lifetime::new()).await.is_empty(),
        "клон без сессии сообщил о потерях"
    );
    assert_left_for_people(&world, &clean, &clean_files);

    let dirty = world.clone_repo().await;
    fs::write(dirty.dir.join("new.txt"), "n").unwrap();
    fs::write(dirty.dir.join("readme.txt"), "правка").unwrap();
    let dirty_files = files(&dirty.dir);
    assert!(
        finish(&dirty.dir, &Lifetime::new()).await.is_empty(),
        "клон без сессии сообщил о потерях"
    );
    assert_left_for_people(&world, &dirty, &dirty_files);
    assert_eq!(
        sh(&dirty.dir, &["status", "--porcelain"]),
        " M readme.txt\n?? new.txt\n",
        "несохранённые правки клона без сессии не остались несохранёнными"
    );
}

#[tokio::test]
async fn failed_autocommit_still_removes_the_worktree_and_names_the_lost_paths() {
    let world = World::new();
    let created = world.clone_repo().await;
    let branch = created.branch.clone().unwrap();
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    // Занятый индекс: `git add` в клоне падает.
    let git_dir = PathBuf::from(sh(&created.dir, &["rev-parse", "--absolute-git-dir"]).trim());
    fs::write(git_dir.join("index.lock"), "").unwrap();
    let lost = finish(&created.dir, &assigned()).await;
    assert_eq!(lost, ["new.txt"], "потерянный файл не назван");
    assert!(!created.dir.exists(), "после сбоя автокоммита рабочая копия осталась");
    assert!(!registered(&world.repo, &created.dir), "клон остался в списке рабочих копий");
    assert!(branch_exists_in(&world.repo, &branch), "ветка клона пропала");
}

#[tokio::test]
async fn finish_leaves_the_main_repository_alone() {
    let world = World::new();
    // Ветка с префиксом клонов, но не дополнительная рабочая копия.
    sh(&world.repo, &["checkout", "-q", "-b", "iapetus/by-hand"]);
    let before = (world.repo_snapshot(), tree(&world.repo));
    assert!(finish(&world.repo, &assigned()).await.is_empty());
    assert_eq!((world.repo_snapshot(), tree(&world.repo)), before, "основной репозиторий тронут");
}

#[tokio::test]
async fn finish_leaves_a_worktree_on_a_foreign_branch_alone() {
    let world = World::new();
    let foreign = world.root.join("feature");
    sh(&world.repo, &["worktree", "add", "-q", "-b", "feature", path_str(&foreign)]);
    fs::write(foreign.join("mine.txt"), "m").unwrap();
    let head = sh(&foreign, &["rev-parse", "HEAD"]);
    assert!(finish(&foreign, &assigned()).await.is_empty());
    assert_eq!(
        fs::read_to_string(foreign.join("mine.txt")).unwrap(),
        "m",
        "чужая рабочая копия изменена"
    );
    assert_eq!(sh(&foreign, &["rev-parse", "HEAD"]), head, "в чужую ветку попал коммит");
    assert!(registered(&world.repo, &foreign), "чужая рабочая копия удалена");
}

#[tokio::test]
async fn finish_leaves_a_clone_switched_to_another_iapetus_branch_alone() {
    let world = World::new();
    let created = world.clone_repo().await;
    sh(&created.dir, &["checkout", "-q", "-b", "iapetus/other"]);
    fs::write(created.dir.join("mine.txt"), "m").unwrap();
    let head = sh(&created.dir, &["rev-parse", "HEAD"]);
    assert!(finish(&created.dir, &assigned()).await.is_empty());
    assert_eq!(
        fs::read_to_string(created.dir.join("mine.txt")).unwrap(),
        "m",
        "рабочая копия на чужой ветке iapetus/* изменена"
    );
    assert_eq!(
        sh(&created.dir, &["rev-parse", "HEAD"]),
        head,
        "в чужую ветку iapetus/* попал коммит"
    );
    assert!(
        registered(&world.repo, &created.dir),
        "рабочая копия на чужой ветке iapetus/* удалена"
    );
}

#[tokio::test]
async fn finish_in_a_subdirectory_of_a_clone_leaves_the_clone_alone() {
    let world = World::new();
    let created = world.clone_repo().await;
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    let head = sh(&created.dir, &["rev-parse", "HEAD"]);
    assert!(finish(&created.dir.join("d"), &assigned()).await.is_empty());
    assert!(created.dir.join("new.txt").exists(), "подкаталог клона принят за клон");
    assert_eq!(sh(&created.dir, &["rev-parse", "HEAD"]), head, "коммит из подкаталога");
}

/// Пути, которыми играют операции: два файла из первого коммита и два новых, один в глубоком каталоге.
const FILES: [&str; 4] = ["readme.txt", "d/c.txt", "d/e/f.txt", "g.txt"];

/// Операции над клоном: словарь для ручных сценариев и для `proptest`.
#[derive(Debug, Clone, Copy, Arbitrary)]
enum Op {
    /// Файл (номер из `FILES` по модулю) и его байт.
    Write(u8, u8),
    Delete(u8),
    /// Обрыв автокоммита после `git add -A`, до коммита; у клона без git ничего не делает.
    Stage,
    /// Автокоммит; у клона без git ничего не делает.
    Autocommit,
    /// Гибель клона: дальше операции игнорируются.
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Клон в `git worktree` репозитория.
    Worktree,
    /// Клон прямо в обычном каталоге.
    Plain,
}

/// Клон под операциями и модель его файлов «путь → байты».
struct Harness {
    world: World,
    kind: Kind,
    repo_before: String,
    entries_before: usize,
    created: Created,
    lifetime: Lifetime,
    model: BTreeMap<String, Vec<u8>>,
    /// После последнего автокоммита изменений не было.
    committed: bool,
    finished: bool,
}

impl Harness {
    async fn new(kind: Kind) -> Self {
        let world = World::new();
        let repo_before = world.repo_snapshot();
        let created = match kind {
            Kind::Worktree => world.clone_repo().await,
            Kind::Plain => create(&world.work, path_str(&world.plain)).await.unwrap(),
        };
        let entries_before = world.entries() - usize::from(kind == Kind::Worktree);
        let model = BTreeMap::from([("readme.txt".to_owned(), b"readme".to_vec()), ("d/c.txt".to_owned(), b"c".to_vec())]);
        Self {
            world,
            kind,
            repo_before,
            entries_before,
            created,
            lifetime: assigned(),
            model,
            committed: true,
            finished: false,
        }
    }

    async fn apply(&mut self, op: Op) {
        if self.finished {
            return;
        }
        match op {
            Op::Write(file, value) => {
                let name = FILES[usize::from(file % 4)];
                let path = self.created.dir.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, [value % 128]).unwrap();
                self.model.insert(name.to_owned(), vec![value % 128]);
                self.committed = false;
            }
            Op::Delete(file) => {
                let name = FILES[usize::from(file % 4)];
                if self.model.remove(name).is_some() {
                    fs::remove_file(self.created.dir.join(name)).unwrap();
                    self.committed = false;
                }
            }
            Op::Stage if self.kind == Kind::Worktree => {
                sh(&self.created.dir, &["add", "-A"]);
            }
            Op::Autocommit if self.kind == Kind::Worktree => {
                autocommit(&self.created.dir, &author()).await.unwrap();
                self.committed = true;
            }
            Op::Stage | Op::Autocommit => {}
            Op::Finish => {
                let lost = finish(&self.created.dir, &self.lifetime).await;
                assert!(lost.is_empty(), "гибель клона потеряла изменения: {lost:?}");
                self.finished = true;
                self.committed = true;
            }
        }
        self.verify_invariants();
    }

    #[track_caller]
    fn verify_invariants(&self) {
        let world = &self.world;
        assert_eq!(
            world.repo_snapshot(),
            self.repo_before,
            "клон изменил рабочую копию, HEAD или ветку исходного репозитория"
        );
        match (self.kind, self.finished) {
            (Kind::Plain, _) => {
                assert_eq!(
                    files(&self.created.dir),
                    self.model,
                    "файлы клона без git расходятся с тем, что в них записано"
                );
                assert!(
                    !self.created.dir.join(".git").exists(),
                    "в каталоге клона без git появился репозиторий"
                );
                assert_eq!(world.entries(), self.entries_before, "клон без git оставил записи рядом");
            }
            (Kind::Worktree, true) => {
                let branch = self.created.branch.as_deref().unwrap();
                assert!(!self.created.dir.exists(), "после гибели рабочая копия клона осталась");
                assert!(
                    !registered(&world.repo, &self.created.dir),
                    "после гибели клон числится рабочей копией"
                );
                assert_eq!(
                    branch_files(&world.repo, branch),
                    self.model,
                    "ветка клона после гибели не содержит всех его изменений"
                );
            }
            (Kind::Worktree, false) => {
                let branch = self.created.branch.as_deref().unwrap();
                assert_eq!(
                    sh(&self.created.dir, &["branch", "--show-current"]).trim(),
                    branch,
                    "клон ушёл со своей ветки"
                );
                if self.committed {
                    assert_eq!(
                        sh(&self.created.dir, &["status", "--porcelain"]),
                        "",
                        "после автокоммита остались несохранённые изменения"
                    );
                    assert_eq!(
                        branch_files(&self.created.dir, "HEAD"),
                        self.model,
                        "коммит клона не совпадает с его файлами"
                    );
                }
            }
        }
    }
}

async fn check_ops_async(kind: Kind, ops: &[Op]) {
    let mut harness = Harness::new(kind).await;
    for &op in ops {
        harness.apply(op).await;
    }
    harness.apply(Op::Finish).await;
}

fn check_ops(kind: Kind, ops: &[Op]) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check_ops_async(kind, ops));
}

#[test]
fn finish_keeps_new_changed_and_deleted_files() {
    let ops = [Op::Write(0, 7), Op::Write(2, 8), Op::Write(3, 9), Op::Delete(1)];
    check_ops(Kind::Worktree, &ops);
    check_ops(Kind::Plain, &ops);
}

#[test]
fn autocommit_after_an_interruption_at_any_step_keeps_all_changes() {
    let changes = [Op::Write(0, 1), Op::Write(2, 2), Op::Delete(1)];
    // Обрыв до `add`, между `add` и `commit`, после `commit`; изменения после обрыва тоже сохраняются.
    for interruption in [&[][..], &[Op::Stage], &[Op::Autocommit]] {
        let mut ops = changes.to_vec();
        ops.extend_from_slice(interruption);
        ops.extend([Op::Write(3, 3), Op::Delete(0)]);
        check_ops(Kind::Worktree, &ops);
    }
}

#[test]
fn random_operations_keep_every_clone_change_and_leave_the_source_alone() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        return;
    }
    TestRunner::default()
        .run(&prop::collection::vec(any::<Op>(), 0..12), |ops| {
            check_ops(Kind::Worktree, &ops);
            check_ops(Kind::Plain, &ops);
            Ok(())
        })
        .unwrap();
}

/// Гибель клона так, как её ведёт runner: дождаться остановки и закончить клон.
async fn die(created: &Created, lifetime: &Lifetime) -> Stop {
    let stop = lifetime.stopped().await;
    finish(&created.dir, lifetime).await;
    stop
}

#[tokio::test(start_paused = true)]
async fn clone_dies_when_its_session_ends_by_disconnect_or_switch() {
    // Отключение сессии и переключение на другой runner приходят одинаково: `End`.
    let world = World::new();
    let created = world.clone_repo().await;
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    let lifetime = assigned();
    lifetime.end();
    assert_eq!(die(&created, &lifetime).await, Stop::SessionEnded);
    assert!(!created.dir.exists(), "клон пережил конец сессии");
    assert_eq!(
        branch_files(&world.repo, created.branch.as_deref().unwrap()).get("new.txt"),
        Some(&b"n".to_vec()),
        "изменения клона не сохранены в ветке"
    );
}

#[tokio::test(start_paused = true)]
async fn clone_that_dies_idle_without_a_session_leaves_its_worktree_to_people() {
    let world = World::new();
    let created = world.clone_repo().await;
    fs::write(created.dir.join("new.txt"), "n").unwrap();
    let before = files(&created.dir);
    assert_eq!(die(&created, &Lifetime::new()).await, Stop::Idle);
    assert_left_for_people(&world, &created, &before);
}

#[tokio::test(start_paused = true)]
async fn assignment_that_arrives_with_the_deadline_wins() {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    // Срок и назначение готовы одновременно, а `select!` выбирает ветку случайно: повторяем, чтобы не полагаться на везение.
    for attempt in 0..32 {
        let lifetime = Lifetime::new();
        let mut stopped = std::pin::pin!(lifetime.stopped());
        assert!(stopped.as_mut().poll(&mut context).is_pending(), "остановка до срока");
        tokio::time::advance(IDLE_STOP).await;
        lifetime.start(&author());
        assert!(
            stopped.as_mut().poll(&mut context).is_pending(),
            "попытка {attempt}: назначение к сроку не отменило остановку"
        );
    }
}
