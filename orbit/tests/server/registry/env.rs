use super::*;
use crate::server::registry::tests::Fixture;

#[test]
fn env_value_is_encrypted_in_the_database() {
    let mut f = Fixture::new();
    f.reg.env_set("all.API_TOKEN", "plain-secret-value").unwrap();

    let stored: Vec<u8> = f
        .reg
        .db
        .query_row("SELECT value_sealed FROM env WHERE key = 'all.API_TOKEN'", [], |row| row.get(0))
        .unwrap();

    assert!(
        !stored.windows(b"plain-secret-value".len()).any(|w| w == b"plain-secret-value"),
        "значение окружения лежит в базе открытым текстом"
    );
    assert_eq!(
        f.reg.env_all().unwrap(),
        [("all.API_TOKEN".to_owned(), "plain-secret-value".to_owned())]
    );
}

#[test]
fn env_value_moved_under_another_key_does_not_decrypt() {
    let mut f = Fixture::new();
    f.reg.env_set("all.A", "value").unwrap();
    f.reg.db.execute("UPDATE env SET key = 'all.B'", []).unwrap();

    assert!(
        matches!(f.reg.env_all(), Err(Error::Crypto)),
        "значение, перенесённое под другой ключ, расшифровалось"
    );
}

#[test]
fn env_scopes_select_only_their_own_keys() {
    let mut f = Fixture::new();
    for key in ["all.A", "abc.B", "abcd.C", "xyz.D"] {
        f.reg.env_set(key, "v").unwrap();
    }

    let keys: Vec<String> = f
        .reg
        .env_in_scopes(&["all", "abc"])
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();

    assert_eq!(keys, ["all.A", "abc.B"], "слой abc захватил ключ слоя abcd или чужой слой");
}
