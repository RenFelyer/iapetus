//! Файл настроек: права `0600` обязательны, потому что в нём `orbit_key`.

use std::fs;

use super::*;
use crate::testing;

const CONTENT: &str = "# worker\norbit_url = orbit.example:2222\norbit_ca = ssh-ed25519 AAAA ca\norbit_key = secret:jwt\nname = alice\n";

fn write(dir: &Path, mode: u32) -> PathBuf {
    let path = dir.join("worker.conf");
    fs::write(&path, CONTENT).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn file_with_owner_only_access_is_loaded() {
    let dir = testing::workdir();
    for mode in [0o600, 0o400] {
        let settings = load(&write(dir.path(), mode)).unwrap();
        assert_eq!(settings.orbit_url, "orbit.example:2222", "orbit_url из файла при правах {mode:o}");
        assert_eq!(settings.orbit_ca, "ssh-ed25519 AAAA ca", "значение с пробелами читается целиком");
        assert_eq!(settings.name.as_deref(), Some("alice"), "name из файла");
    }
}

#[test]
fn file_wider_than_0600_is_rejected() {
    let dir = testing::workdir();
    for mode in [0o640, 0o604, 0o644, 0o660, 0o700, 0o666] {
        let error = load(&write(dir.path(), mode)).unwrap_err();
        assert!(
            format!("{error:#}").contains("0600"),
            "права {mode:o} шире 0600 должны отвергаться: {error:#}"
        );
    }
}

#[test]
fn incomplete_or_unknown_settings_are_rejected() {
    for text in [
        "orbit_url = a:1\norbit_ca = k\n",
        "orbit_url = a:1\norbit_ca = k\norbit_key = s\nextra = 1\n",
    ] {
        assert!(
            parse(text).is_err(),
            "настройки без обязательного ключа или с лишним ключом: {text:?}"
        );
    }
}

#[test]
fn identity_is_read_from_env_file_and_author_needs_both_parts() {
    let dir = testing::workdir();
    let env = dir.path().join(".env");
    assert_eq!(
        session_identity(&env).unwrap(),
        SessionIdentity::default(),
        "нет .env, а идентичность задана"
    );

    fs::write(
        &env,
        "GIT_AUTHOR_NAME=Alice\nGIT_AUTHOR_EMAIL=alice@example.org\nGITHUB_TOKEN='ghp-1'\n",
    )
    .unwrap();
    let identity = session_identity(&env).unwrap();
    assert_eq!(identity.github_token.as_deref(), Some("ghp-1"), "токен не прочитан");
    assert_eq!(
        identity.author,
        Some(Author {
            name: "Alice".to_owned(),
            email: "alice@example.org".to_owned()
        }),
        "автор не прочитан"
    );

    fs::write(&env, "GIT_AUTHOR_NAME=Alice\n").unwrap();
    assert_eq!(session_identity(&env).unwrap().author, None, "автор без почты принят");
}
