use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Неизменяемый идентификатор runner или session.
///
/// Ключи окружения (`<fingerprint>.VAR`) и кэш строятся на нём, поэтому это один токен:
/// только `[A-Za-z0-9_-]`, и не `all` — под это имя зарезервированы общие ключи `all.VAR`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Fingerprint(String);

/// Строка не годится в fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintError {
    Empty,
    InvalidChar(char),
    Reserved,
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "fingerprint пуст"),
            Self::InvalidChar(c) => write!(f, "недопустимый символ {c:?} в fingerprint: разрешены только A-Z, a-z, 0-9, _ и -"),
            Self::Reserved => write!(f, "fingerprint `all` зарезервирован под общие ключи окружения"),
        }
    }
}

impl std::error::Error for FingerprintError {}

impl Fingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Fingerprint {
    type Err = FingerprintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(FingerprintError::Empty);
        }
        if let Some(c) = s.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-')) {
            return Err(FingerprintError::InvalidChar(c));
        }
        if s == "all" {
            return Err(FingerprintError::Reserved);
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = FingerprintError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Fingerprint> for String {
    fn from(f: Fingerprint) -> Self {
        f.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[path = "../tests/fingerprint.rs"]
mod tests;
