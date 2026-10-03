use super::*;
use crate::agent::AGENT_ROOT;

fn check(dir: &str) -> Result<(), String> {
    check_project(Path::new(dir), Path::new(AGENT_ROOT))
}

const CONTRACT: &[&str] = &[
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/usr",
    "/etc",
    "/opt",
    "/opt/iapetus",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/tmp",
    "/var",
    "/work",
    AGENT_ROOT,
    "/home/agent/.claude",
    "/home/agent/.codex",
    "/home/agent/.gemini",
];

#[track_caller]
fn assert_refused(dir: &str, relation: &str) {
    match check(dir) {
        Err(message) => assert!(message.contains(relation), "отказ для {dir} не говорит «{relation}»: {message}"),
        Ok(()) => panic!("каталог {dir} ({relation} путь контейнера session) принят"),
    }
}

#[test]
fn directory_equal_to_a_container_path_is_refused() {
    for dir in CONTRACT
        .iter()
        .filter(|dir| !dir.starts_with("/home/agent/") && **dir != "/opt/iapetus")
    {
        assert_refused(dir, "совпадает с");
    }
}

#[test]
fn directory_inside_a_container_path_is_refused() {
    for path in CONTRACT {
        assert_refused(&format!("{path}/site"), "лежит внутри");
    }
}

#[test]
fn directory_containing_a_container_path_is_refused() {
    assert_refused("/home", "содержит");
    assert_refused("/", "содержит");
}

#[test]
fn state_root_is_taken_from_the_argument() {
    assert!(
        check_project(Path::new("/srv/state/x"), Path::new("/srv/state")).is_err(),
        "каталог внутри каталога состояния принят"
    );
    assert!(
        check_project(Path::new("/srv"), Path::new("/srv/state")).is_err(),
        "предок каталога состояния принят"
    );
}

#[test]
fn relative_and_unnormalized_paths_are_refused() {
    for dir in [
        "srv/site",
        "./srv",
        "/srv/../etc",
        "/srv/./site",
        "//srv/site",
        "/srv/site/",
        "/srv//site",
        "",
    ] {
        assert!(check(dir).is_err(), "путь {dir:?} принят");
    }
}

#[test]
fn directories_next_to_container_paths_are_allowed() {
    for dir in [
        "/srv/site",
        "/home/alice/proj",
        "/home/claudette",
        "/home/alice/claude",
        "/workspace",
        "/usr2",
        "/srv/tmp",
        "/mnt/work/lib",
        "/bi",
    ] {
        assert_eq!(check(dir), Ok(()), "каталог {dir} вне путей контейнера отклонён");
    }
}
