use super::*;

const STATE_ROOT: &str = "/home/agent";
const AGENT_UID: u32 = 1000;

fn host(project: &str, home: &str) -> RunnerHost {
    RunnerHost {
        project: project.to_owned(),
        hostname: "box".to_owned(),
        os_release: "ID=arch\n".to_owned(),
        home: home.to_owned(),
        home_origin: String::new(),
        home_served: false,
    }
}

fn resolved(project: &str, home: &str) -> Result<Environment, String> {
    resolve(Path::new(STATE_ROOT), AGENT_UID, &host(project, home))
}

#[test]
fn project_path_that_cannot_be_mounted_is_refused() {
    for project in [
        "work/project",
        "/srv/../etc",
        "/srv/./project",
        "/usr/lib/project",
        "/etc",
        "/opt/iapetus/x",
        "/run/secrets",
        "/tmp/project",
        "/var/lib/project",
        "/proc/1",
        "/home/agent/project",
        "/home/agent/.claude/projects",
        "/home",
        "/",
        "",
    ] {
        assert!(resolved(project, "").is_err(), "путь проекта {project:?} принят");
    }
}

#[test]
fn project_inside_the_runner_home_is_accepted_with_that_home() {
    let environment = resolved("/home/alice/project", "/home/alice").expect("обычный проект отвергнут");
    assert_eq!(environment.project, Path::new("/home/alice/project"));
    assert_eq!(environment.home, Path::new("/home/alice"), "HOME runner не передан агенту");
}

#[test]
fn home_that_the_agent_cannot_use_falls_back_to_the_state_root() {
    for home in [
        "",
        "relative",
        "/home/alice/project/home",
        "/etc",
        "/home",
        "/home/agent/x",
        "/srv/../root",
    ] {
        let environment = resolved("/home/alice/project", home).expect("проект отвергнут из-за HOME");
        assert_eq!(environment.home, Path::new(STATE_ROOT), "HOME {home:?} принят");
    }
}

#[test]
fn existing_home_closed_to_the_agent_falls_back_to_the_state_root() {
    // Каталог вне /tmp: он входит в системные пути контейнера.
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let closed = dir.path().join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = closed.to_str().unwrap();
    // Владелец каталога — не агент, а доступа остальным нет.
    let agent = std::fs::metadata(&closed).unwrap().uid() + 1;
    let environment = resolve(Path::new(STATE_ROOT), agent, &host("/srv/p", home)).unwrap();
    assert_eq!(environment.home, Path::new(STATE_ROOT), "закрытый каталог принят как HOME");

    let owner = std::fs::metadata(&closed).unwrap().uid();
    let environment = resolve(Path::new(STATE_ROOT), owner, &host("/srv/p", home)).unwrap();
    assert_eq!(environment.home, closed, "каталог агента отвергнут как HOME");
}

#[test]
fn os_release_replaces_a_symlink_with_the_runner_file() {
    let etc = tempfile::tempdir().unwrap();
    let image = etc.path().join("image-os-release");
    std::fs::write(&image, "ID=debian\n").unwrap();
    std::os::unix::fs::symlink(&image, etc.path().join("os-release")).unwrap();

    write_os_release(etc.path(), "ID=arch\n").unwrap();

    assert_eq!(std::fs::read_to_string(etc.path().join("os-release")).unwrap(), "ID=arch\n");
    assert_eq!(
        std::fs::read_to_string(&image).unwrap(),
        "ID=debian\n",
        "запись прошла по ссылке в файл образа"
    );
}

#[test]
fn empty_or_oversized_os_release_leaves_the_file_alone() {
    let etc = tempfile::tempdir().unwrap();
    std::fs::write(etc.path().join("os-release"), "ID=debian\n").unwrap();

    write_os_release(etc.path(), "").unwrap();
    write_os_release(etc.path(), &"x".repeat(MAX_OS_RELEASE + 1)).unwrap();

    assert_eq!(std::fs::read_to_string(etc.path().join("os-release")).unwrap(), "ID=debian\n");
}

#[test]
fn hosts_entry_is_added_once() {
    let etc = tempfile::tempdir().unwrap();
    std::fs::write(etc.path().join("hosts"), "127.0.0.1\tlocalhost").unwrap();

    add_hosts_entry(etc.path(), "box").unwrap();
    add_hosts_entry(etc.path(), "box").unwrap();

    assert_eq!(
        std::fs::read_to_string(etc.path().join("hosts")).unwrap(),
        "127.0.0.1\tlocalhost\n127.0.0.1\tbox\n"
    );
}

#[test]
fn hostname_with_line_breaks_or_spaces_is_not_valid() {
    assert!(valid_hostname("build-01.example_net"));
    for name in ["a\n127.0.0.1 evil", "a b", "a/b", &"x".repeat(MAX_HOSTNAME + 1)] {
        assert!(!valid_hostname(name), "имя {name:?} принято");
    }
}

fn serving(project: &str, home: &str, origin: &str) -> RunnerHost {
    RunnerHost {
        home_origin: origin.to_owned(),
        home_served: true,
        ..host(project, home)
    }
}

fn resolved_serving(project: &str, home: &str, origin: &str) -> Environment {
    resolve(Path::new(STATE_ROOT), AGENT_UID, &serving(project, home, origin)).expect("проект отвергнут")
}

/// Проект внутри `$HOME` лежит поверх его ФС: `$HOME` монтируется первым, а размонтирование идёт в обратном порядке.
#[test]
fn home_is_mounted_before_the_project_inside_it() {
    let environment = resolved_serving("/home/alice/project", "/home/alice", "");
    assert!(environment.home_fs, "HOME runner не стал ФС");
    let plan = environment.mount_plan();
    let order: Vec<_> = plan.iter().map(|point| (point.path.to_str().unwrap(), point.root)).collect();
    assert_eq!(
        order,
        [("/home/alice", Root::Home), ("/home/alice/project", Root::Project)],
        "$HOME смонтирован не раньше проекта"
    );
}

#[test]
fn project_outside_the_home_is_mounted_next_to_it_and_without_home_service_alone() {
    let environment = resolved_serving("/srv/project", "/home/alice", "");
    assert_eq!(environment.mount_plan().len(), 2, "проект вне $HOME и $HOME — две ФС");

    for (project, home, served) in [
        ("/srv/project", "/home/alice", false),
        ("/home/alice/project", "/home/alice/project", true),
        ("/srv/p", "/srv/p/home", true),
    ] {
        let host = RunnerHost {
            home_served: served,
            ..host(project, home)
        };
        let environment = resolve(Path::new(STATE_ROOT), AGENT_UID, &host).unwrap();
        let plan = environment.mount_plan();
        assert!(
            !environment.home_fs,
            "{project} {home} {served}: вторая ФС при $HOME без службы или внутри проекта"
        );
        assert_eq!(plan.len(), 1, "{project} {home} {served}: смонтировано не только проект");
        assert_eq!(plan[0].root, Root::Project);
    }
}

#[test]
fn environment_moves_when_a_mounted_path_or_the_home_service_changes() {
    let base = resolved_serving("/home/alice/project", "/home/alice", "");
    assert!(!base.moved_to(&base.clone()));
    assert!(
        base.moved_to(&resolved_serving("/home/alice/other", "/home/alice", "")),
        "новый проект не перемонтирует"
    );
    assert!(
        base.moved_to(&resolved_serving("/home/alice/project", "/home/other", "")),
        "новый $HOME не перемонтирует"
    );
    let without = resolve(Path::new(STATE_ROOT), AGENT_UID, &host("/home/alice/project", "/home/alice")).unwrap();
    assert!(base.moved_to(&without), "выключенная служба $HOME не перемонтирует");
}

/// `$HOME` runner записан через ссылку (`/home` → `/data/home`): агент получает канонический путь, а исходный в
/// контейнере становится ссылкой на него — но только вне системных каталогов и корня состояния.
#[test]
fn original_home_path_becomes_a_link_only_outside_system_paths() {
    let environment = resolved_serving("/data/home/alice/project", "/data/home/alice", "/home/alice");
    assert_eq!(
        environment.home,
        Path::new("/data/home/alice"),
        "агент получил не канонический $HOME"
    );
    assert_eq!(environment.home_alias.as_deref(), Some(Path::new("/home/alice")));

    for origin in [
        "",
        "/data/home/alice",
        "/var/home/alice",
        "/usr/home/alice",
        "/home/agent/alice",
        "relative/home",
        "/data/home/alice/project/x",
    ] {
        let environment = resolved_serving("/data/home/alice/project", "/data/home/alice", origin);
        assert_eq!(environment.home_alias, None, "исходный путь {origin:?} получил ссылку");
    }
}

#[test]
fn link_to_the_canonical_home_replaces_an_old_link_and_keeps_a_real_directory() {
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("deep/home/alice");
    link_alias(&alias, Path::new("/data/home/alice")).unwrap();
    assert_eq!(std::fs::read_link(&alias).unwrap(), Path::new("/data/home/alice"));
    link_alias(&alias, Path::new("/data/other")).unwrap();
    assert_eq!(
        std::fs::read_link(&alias).unwrap(),
        Path::new("/data/other"),
        "прежняя ссылка не заменена"
    );

    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("file"), "keep").unwrap();
    assert!(
        link_alias(&real, Path::new("/data/home/alice")).is_err(),
        "настоящий каталог заменён ссылкой"
    );
    assert_eq!(std::fs::read_to_string(real.join("file")).unwrap(), "keep");
}
