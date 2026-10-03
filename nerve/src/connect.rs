//! Общее подключение к server: `orbit_key`, имя узла, сертификат, SSH к session.

pub mod certificate;
pub mod server;
pub mod session;

use std::fmt;
use std::str::FromStr;

use anyhow::Context;

/// `orbit_key`: секрет пользователя одной строкой; server хранит и сверяет его. В `Debug` и журнале не показывается.
#[derive(Clone, PartialEq, Eq)]
pub struct OrbitKey(String);

impl OrbitKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl FromStr for OrbitKey {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(text))
    }
}

impl fmt::Debug for OrbitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OrbitKey(***)")
    }
}

/// Имя по умолчанию: имя локального пользователя.
pub fn default_name() -> anyhow::Result<String> {
    let from_env = ["USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    if let Some(name) = from_env {
        return Ok(name);
    }
    let uid = rustix::process::getuid().as_raw();
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    user_by_uid(&passwd, uid).context("имя локального пользователя не определено: задайте --name")
}

/// Имя пользователя с `uid` по тексту `/etc/passwd`.
pub fn user_by_uid(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let line_uid: u32 = fields.nth(1)?.parse().ok()?;
        (line_uid == uid && !name.is_empty()).then(|| name.to_owned())
    })
}

#[cfg(test)]
#[path = "../tests/connect.rs"]
mod tests;
