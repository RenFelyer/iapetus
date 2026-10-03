//! Реестр orbit server в SQLite: единственный `orbit_key` системы, waldo runner с открытыми
//! ключами и записями о выданных сертификатах, orbit session и назначения runner к сессиям.
//!
//! Владельцев и учётных записей нет: все равны, любой waldo worker с действующим `orbit_key` пользуется любым
//! runner и сессией. waldo worker в реестре нет вовсе — личности у него нет.
//!
//! Fingerprint runner придумывает сам runner, закрытый ключ остаётся у него. Реестр хранит открытый ключ и по
//! нему проверяет продление; отзыв runner — пометка в записи.
//!
//! Присутствие runner в реестре не хранится: его ведёт server в памяти по открытым каналам управления.
//!
//! Учётных данных агентов в реестре нет: агент входит сам в своём контейнере.
//!
//! Значения окружения команд и токен GitHub лежат в базе зашифрованными ключом из файла `registry.key`.
//!
//! Таблицы и запросы — в подмодулях `orbit_key`, `nodes` (узлы, сертификаты, отзыв),
//! `sessions` (сессии, секреты, назначения), `identity` (пользователь сессии), `env` (окружение команд) и
//! `versions` (когда версия агента впервые увидена в канале выпусков); все они дополняют [`Registry`].

mod cipher;
mod env;
mod identity;
mod nodes;
mod orbit_key;
mod schema;
mod secret;
mod sessions;
mod versions;

#[cfg(test)]
#[path = "../../tests/server/registry.rs"]
mod tests;

use std::fmt;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use nerve::fingerprint::Fingerprint;
use nerve::session_state::SessionState;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use tokio::sync::watch;

pub(super) use cipher::TokenCipher;
pub(super) use nerve::connect::OrbitKey;
pub(super) use nodes::{CertificateRecord, NewNode, RunnerInfo};
pub(super) use secret::{SessionSecret, Token};

use secret::{hex, random};
use sessions::state_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Entity {
    Node,
    Session,
}

/// Ошибки реестра. Ни одна не содержит токен или `orbit_key`.
#[derive(Debug)]
pub(super) enum Error {
    /// Нет такой записи.
    NotFound(Entity),
    /// Имя пустое.
    InvalidName,
    /// Родитель клона не зарегистрирован, отозван или сам клон.
    InvalidParent,
    /// С runner уже работает другая сессия.
    RunnerBusy,
    /// Узел отозван.
    Revoked,
    /// Такого перехода состояния сессии нет среди разрешённых.
    InvalidTransition {
        from: SessionState,
        to: SessionState,
    },
    /// Неверный или заменённый `orbit_key`.
    BadCredentials,
    /// Fingerprint уже принадлежит другому узлу или сессии.
    FingerprintTaken,
    /// Этот открытый ключ уже зарегистрирован за другим узлом.
    KeyTaken,
    /// Открытый ключ не записывается в формат OpenSSH.
    InvalidPublicKey,
    /// Сессия и runner не связаны: runner без назначения или сессия без runner.
    NotLinked,
    /// База создана более новой программой.
    SchemaTooNew {
        found: u32,
        supported: u32,
    },
    /// Файл ключа шифрования не годится.
    KeyFile(&'static str),
    /// Шифротекст не расшифровался или не зашифровался.
    Crypto,
    /// Системный генератор случайных чисел недоступен.
    Random,
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(entity) => write!(f, "{} не найден", entity_name(*entity)),
            Self::InvalidName => f.write_str("имя пустое"),
            Self::InvalidParent => f.write_str("у клона нет годного родителя: он не зарегистрирован, отозван или сам клон"),
            Self::RunnerBusy => f.write_str("runner занят другой сессией"),
            Self::Revoked => f.write_str("узел отозван"),
            Self::InvalidTransition { from, to } => {
                write!(f, "переход сессии из {} в {} не допускается", state_name(*from), state_name(*to))
            }
            Self::BadCredentials => f.write_str("неверный orbit_key"),
            Self::FingerprintTaken => f.write_str("fingerprint принадлежит другому узлу"),
            Self::KeyTaken => f.write_str("открытый ключ уже зарегистрирован за другим узлом"),
            Self::InvalidPublicKey => f.write_str("открытый ключ не годится"),
            Self::NotLinked => f.write_str("сессия и runner не связаны"),
            Self::SchemaTooNew { found, supported } => {
                write!(f, "версия схемы базы {found} новее поддерживаемой {supported}")
            }
            Self::KeyFile(reason) => write!(f, "файл ключа шифрования: {reason}"),
            Self::Crypto => f.write_str("ошибка шифрования"),
            Self::Random => f.write_str("генератор случайных чисел недоступен"),
            Self::Io(e) => write!(f, "ввод-вывод: {e}"),
            Self::Sqlite(e) => write!(f, "SQLite: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

fn entity_name(entity: Entity) -> &'static str {
    match entity {
        Entity::Node => "узел",
        Entity::Session => "сессия",
    }
}

fn fingerprint_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Fingerprint> {
    let text: String = row.get(index)?;
    Fingerprint::from_str(&text).map_err(|e| conversion_error(index, e.to_string()))
}

fn conversion_error(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, message.into())
}

/// Транзакция сразу берёт блокировку записи: проверки внутри неё не гоняются с другим писателем.
fn begin(db: &mut Connection) -> Result<Transaction<'_>, Error> {
    Ok(db.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

pub(super) struct Registry {
    db: Connection,
    cipher: TokenCipher,
    /// Время выдачи действующего `orbit_key`: подписчики узнают о новом ключе сразу.
    orbit_key_issued: watch::Sender<Option<i64>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry").finish_non_exhaustive()
    }
}

impl Registry {
    /// Открывает базу, создаёт её с правами `0600` и применяет миграции.
    pub(super) fn open(path: &Path, cipher: TokenCipher) -> Result<Self, Error> {
        // Файл создаётся заранее: SQLite делает это с правами по umask, а в базе лежат токены.
        OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
        let mut db = Connection::open(path)?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        db.busy_timeout(Duration::from_secs(5))?;
        schema::migrate(&mut db, &cipher)?;
        let issued: Option<i64> = db
            .query_row("SELECT issued_at FROM orbit_key WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        Ok(Self {
            db,
            cipher,
            orbit_key_issued: watch::channel(issued).0,
        })
    }
}

/// Сравнение хешей без раннего выхода: время ответа не выдаёт, сколько байт совпало.
fn same_hash(stored: &[u8], given: &[u8]) -> bool {
    stored.len() == given.len() && stored.iter().zip(given).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}
