use super::*;

#[test]
fn orbit_key_is_hidden_in_debug_output() {
    let key = OrbitKey::new("orbit-key-secret");
    assert!(!format!("{key:?}").contains("orbit-key-secret"), "orbit_key попал в Debug");
}

#[test]
fn user_is_found_by_uid_in_passwd_text() {
    let passwd =
        "root:x:0:0:root:/root:/bin/bash\n# чужая строка\nbad\nalice:x:1000:1000::/home/alice:/bin/zsh\n:x:1001:1001::/:/bin/false\n";
    assert_eq!(
        user_by_uid(passwd, 1000).as_deref(),
        Some("alice"),
        "пользователь с uid 1000 не найден"
    );
    assert_eq!(user_by_uid(passwd, 0).as_deref(), Some("root"), "root не найден");
    assert_eq!(user_by_uid(passwd, 1001), None, "принята строка без имени");
    assert_eq!(user_by_uid(passwd, 5), None, "найден несуществующий uid");
}
