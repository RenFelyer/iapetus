//! Сценарии: каждый проверяет одно обещание реестра.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use rusqlite::params;

use super::*;

const MARKER: &str = "sk-ant-oat01-marker-7f3a91";

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Содержимое всех файлов каталога: база, журнал WAL и разделяемая память.
fn all_bytes(dir: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        bytes.extend(fs::read(entry.unwrap().path()).unwrap());
    }
    bytes
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// Секретность значений

#[test]
fn secret_value_is_stored_encrypted_in_every_database_file() {
    let mut f = Fixture::new();
    f.reg.env_set("all.API_TOKEN", MARKER).unwrap();
    let orbit_key = f.reg.issue_orbit_key(1).unwrap();

    let open = all_bytes(f.dir.path());
    assert!(!contains(&open, MARKER), "значение лежит в базе или журнале открытым текстом");
    assert!(!contains(&open, orbit_key.expose()), "orbit_key лежит в базе открытым текстом");

    let dir = f.dir.path().to_owned();
    drop(f.reg);
    let closed = all_bytes(&dir);
    assert!(closed.len() > 4096, "после закрытия в каталоге нет базы");
    assert!(!contains(&closed, MARKER), "значение лежит в файле базы открытым текстом");
}

#[test]
fn secret_never_appears_in_debug_display_or_errors() {
    let mut f = Fixture::new();
    f.reg.env_set("all.API_TOKEN", MARKER).unwrap();
    let orbit_key = f.reg.issue_orbit_key(1).unwrap();
    let secret = orbit_key.expose().to_owned();

    let mut shown = vec![
        format!("{:?}", Token::new(MARKER)),
        format!("{:?}", f.reg),
        format!("{orbit_key:?}"),
    ];
    shown.push(format!("{:?}", TokenCipher::from_key(TEST_KEY)));
    let error = f.reg.check_orbit_key(&OrbitKey::new(MARKER)).unwrap_err();
    shown.push(format!("{error} {error:?}"));
    // Расшифровка чужим ключом: ошибка шифрования не должна цитировать ни значение, ни шифротекст.
    f.reg.cipher = TokenCipher::from_key([9; 32]);
    let error = f.reg.env_all().unwrap_err();
    shown.push(format!("{error} {error:?}"));

    for text in shown {
        assert!(!text.contains(MARKER), "значение попало в текст: {text}");
        assert!(!text.contains(&secret), "orbit_key попал в текст: {text}");
    }
}

#[test]
fn value_does_not_decrypt_with_another_encryption_key() {
    let mut f = Fixture::new();
    f.reg.env_set("all.API_TOKEN", MARKER).unwrap();
    drop(f.reg);

    let reg = Registry::open(&f.dir.path().join("registry.db"), TokenCipher::from_key([9; 32])).unwrap();

    assert!(matches!(reg.env_all(), Err(Error::Crypto)));
}

#[test]
fn corrupted_ciphertext_is_rejected() {
    let mut f = Fixture::new();
    f.reg.env_set("all.API_TOKEN", MARKER).unwrap();

    // Последний байт шифротекста — часть тега аутентификации.
    let mut sealed: Vec<u8> = f.reg.db.query_row("SELECT value_sealed FROM env", [], |row| row.get(0)).unwrap();
    *sealed.last_mut().unwrap() ^= 1;
    f.reg.db.execute("UPDATE env SET value_sealed = ?1", params![sealed]).unwrap();
    assert!(matches!(f.reg.env_all(), Err(Error::Crypto)), "подделанный шифротекст принят");

    f.reg.db.execute("UPDATE env SET value_sealed = x'0102'", []).unwrap();
    assert!(matches!(f.reg.env_all(), Err(Error::Crypto)), "обрезанный шифротекст принят");
}

#[test]
fn one_value_is_sealed_differently_each_time() {
    let mut f = Fixture::new();
    f.reg.env_set("all.A", MARKER).unwrap();
    f.reg.env_set("all.B", MARKER).unwrap();
    let sealed = |key: &str| -> Vec<u8> {
        f.reg
            .db
            .query_row("SELECT value_sealed FROM env WHERE key = ?1", [key], |row| row.get(0))
            .unwrap()
    };

    assert_ne!(sealed("all.A")[..24], sealed("all.B")[..24], "nonce повторился");
}

// Файлы с секретами

#[test]
fn encryption_key_file_is_created_private_and_reused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("token.key");

    let first = TokenCipher::load_or_create(&path).unwrap();
    assert_eq!(mode(&path), 0o600);
    assert_eq!(fs::read(&path).unwrap().len(), 32);

    let sealed = first.seal(&Token::new(MARKER), b"aad").unwrap();
    let second = TokenCipher::load_or_create(&path).unwrap();
    assert_eq!(second.open(&sealed, b"aad").unwrap().expose(), MARKER, "перечитанный ключ не тот");
}

#[test]
fn encryption_key_file_readable_by_others_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("token.key");
    fs::write(&path, [1u8; 32]).unwrap();

    // Группа и остальные проверяются каждые отдельно: ловится именно та часть маски, которая нарушена.
    for open in [0o640, 0o604, 0o660, 0o606] {
        fs::set_permissions(&path, fs::Permissions::from_mode(open)).unwrap();
        assert!(
            matches!(TokenCipher::load_or_create(&path), Err(Error::KeyFile(_))),
            "права {open:o} приняты"
        );
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    TokenCipher::load_or_create(&path).unwrap();
}

#[test]
fn encryption_key_file_of_wrong_length_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("token.key");

    for len in [0, 31, 33, 4096] {
        fs::write(&path, vec![1u8; len]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            matches!(TokenCipher::load_or_create(&path), Err(Error::KeyFile(_))),
            "ключ длины {len} принят"
        );
    }
}

#[test]
fn database_files_are_private() {
    let mut f = Fixture::new();
    f.reg.env_set("all.A", MARKER).unwrap();

    for entry in fs::read_dir(f.dir.path()).unwrap() {
        let path = entry.unwrap().path();
        assert_eq!(mode(&path), 0o600, "права {} шире 0600", path.display());
    }
}

// Хранение и миграции

#[test]
fn database_from_a_newer_schema_is_rejected() {
    let f = Fixture::new();
    f.reg.db.pragma_update(None, "user_version", 99).unwrap();
    let path = f.db_path();
    drop(f.reg);

    let error = Registry::open(&path, TokenCipher::from_key(TEST_KEY)).unwrap_err();

    assert!(
        matches!(error, Error::SchemaTooNew { found: 99, .. }),
        "открыта база новой схемы: {error:?}"
    );
}

// Миграция

/// База в схеме с владельцами (версия 2): два владельца, ключи, runner, worker, сессия с назначением.
fn legacy_database(path: &Path, cipher: &TokenCipher) -> (Fingerprint, Fingerprint, PrivateKey) {
    let mut db = Connection::open(path).unwrap();
    schema::tests::create_at(&mut db, cipher, 2).unwrap();
    let (runner, worker) = (
        Fingerprint::from_str("runner-1").unwrap(),
        Fingerprint::from_str("worker-1").unwrap(),
    );
    let key = random_key();
    let public = key.public_key().to_openssh().unwrap();
    let hash = secret::orbit_key_hash(&OrbitKey::new("старый-ключ"));
    db.execute(
        "INSERT INTO owners (id, name, orbit_key_hash) VALUES (1, 'alice', NULL), (2, 'bob', ?1)",
        [hash.as_slice()],
    )
    .unwrap();
    for (id, owner, token) in [(1_i64, 1_i64, MARKER), (2, 2, "второй")] {
        let sealed = cipher.seal(&Token::new(token), &schema::legacy_token_aad(owner, id)).unwrap();
        db.execute(
            "INSERT INTO claude_keys (id, owner_id, label, token_sealed) VALUES (?1, ?2, 'ключ', ?3)",
            rusqlite::params![id, owner, sealed],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO nodes (fingerprint, owner_id, kind, name, os, public_key) VALUES (?1, 1, 'runner', 'дом', 'linux', ?2)",
        rusqlite::params![runner.as_str(), public],
    )
    .unwrap();
    db.execute(
        "INSERT INTO nodes (fingerprint, owner_id, kind, name, os, public_key) VALUES (?1, 2, 'worker', 'ноут', 'linux', ?2)",
        rusqlite::params![worker.as_str(), random_key().public_key().to_openssh().unwrap()],
    )
    .unwrap();
    for node in [&runner, &worker] {
        db.execute(
            "INSERT INTO node_certificates (node, issued_at, valid_before) VALUES (?1, 1, 2)",
            [node.as_str()],
        )
        .unwrap();
    }
    db.execute_batch(
        "INSERT INTO sessions (fingerprint, owner_id, key_id, state, secret_hash) VALUES ('s-1', 2, 1, 'success', x'01');
         INSERT INTO assignments (session, runner) VALUES ('s-1', 'runner-1');",
    )
    .unwrap();
    (runner, worker, key)
}

#[test]
fn owners_are_migrated_away_keeping_runners_sessions_and_the_orbit_key() {
    let f = Fixture::new();
    let path = f.dir.path().join("legacy.db");
    let (runner, worker, key) = legacy_database(&path, &TokenCipher::from_key(TEST_KEY));

    let reg = Registry::open(&path, TokenCipher::from_key(TEST_KEY)).unwrap();

    let nodes: Vec<_> = reg.runners().unwrap().into_iter().map(|n| n.node.fingerprint).collect();
    assert_eq!(nodes, std::slice::from_ref(&runner), "в реестре остались worker или пропали runner");
    assert_eq!(reg.node_public_key(&runner).unwrap(), *key.public_key(), "ключ runner потерян");
    assert!(matches!(reg.node(&worker), Err(Error::NotFound(Entity::Node))));
    let session = reg.session(&Fingerprint::from_str("s-1").unwrap()).unwrap();
    assert_eq!(
        (session.state, session.runner),
        (SessionState::Success, Some(runner)),
        "сессия потеряла состояние или runner"
    );
    reg.check_orbit_key(&OrbitKey::new("старый-ключ"))
        .expect("orbit_key владельца не стал ключом системы");
    assert_eq!(
        session.assignment,
        Some(nerve::protocol::Assignment(1)),
        "назначение из старой базы не получило номер"
    );
}

#[test]
fn assignment_numbers_continue_after_migration_of_existing_assignments() {
    let f = Fixture::new();
    let path = f.dir.path().join("legacy.db");
    let (runner, ..) = legacy_database(&path, &TokenCipher::from_key(TEST_KEY));
    let mut reg = Registry::open(&path, TokenCipher::from_key(TEST_KEY)).unwrap();
    let other = reg.create_session(nerve::protocol::Agent::ClaudeCode).unwrap();
    reg.release_runner(&Fingerprint::from_str("s-1").unwrap()).unwrap();

    let assigned = reg.assign_runner(&other, &runner).unwrap();

    assert_eq!(
        assigned.assignment,
        nerve::protocol::Assignment(2),
        "номер повторил номер перенесённого назначения"
    );
}

#[test]
fn migration_that_cannot_decrypt_a_token_changes_nothing() {
    let f = Fixture::new();
    let path = f.dir.path().join("legacy.db");
    legacy_database(&path, &TokenCipher::from_key([1; 32]));

    let result = Registry::open(&path, TokenCipher::from_key(TEST_KEY));

    assert!(
        matches!(result, Err(Error::Crypto)),
        "миграция с чужим ключом шифрования прошла: {result:?}"
    );
    let db = Connection::open(&path).unwrap();
    let version: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
    let owners: i64 = db.query_row("SELECT count(*) FROM owners", [], |row| row.get(0)).unwrap();
    assert_eq!(
        (version, owners),
        (2, 2),
        "неудачная миграция оставила базу наполовину переделанной"
    );
}

fn table_exists(reg: &Registry, name: &str) -> bool {
    reg.db
        .query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)", [name], |row| {
            row.get(0)
        })
        .unwrap()
}

/// База со схемой, в которой у сессий были ключи агентов, билеты и сохранённые учётные данные.
fn database_with_agent_keys(path: &Path, cipher: &TokenCipher) -> (Fingerprint, PrivateKey) {
    let mut db = Connection::open(path).unwrap();
    schema::tests::create_at(&mut db, cipher, 10).unwrap();
    let runner = Fingerprint::from_str("runner-1").unwrap();
    let key = random_key();
    let sealed = cipher.seal(&Token::new(MARKER), &1_i64.to_le_bytes()).unwrap();
    db.execute(
        "INSERT INTO claude_keys (id, label, token_sealed, invalid) VALUES (1, 'ключ', ?1, 1)",
        [sealed],
    )
    .unwrap();
    db.execute(
        "INSERT INTO nodes (fingerprint, name, os, public_key) VALUES (?1, 'дом', 'linux', ?2)",
        rusqlite::params![runner.as_str(), key.public_key().to_openssh().unwrap()],
    )
    .unwrap();
    let secret_hash = SessionSecret::new("секрет").hash();
    db.execute(
        "INSERT INTO sessions (fingerprint, key_id, state, secret_hash, agent, ticket_hash, agent_stopped) \
         VALUES ('s-1', 1, 'waiting', ?1, 'codex', x'0102', 1), ('s-2', NULL, 'success', NULL, 'claude', NULL, 0)",
        [secret_hash.as_slice()],
    )
    .unwrap();
    db.execute_batch(
        "INSERT INTO session_credentials (session, sealed) VALUES ('s-1', x'0304');
         INSERT INTO session_identity (session, author_name, author_email) VALUES ('s-1', 'Alice', 'alice@example.org');
         INSERT INTO assignments (session, runner) VALUES ('s-1', 'runner-1');",
    )
    .unwrap();
    (runner, key)
}

#[test]
fn database_with_agent_keys_is_opened_keeping_sessions_and_dropping_keys() {
    let f = Fixture::new();
    let path = f.dir.path().join("with-keys.db");
    let (runner, _) = database_with_agent_keys(&path, &TokenCipher::from_key(TEST_KEY));

    let reg = Registry::open(&path, TokenCipher::from_key(TEST_KEY)).unwrap();

    assert!(
        !table_exists(&reg, "claude_keys") && !table_exists(&reg, "session_credentials"),
        "таблицы ключей и учётных данных агентов остались"
    );
    let columns: Vec<String> = reg
        .db
        .prepare("SELECT name FROM pragma_table_info('sessions')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        !columns.iter().any(|name| name == "key_id" || name == "ticket_hash"),
        "в таблице сессий остались столбцы ключа или билета: {columns:?}"
    );
    let first = reg.session(&Fingerprint::from_str("s-1").unwrap()).unwrap();
    assert_eq!(
        (first.agent, first.state, first.runner, first.agent_stopped),
        (nerve::protocol::Agent::Codex, SessionState::Waiting, Some(runner), true),
        "сессия с ключом потеряла агента, состояние, runner или остановку агента"
    );
    let second = reg.session(&Fingerprint::from_str("s-2").unwrap()).unwrap();
    assert_eq!(second.state, SessionState::Success, "сессия без ключа изменилась");
    assert!(
        reg.session_secret_matches(&first.fingerprint, &SessionSecret::new("секрет"))
            .unwrap(),
        "секрет входа session потерян"
    );
    assert_eq!(
        reg.session_identity(&first.fingerprint).unwrap().author.map(|author| author.name),
        Some("Alice".to_owned()),
        "пользователь сессии потерян"
    );
    let version: u32 = reg.db.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
    assert_eq!(version, 13, "схема не доведена до последней версии");
}

#[test]
fn migrated_database_accepts_new_sessions_and_foreign_keys_stay_enforced() {
    let f = Fixture::new();
    let path = f.dir.path().join("with-keys.db");
    database_with_agent_keys(&path, &TokenCipher::from_key(TEST_KEY));
    let mut reg = Registry::open(&path, TokenCipher::from_key(TEST_KEY)).unwrap();

    let created = reg.create_session(nerve::protocol::Agent::Antigravity).unwrap();

    assert_eq!(reg.session(&created).unwrap().agent, nerve::protocol::Agent::Antigravity);
    let enforced: bool = reg.db.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap();
    assert!(enforced, "после миграции внешние ключи не включены");
    reg.remove_session(&Fingerprint::from_str("s-1").unwrap()).unwrap();
    let identities: i64 = reg
        .db
        .query_row("SELECT count(*) FROM session_identity", [], |row| row.get(0))
        .unwrap();
    assert_eq!(identities, 0, "каскадное удаление пересобранной таблицы сессий не работает");
}

#[test]
fn session_secret_is_stored_only_as_a_hash() {
    let mut f = Fixture::new();
    let session = f.session();
    let secret = f.reg.issue_session_secret(&session).unwrap();
    f.reg.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    assert!(
        !contains(&all_bytes(f.dir.path()), secret.expose()),
        "секрет сессии лежит в базе открытым текстом"
    );
    assert!(!format!("{secret:?}").contains(secret.expose()), "секрет сессии попал в Debug");
}
