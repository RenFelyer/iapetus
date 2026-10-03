//! Схема базы и миграции. Версия схемы — `PRAGMA user_version`: номер последней применённой
//! миграции. Каждая миграция идёт в своей транзакции вместе с повышением версии.

use rusqlite::{Connection, OptionalExtension, Transaction};

use super::{Error, Token, TokenCipher};

/// Шаг схемы: SQL или код, когда данные нужно пересчитать (перешифровать токены).
enum Migration {
    Sql(&'static str),
    Code(fn(&Transaction<'_>, &TokenCipher) -> Result<(), Error>),
}

const MIGRATIONS: &[Migration] = &[
    Migration::Sql(
        "
CREATE TABLE owners (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE CHECK (name <> ''),
    orbit_key_hash BLOB UNIQUE
) STRICT;

CREATE TABLE nodes (
    fingerprint TEXT PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('runner', 'worker')),
    name TEXT NOT NULL CHECK (name <> ''),
    os TEXT NOT NULL,
    public_key TEXT NOT NULL UNIQUE,
    temporary INTEGER NOT NULL DEFAULT 0 CHECK (temporary IN (0, 1)),
    parent TEXT REFERENCES nodes(fingerprint) ON DELETE SET NULL,
    online INTEGER NOT NULL DEFAULT 0 CHECK (online IN (0, 1)),
    UNIQUE (owner_id, name),
    CHECK (kind = 'runner' OR (temporary = 0 AND parent IS NULL AND online = 0))
) STRICT;

CREATE TABLE node_certificates (
    node TEXT PRIMARY KEY REFERENCES nodes(fingerprint) ON DELETE CASCADE,
    issued_at INTEGER NOT NULL,
    valid_before INTEGER NOT NULL CHECK (valid_before > issued_at)
) STRICT;

CREATE TABLE claude_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    token_sealed BLOB NOT NULL,
    invalid INTEGER NOT NULL DEFAULT 0 CHECK (invalid IN (0, 1))
) STRICT;

CREATE TABLE sessions (
    fingerprint TEXT PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
    key_id INTEGER REFERENCES claude_keys(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('success', 'waiting', 'warning', 'failure'))
) STRICT;

CREATE TABLE assignments (
    session TEXT PRIMARY KEY REFERENCES sessions(fingerprint) ON DELETE CASCADE,
    runner TEXT NOT NULL UNIQUE REFERENCES nodes(fingerprint) ON DELETE CASCADE
) STRICT;
",
    ),
    Migration::Sql(
        "
ALTER TABLE sessions ADD COLUMN secret_hash BLOB;
",
    ),
    Migration::Code(drop_owners),
    Migration::Sql(
        "
CREATE TABLE env (
    key TEXT PRIMARY KEY CHECK (key <> ''),
    value_sealed BLOB NOT NULL
) STRICT;
",
    ),
    Migration::Sql(
        "
ALTER TABLE nodes DROP COLUMN online;
ALTER TABLE nodes ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1));
",
    ),
    Migration::Sql(
        "
ALTER TABLE sessions ADD COLUMN agent TEXT NOT NULL DEFAULT 'claude' CHECK (agent IN ('claude', 'codex', 'antigravity'));
",
    ),
    Migration::Sql(
        "
ALTER TABLE sessions ADD COLUMN ticket_hash BLOB;

CREATE TABLE session_credentials (
    session TEXT PRIMARY KEY REFERENCES sessions(fingerprint) ON DELETE CASCADE,
    sealed BLOB NOT NULL
) STRICT;
",
    ),
    Migration::Sql(
        "
CREATE TABLE assignment_counter (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
) STRICT;
ALTER TABLE assignments ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN last_epoch INTEGER NOT NULL DEFAULT 0;
UPDATE assignments SET epoch = rowid;
UPDATE nodes SET last_epoch = coalesce((SELECT epoch FROM assignments WHERE runner = nodes.fingerprint), 0);
INSERT INTO assignment_counter (id, last) VALUES (1, coalesce((SELECT max(epoch) FROM assignments), 0));
",
    ),
    Migration::Sql(
        "
CREATE TABLE session_identity (
    session TEXT PRIMARY KEY REFERENCES sessions(fingerprint) ON DELETE CASCADE,
    author_name TEXT,
    author_email TEXT,
    github_token_sealed BLOB,
    CHECK ((author_name IS NULL) = (author_email IS NULL))
) STRICT;
",
    ),
    Migration::Sql(
        "
ALTER TABLE sessions ADD COLUMN agent_stopped INTEGER NOT NULL DEFAULT 0 CHECK (agent_stopped IN (0, 1));
",
    ),
    Migration::Sql(
        "
DROP TABLE session_credentials;

CREATE TABLE sessions_new (
    fingerprint TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('success', 'waiting', 'warning', 'failure')),
    secret_hash BLOB,
    agent TEXT NOT NULL DEFAULT 'claude' CHECK (agent IN ('claude', 'codex', 'antigravity')),
    agent_stopped INTEGER NOT NULL DEFAULT 0 CHECK (agent_stopped IN (0, 1))
) STRICT;
INSERT INTO sessions_new (fingerprint, state, secret_hash, agent, agent_stopped)
    SELECT fingerprint, state, secret_hash, agent, agent_stopped FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

DROP TABLE claude_keys;
",
    ),
    Migration::Sql(
        "
CREATE TABLE sessions_new (
    fingerprint TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('success', 'waiting', 'warning', 'failure')),
    secret_hash BLOB,
    agent TEXT NOT NULL CHECK (agent IN ('claude', 'codex', 'antigravity')),
    agent_stopped INTEGER NOT NULL DEFAULT 0 CHECK (agent_stopped IN (0, 1))
) STRICT;
INSERT INTO sessions_new (fingerprint, state, secret_hash, agent, agent_stopped)
    SELECT fingerprint, state, secret_hash, agent, agent_stopped FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;
",
    ),
    Migration::Sql(
        "
CREATE TABLE agent_versions (
    agent TEXT NOT NULL,
    version TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (agent, version)
) STRICT;
",
    ),
];

/// Убирает владельцев: все узлы равны, `orbit_key` один на систему, в реестре только runner.
///
/// Таблицы с `owner_id` пересобираются: столбец под `UNIQUE` и внешним ключом SQLite удалить на месте не умеет.
/// Записи waldo worker удаляются вместе со сведениями о сертификатах. Токены ключей агентов, привязанные к владельцу
/// через AAD, перешифровываются под номер ключа. `orbit_key` первого владельца, у которого он был, становится
/// ключом системы: им продолжают пользоваться прежние runner.
fn drop_owners(tx: &Transaction<'_>, cipher: &TokenCipher) -> Result<(), Error> {
    let mut stmt = tx.prepare("SELECT id, owner_id, token_sealed FROM claude_keys")?;
    let sealed: Vec<(i64, i64, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    let mut resealed = Vec::with_capacity(sealed.len());
    for (id, owner, blob) in sealed {
        let token: Token = cipher.open(&blob, &legacy_token_aad(owner, id))?;
        resealed.push((id, cipher.seal(&token, &key_aad(id))?));
    }
    let orbit_key: Option<Vec<u8>> = tx
        .query_row(
            "SELECT orbit_key_hash FROM owners WHERE orbit_key_hash IS NOT NULL ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    tx.execute_batch(
        "
CREATE TABLE orbit_key (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    hash BLOB NOT NULL,
    issued_at INTEGER NOT NULL
) STRICT;

DELETE FROM node_certificates WHERE node IN (SELECT fingerprint FROM nodes WHERE kind <> 'runner');

CREATE TABLE nodes_new (
    fingerprint TEXT PRIMARY KEY,
    name TEXT NOT NULL CHECK (name <> ''),
    os TEXT NOT NULL,
    public_key TEXT NOT NULL UNIQUE,
    temporary INTEGER NOT NULL DEFAULT 0 CHECK (temporary IN (0, 1)),
    parent TEXT REFERENCES nodes(fingerprint) ON DELETE SET NULL,
    online INTEGER NOT NULL DEFAULT 0 CHECK (online IN (0, 1))
) STRICT;
INSERT INTO nodes_new (fingerprint, name, os, public_key, temporary, parent, online)
    SELECT fingerprint, name, os, public_key, temporary, parent, online FROM nodes WHERE kind = 'runner';
DROP TABLE nodes;
ALTER TABLE nodes_new RENAME TO nodes;

CREATE TABLE claude_keys_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    token_sealed BLOB NOT NULL,
    invalid INTEGER NOT NULL DEFAULT 0 CHECK (invalid IN (0, 1))
) STRICT;
INSERT INTO claude_keys_new (id, label, token_sealed, invalid) SELECT id, label, token_sealed, invalid FROM claude_keys;
DROP TABLE claude_keys;
ALTER TABLE claude_keys_new RENAME TO claude_keys;

CREATE TABLE sessions_new (
    fingerprint TEXT PRIMARY KEY,
    key_id INTEGER REFERENCES claude_keys(id) ON DELETE SET NULL,
    state TEXT NOT NULL CHECK (state IN ('success', 'waiting', 'warning', 'failure')),
    secret_hash BLOB
) STRICT;
INSERT INTO sessions_new (fingerprint, key_id, state, secret_hash) SELECT fingerprint, key_id, state, secret_hash FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_new RENAME TO sessions;

DROP TABLE owners;
",
    )?;
    for (id, blob) in resealed {
        tx.execute(
            "UPDATE claude_keys SET token_sealed = ?1 WHERE id = ?2",
            rusqlite::params![blob, id],
        )?;
    }
    if let Some(hash) = orbit_key {
        tx.execute("INSERT INTO orbit_key (id, hash, issued_at) VALUES (1, ?1, unixepoch())", [hash])?;
    }
    Ok(())
}

/// AAD токена ключа агента после отказа от владельцев: номер ключа.
fn key_aad(key: i64) -> [u8; 8] {
    key.to_le_bytes()
}

/// AAD токена до отказа от владельцев: владелец и номер ключа.
pub(super) fn legacy_token_aad(owner: i64, key: i64) -> [u8; 16] {
    let mut aad = [0u8; 16];
    aad[..8].copy_from_slice(&owner.to_le_bytes());
    aad[8..].copy_from_slice(&key.to_le_bytes());
    aad
}

/// Применяет недостающие миграции. Внешние ключи на время миграций выключены: пересборка таблицы удаляет
/// таблицу, на которую ссылаются другие, а `PRAGMA foreign_keys` внутри транзакции не меняется. Ссылки
/// сверяются перед фиксацией каждой миграции.
pub(super) fn migrate(db: &mut Connection, cipher: &TokenCipher) -> Result<(), Error> {
    let found: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = (1u32..).zip(MIGRATIONS).last().map_or(0, |(version, _)| version);
    if found > supported {
        return Err(Error::SchemaTooNew { found, supported });
    }
    db.pragma_update(None, "foreign_keys", false)?;
    let migrated = apply(db, cipher, found, supported);
    db.pragma_update(None, "foreign_keys", true)?;
    migrated
}

fn apply(db: &mut Connection, cipher: &TokenCipher, found: u32, until: u32) -> Result<(), Error> {
    let pending = (1u32..).zip(MIGRATIONS).skip_while(|(version, _)| *version <= found);
    for (version, migration) in pending.take_while(|(version, _)| *version <= until) {
        let tx = db.transaction()?;
        match migration {
            Migration::Sql(sql) => tx.execute_batch(sql)?,
            Migration::Code(step) => step(&tx, cipher)?,
        }
        let broken: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0))?;
        if broken != 0 {
            return Err(Error::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some(format!("миграция {version} оставила {broken} ссылок в пустоту")),
            )));
        }
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/server/registry/schema.rs"]
pub(super) mod tests;
