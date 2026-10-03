//! `orbit_key`: в базе лежит только его хеш.

#[cfg(test)]
#[path = "../../../tests/server/registry/orbit_key.rs"]
mod tests;

use rusqlite::{OptionalExtension, params};
use tokio::sync::watch;

use super::{Error, OrbitKey, Registry, begin, same_hash, secret};

impl Registry {
    /// Выдаёт новый `orbit_key` системы в момент `now` (секунды Unix); прежний перестаёт действовать. Ключ
    /// возвращается один раз, в базе остаётся его хеш и время выдачи.
    ///
    /// Время выдачи отсчитывает отзыв всего, что выдано по прежнему ключу, а входы по ключу помечаются секундой
    /// после его выдачи. Поэтому новый ключ выдаётся не раньше чем через две секунды после прежнего: иначе вход по
    /// прежнему ключу, помеченный следующей секундой, пережил бы отзыв.
    pub(in crate::server) fn issue_orbit_key(&mut self, now: i64) -> Result<OrbitKey, Error> {
        let now = (*self.orbit_key_issued.borrow()).map_or(now, |previous| now.max(previous + 2));
        let key = secret::generate_orbit_key()?;
        self.db.execute(
            "INSERT INTO orbit_key (id, hash, issued_at) VALUES (1, ?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET hash = excluded.hash, issued_at = excluded.issued_at",
            params![secret::orbit_key_hash(&key).as_slice(), now],
        )?;
        self.orbit_key_issued.send_replace(Some(now));
        Ok(key)
    }

    /// Первый `orbit_key`: выдаётся, только если реестр пуст — ни ключа, ни runner. Иначе `None`.
    pub(in crate::server) fn issue_first_orbit_key(&mut self, now: i64) -> Result<Option<OrbitKey>, Error> {
        let tx = begin(&mut self.db)?;
        let used: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM orbit_key) OR EXISTS (SELECT 1 FROM nodes)",
            [],
            |row| row.get(0),
        )?;
        if used {
            return Ok(None);
        }
        let key = secret::generate_orbit_key()?;
        tx.execute(
            "INSERT INTO orbit_key (id, hash, issued_at) VALUES (1, ?1, ?2)",
            params![secret::orbit_key_hash(&key).as_slice(), now],
        )?;
        tx.commit()?;
        self.orbit_key_issued.send_replace(Some(now));
        Ok(Some(key))
    }

    /// Действует ли `key`. Неизвестный, заменённый и пустой ключ неразличимы.
    pub(in crate::server) fn check_orbit_key(&self, key: &OrbitKey) -> Result<(), Error> {
        let stored: Option<Vec<u8>> = self
            .db
            .query_row("SELECT hash FROM orbit_key WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        match stored {
            Some(stored) if same_hash(&stored, &secret::orbit_key_hash(key)) => Ok(()),
            _ => Err(Error::BadCredentials),
        }
    }

    /// Когда выдан действующий `orbit_key` (секунды Unix); `None`, если ключа ещё нет. Всё, что выдано по
    /// прежнему ключу раньше этого момента, недействительно. Получатель узнаёт о новом ключе сразу.
    pub(in crate::server) fn orbit_key_issued(&self) -> watch::Receiver<Option<i64>> {
        self.orbit_key_issued.subscribe()
    }
}
