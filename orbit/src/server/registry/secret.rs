//! Секреты реестра: секретное значение базы, `orbit_key` и секрет сессии. `Debug` печатает их как `***`,
//! чтобы они не попали в журналы и тексты ошибок; читаются только явным `expose`.

use std::fmt;

use nerve::connect::OrbitKey;
use sha2::{Digest, Sha256};

use super::Error;

/// Секретное значение, которое база хранит зашифрованным: окружение команд, токен GitHub.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token(String);

impl Token {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

/// Новый многоразовый токен регистрации узлов (`orbit_key`).
pub(super) fn generate_orbit_key() -> Result<OrbitKey, Error> {
    Ok(OrbitKey::new(hex(&random::<32>()?)))
}

/// В базе лежит только SHA-256 токена: он — 256 случайных бит, перебирать нечего, медленный хеш не нужен.
pub(super) fn orbit_key_hash(key: &OrbitKey) -> [u8; 32] {
    Sha256::digest(key.expose().as_bytes()).into()
}

pub(super) fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Random)?;
    Ok(bytes)
}

pub(super) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 15)]])
        .map(char::from)
        .collect()
}

/// Секрет сессии: server выдаёт его при создании, сессия предъявляет при входе на внутренний слушатель. В базе
/// лежит только SHA-256 — секрет тоже 256 случайных бит.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SessionSecret(String);

impl SessionSecret {
    pub(super) fn generate() -> Result<Self, Error> {
        Ok(Self(hex(&random::<32>()?)))
    }

    pub(crate) fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub(super) fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionSecret(***)")
    }
}
