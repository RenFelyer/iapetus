use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::session::agent::Profile;
use nerve::agent::Agent;

#[test]
fn secret_values_land_in_private_files_and_missing_values_leave_files_alone() {
    let dir = tempfile::tempdir().unwrap();
    let written = dir.path().join("run/secrets/session-secret");
    let untouched = dir.path().join("kept");
    std::fs::write(&untouched, "old").unwrap();

    write_secrets(&[(&written, Some("s3cret")), (&untouched, None)]).unwrap();

    assert_eq!(std::fs::read_to_string(&written).unwrap(), "s3cret");
    assert_eq!(
        std::fs::metadata(&written).unwrap().permissions().mode() & 0o777,
        0o600,
        "секрет доступен не только владельцу"
    );
    assert_eq!(std::fs::read_to_string(&untouched).unwrap(), "old", "файл без значения перезаписан");
}

#[test]
fn resume_flag_goes_right_after_the_agent_program() {
    let command = |words: &[&str]| words.iter().map(|word| (*word).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        Profile(Agent::ClaudeCode).resume_command(&command(&["claude", "--model", "x"])),
        command(&["claude", "--continue", "--model", "x"])
    );
    assert_eq!(
        Profile(Agent::Codex).resume_command(&command(&["codex"])),
        command(&["codex", "resume", "--last"])
    );
    assert_eq!(
        Profile(Agent::Antigravity).resume_command(&command(&["agy"])),
        command(&["agy", "-c"])
    );
}

/// Записи изоляции профиля становятся ссылками в каталог состояния: Antigravity ищет состояние в `$HOME/.gemini`, а
/// профили со своей переменной состояния записей не имеют.
#[test]
fn home_entries_of_the_profile_become_links_into_the_state_directory() {
    let state_root = std::path::Path::new("/home/agent");
    let links = |agent| home_links(&Profile(agent).isolation().home_links, state_root);
    assert_eq!(
        links(Agent::Antigravity),
        [fs::Symlink {
            name: ".gemini".to_owned(),
            target: "/home/agent/.gemini".to_owned(),
        }]
    );
    assert!(links(Agent::ClaudeCode).is_empty() && links(Agent::Codex).is_empty());
}
