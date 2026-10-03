use super::*;

/// Схема ровно версии `version`: тесты миграций собирают на ней базу прежней программы.
pub(crate) fn create_at(db: &mut Connection, cipher: &TokenCipher, version: u32) -> Result<(), Error> {
    apply(db, cipher, 0, version)
}
#[test]
fn migration_12_keeps_sessions_of_all_agents_and_removes_the_default_agent() {
    type Row = (String, String, Option<Vec<u8>>, String, i64);
    let cipher = TokenCipher::from_key([7; 32]);
    let mut db = Connection::open_in_memory().unwrap();
    create_at(&mut db, &cipher, 11).unwrap();
    db.execute_batch(
        "INSERT INTO sessions (fingerprint, state, secret_hash, agent, agent_stopped) VALUES
            ('s-claude', 'waiting', x'01', 'claude', 0),
            ('s-codex', 'success', NULL, 'codex', 1),
            ('s-agy', 'warning', x'02', 'antigravity', 0);",
    )
    .unwrap();

    apply(&mut db, &cipher, 11, 12).unwrap();

    let rows: Vec<Row> = db
        .prepare("SELECT fingerprint, state, secret_hash, agent, agent_stopped FROM sessions ORDER BY fingerprint")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let expected: Vec<Row> = vec![
        ("s-agy".into(), "warning".into(), Some(vec![2]), "antigravity".into(), 0),
        ("s-claude".into(), "waiting".into(), Some(vec![1]), "claude".into(), 0),
        ("s-codex".into(), "success".into(), None, "codex".into(), 1),
    ];
    assert_eq!(rows, expected, "данные сессий изменились при миграции");
    assert!(
        db.execute("INSERT INTO sessions (fingerprint, state) VALUES ('s-new', 'waiting')", [])
            .is_err(),
        "вставка без агента принята: остался DEFAULT"
    );
    assert!(
        db.execute(
            "INSERT INTO sessions (fingerprint, state, agent) VALUES ('s-new', 'waiting', 'gemini')",
            []
        )
        .is_err(),
        "неизвестный агент принят: пропал CHECK"
    );
}
