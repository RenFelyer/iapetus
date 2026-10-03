use super::*;
use crate::server::registry::tests::Fixture;

#[test]
fn github_token_is_encrypted_and_bound_to_its_session() {
    let mut f = Fixture::new();
    let (first, second) = (f.session(), f.session());
    let identity = SessionIdentity {
        author: Some(Author {
            name: "Alice".to_owned(),
            email: "alice@example.org".to_owned(),
        }),
        github_token: Some("ghp-plain-secret".to_owned()),
    };
    f.reg.set_session_identity(&first, &identity).unwrap();

    let stored: Vec<u8> = f
        .reg
        .db
        .query_row("SELECT github_token_sealed FROM session_identity", [], |row| row.get(0))
        .unwrap();
    assert!(
        !stored.windows(b"ghp-plain-secret".len()).any(|w| w == b"ghp-plain-secret"),
        "токен GitHub лежит в базе открытым текстом"
    );
    assert_eq!(f.reg.session_identity(&first).unwrap(), identity, "пользователь сессии не прочитан");
    assert_eq!(
        f.reg.session_identity(&second).unwrap(),
        SessionIdentity::default(),
        "у сессии без записи оказался чужой пользователь"
    );

    f.reg
        .db
        .execute("UPDATE session_identity SET session = ?1", [second.as_str()])
        .unwrap();
    assert!(
        matches!(f.reg.session_identity(&second), Err(Error::Crypto)),
        "токен, перенесённый в другую сессию, расшифровался"
    );
}
