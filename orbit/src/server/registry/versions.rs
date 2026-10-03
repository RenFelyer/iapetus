//! Когда server впервые увидел версию агента в канале выпусков: от этого времени отсчитывается задержка перед
//! предложением версии session.

#[cfg(test)]
#[path = "../../../tests/server/registry/versions.rs"]
mod tests;

use rusqlite::params;

use super::{Error, Registry};

impl Registry {
    /// Когда `version` агента `agent` впервые увидена в канале (секунды Unix). Первый вызов запоминает `now`;
    /// второе значение — `true`, если версия увидена только что.
    pub(in crate::server) fn version_first_seen(&mut self, agent: &str, version: &str, now: i64) -> Result<(i64, bool), Error> {
        let inserted = self.db.execute(
            "INSERT INTO agent_versions (agent, version, first_seen) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            params![agent, version, now],
        )?;
        let first_seen = self.db.query_row(
            "SELECT first_seen FROM agent_versions WHERE agent = ?1 AND version = ?2",
            params![agent, version],
            |row| row.get(0),
        )?;
        Ok((first_seen, inserted == 1))
    }
}
