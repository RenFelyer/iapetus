//! Окружение команд: ключи `all.VAR` и `<fingerprint>.VAR`, значения зашифрованы в базе.

#[cfg(test)]
#[path = "../../../tests/server/registry/env.rs"]
mod tests;

use nerve::fingerprint::Fingerprint;
use rusqlite::{OptionalExtension, params};

use super::{Error, Registry, Token, fingerprint_from_row};

/// Шифротекст значения привязан к ключу записи: значение, перенесённое в базе под другой ключ, не расшифруется.
fn env_aad(key: &str) -> Vec<u8> {
    [b"env:".as_slice(), key.as_bytes()].concat()
}

impl Registry {
    pub(in crate::server) fn env_set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let sealed = self.cipher.seal(&Token::new(value), &env_aad(key))?;
        self.db.execute(
            "INSERT INTO env (key, value_sealed) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value_sealed = excluded.value_sealed",
            params![key, sealed],
        )?;
        Ok(())
    }

    pub(in crate::server) fn env_delete(&mut self, key: &str) -> Result<(), Error> {
        self.db.execute("DELETE FROM env WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Все записи, по ключу.
    pub(in crate::server) fn env_all(&self) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self.db.prepare("SELECT key, value_sealed FROM env ORDER BY key")?;
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows.into_iter().map(|(key, sealed)| self.open_env(key, &sealed)).collect()
    }

    /// Записи слоёв `scopes` (`all` или fingerprint): ключ записи начинается с `<слой>.`.
    pub(in crate::server) fn env_in_scopes(&self, scopes: &[&str]) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .db
            .prepare("SELECT key, value_sealed FROM env WHERE substr(key, 1, length(?1) + 1) = ?1 || '.' ORDER BY key")?;
        let mut entries = Vec::new();
        for scope in scopes {
            let rows: Vec<(String, Vec<u8>)> = stmt
                .query_map([scope], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (key, sealed) in rows {
                entries.push(self.open_env(key, &sealed)?);
            }
        }
        Ok(entries)
    }

    /// Родитель клона; у обычного runner и неизвестного узла — `None`.
    pub(in crate::server) fn node_parent(&self, fingerprint: &Fingerprint) -> Result<Option<Fingerprint>, Error> {
        let parent = self
            .db
            .query_row(
                "SELECT parent FROM nodes WHERE fingerprint = ?1 AND parent IS NOT NULL",
                [fingerprint.as_str()],
                |row| fingerprint_from_row(row, 0),
            )
            .optional()?;
        Ok(parent)
    }

    fn open_env(&self, key: String, sealed: &[u8]) -> Result<(String, String), Error> {
        let value = self.cipher.open(sealed, &env_aad(&key))?;
        Ok((key, value.expose().to_owned()))
    }
}
