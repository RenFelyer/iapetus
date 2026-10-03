//! Сессии: создание, секреты, состояния и назначения runner.

#[cfg(test)]
#[path = "../../../tests/server/registry/sessions.rs"]
mod tests;

use std::str::FromStr as _;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Agent, Assignment};
use nerve::session_state::SessionState;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::nodes::find_node;
use super::{Entity, Error, Registry, SessionSecret, begin, conversion_error, fingerprint_from_row, hex, random, same_hash};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct SessionInfo {
    pub fingerprint: Fingerprint,
    pub agent: Agent,
    pub state: SessionState,
    pub runner: Option<Fingerprint>,
    /// Номер назначения; есть, когда есть `runner`.
    pub assignment: Option<Assignment>,
    /// Агент остановлен и ждёт явного назначения, хотя пара в реестре есть: runner не вернулся вовремя.
    pub agent_stopped: bool,
}

/// Итог [`Registry::assign_runner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct Assigned {
    /// Номер текущей пары: новый, если пара сменилась.
    pub assignment: Assignment,
    /// Назначение изменилось: пара новая.
    pub changed: bool,
    /// Пара, которую заменила новая; `None`, если session была без runner или назначение то же.
    pub previous: Option<(Fingerprint, Assignment)>,
}

pub(super) fn assignment_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Assignment> {
    let epoch: i64 = row.get(index)?;
    u64::try_from(epoch)
        .map(Assignment)
        .map_err(|_| conversion_error(index, format!("номер назначения {epoch} отрицателен")))
}

pub(super) fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Success => "success",
        SessionState::Waiting => "waiting",
        SessionState::Warning => "warning",
        SessionState::Failure => "failure",
    }
}

fn state_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<SessionState> {
    let text: String = row.get(index)?;
    [
        SessionState::Success,
        SessionState::Waiting,
        SessionState::Warning,
        SessionState::Failure,
    ]
    .into_iter()
    .find(|state| state_name(*state) == text)
    .ok_or_else(|| conversion_error(index, format!("неизвестное состояние сессии {text:?}")))
}

/// Разрешённые переходы состояния сессии: из `waiting` — в рабочее или в
/// `failure`; `success` и `warning` ходят друг к другу (пауза и возврат), оба ещё могут
/// снова ждать (нужен вход, агент остановлен планово); любое состояние, кроме `failure`,
/// может упасть в него; из `failure` сессию возвращает только человек — в `waiting`.
pub(in crate::server) fn transition_allowed(from: SessionState, to: SessionState) -> bool {
    use SessionState::{Failure, Success, Waiting, Warning};
    matches!(
        (from, to),
        (Waiting, Success | Failure) | (Success, Warning | Waiting | Failure) | (Warning, Success | Waiting | Failure) | (Failure, Waiting)
    )
}

fn agent_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Agent> {
    let text: String = row.get(index)?;
    text.parse().map_err(|why| conversion_error(index, format!("агент сессии: {why}")))
}

const SESSION_SELECT: &str = "SELECT s.fingerprint, s.state, a.runner, s.agent, a.epoch, s.agent_stopped \
                              FROM sessions s LEFT JOIN assignments a ON a.session = s.fingerprint";

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<SessionInfo> {
    let (runner, assignment) = match row.get::<_, Option<String>>(2)? {
        Some(_) => (Some(fingerprint_from_row(row, 2)?), Some(assignment_from_row(row, 4)?)),
        None => (None, None),
    };
    Ok(SessionInfo {
        fingerprint: fingerprint_from_row(row, 0)?,
        agent: agent_from_row(row, 3)?,
        state: state_from_row(row, 1)?,
        runner,
        assignment,
        agent_stopped: row.get(5)?,
    })
}

pub(super) fn new_fingerprint() -> Result<Fingerprint, Error> {
    let text = hex(&random::<16>()?);
    Fingerprint::from_str(&text).map_err(|_| Error::Random)
}

pub(super) fn find_session(db: &Connection, fingerprint: &Fingerprint) -> Result<SessionInfo, Error> {
    db.query_row(
        &format!("{SESSION_SELECT} WHERE s.fingerprint = ?1"),
        [fingerprint.as_str()],
        session_from_row,
    )
    .optional()?
    .ok_or(Error::NotFound(Entity::Session))
}

pub(super) fn assigned_session(db: &Connection, runner: &Fingerprint) -> Result<Option<Fingerprint>, Error> {
    let session = db
        .query_row("SELECT session FROM assignments WHERE runner = ?1", [runner.as_str()], |row| {
            fingerprint_from_row(row, 0)
        })
        .optional()?;
    Ok(session)
}

impl Registry {
    /// Новая сессия агента `agent` ждёт его запуска и входа.
    pub(in crate::server) fn create_session(&mut self, agent: Agent) -> Result<Fingerprint, Error> {
        let fingerprint = new_fingerprint()?;
        self.db.execute(
            "INSERT INTO sessions (fingerprint, agent, state) VALUES (?1, ?2, 'waiting')",
            params![fingerprint.as_str(), agent.id()],
        )?;
        Ok(fingerprint)
    }

    pub(in crate::server) fn session(&self, fingerprint: &Fingerprint) -> Result<SessionInfo, Error> {
        find_session(&self.db, fingerprint)
    }

    /// Выдаёт сессии новый секрет для входа на внутренний слушатель server; прежний перестаёт действовать. Секрет
    /// возвращается один раз, в базе остаётся его хеш.
    pub(in crate::server) fn issue_session_secret(&mut self, session: &Fingerprint) -> Result<SessionSecret, Error> {
        let secret = SessionSecret::generate()?;
        let updated = self.db.execute(
            "UPDATE sessions SET secret_hash = ?1 WHERE fingerprint = ?2",
            params![secret.hash().as_slice(), session.as_str()],
        )?;
        if updated == 0 {
            return Err(Error::NotFound(Entity::Session));
        }
        Ok(secret)
    }

    /// Верен ли секрет сессии. Несуществующая сессия, сессия без секрета и чужой секрет неразличимы.
    pub(in crate::server) fn session_secret_matches(&self, session: &Fingerprint, secret: &SessionSecret) -> Result<bool, Error> {
        let stored: Option<Option<Vec<u8>>> = self
            .db
            .query_row(
                "SELECT secret_hash FROM sessions WHERE fingerprint = ?1",
                [session.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(Some(stored)) = stored else {
            return Ok(false);
        };
        Ok(same_hash(&stored, &secret.hash()))
    }

    pub(in crate::server) fn sessions(&self) -> Result<Vec<SessionInfo>, Error> {
        let mut stmt = self.db.prepare(&format!("{SESSION_SELECT} ORDER BY s.fingerprint"))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub(in crate::server) fn remove_session(&mut self, fingerprint: &Fingerprint) -> Result<(), Error> {
        let deleted = self
            .db
            .execute("DELETE FROM sessions WHERE fingerprint = ?1", [fingerprint.as_str()])?;
        if deleted == 0 {
            return Err(Error::NotFound(Entity::Session));
        }
        Ok(())
    }

    /// Меняет состояние сессии по правилам [`transition_allowed`]. Повтор текущего состояния —
    /// не переход и ничего не меняет.
    pub(in crate::server) fn set_session_state(&mut self, fingerprint: &Fingerprint, to: SessionState) -> Result<(), Error> {
        let tx = begin(&mut self.db)?;
        let from = find_session(&tx, fingerprint)?.state;
        if from == to {
            return Ok(());
        }
        if !transition_allowed(from, to) {
            return Err(Error::InvalidTransition { from, to });
        }
        tx.execute(
            "UPDATE sessions SET state = ?1 WHERE fingerprint = ?2",
            params![state_name(to), fingerprint.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Возвращает сессию в `waiting` из любого состояния: контейнер пересоздан и начинает с ожидания первого ответа.
    pub(in crate::server) fn reset_session_state(&mut self, fingerprint: &Fingerprint) -> Result<(), Error> {
        let updated = self.db.execute(
            "UPDATE sessions SET state = ?1 WHERE fingerprint = ?2",
            params![state_name(SessionState::Waiting), fingerprint.as_str()],
        )?;
        if updated == 0 {
            return Err(Error::NotFound(Entity::Session));
        }
        Ok(())
    }

    // Назначения

    /// Назначает сессии свободный runner (в сети ли он, проверяет вызывающий). Новая пара получает новый номер
    /// назначения; прежняя пара сессии заменяется и возвращается. Повторное назначение того же runner ничего не
    /// меняет и возвращает его номер.
    pub(in crate::server) fn assign_runner(&mut self, session: &Fingerprint, runner: &Fingerprint) -> Result<Assigned, Error> {
        let tx = begin(&mut self.db)?;
        let session_info = find_session(&tx, session)?;
        let node = find_node(&tx, runner)?;
        if let (Some(current), Some(assignment)) = (&session_info.runner, session_info.assignment)
            && current == runner
        {
            return Ok(Assigned {
                assignment,
                changed: false,
                previous: None,
            });
        }
        if node.revoked {
            return Err(Error::Revoked);
        }
        if assigned_session(&tx, runner)?.is_some() {
            return Err(Error::RunnerBusy);
        }
        tx.execute("DELETE FROM assignments WHERE session = ?1", [session.as_str()])?;
        tx.execute("UPDATE assignment_counter SET last = last + 1", [])?;
        let epoch: i64 = tx.query_row("SELECT last FROM assignment_counter", [], |row| row.get(0))?;
        tx.execute(
            "INSERT INTO assignments (session, runner, epoch) VALUES (?1, ?2, ?3)",
            params![session.as_str(), runner.as_str(), epoch],
        )?;
        tx.execute(
            "UPDATE nodes SET last_epoch = ?1 WHERE fingerprint = ?2",
            params![epoch, runner.as_str()],
        )?;
        // Новая пара поднимает агента: остановка прежней пары на неё не переходит.
        tx.execute("UPDATE sessions SET agent_stopped = 0 WHERE fingerprint = ?1", [session.as_str()])?;
        tx.commit()?;
        let previous = session_info.runner.zip(session_info.assignment);
        Ok(Assigned {
            assignment: Assignment(epoch.cast_unsigned()),
            changed: true,
            previous,
        })
    }

    /// Отмечает, что агент остановлен и ждёт явного назначения (`true`) или что он снова работает (`false`).
    pub(in crate::server) fn set_agent_stopped(&mut self, session: &Fingerprint, stopped: bool) -> Result<(), Error> {
        let updated = self.db.execute(
            "UPDATE sessions SET agent_stopped = ?1 WHERE fingerprint = ?2",
            params![stopped, session.as_str()],
        )?;
        if updated == 0 {
            return Err(Error::NotFound(Entity::Session));
        }
        Ok(())
    }

    /// Снимает назначение сессии; без назначения ничего не делает.
    pub(in crate::server) fn release_runner(&mut self, session: &Fingerprint) -> Result<(), Error> {
        let tx = begin(&mut self.db)?;
        find_session(&tx, session)?;
        tx.execute("DELETE FROM assignments WHERE session = ?1", [session.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    /// Сессия, в которую пускают waldo worker: он называет её сам, и она должна быть уже связана с runner.
    pub(in crate::server) fn worker_destination(&self, requested: &str) -> Result<Fingerprint, Error> {
        let requested = Fingerprint::from_str(requested).map_err(|_| Error::NotFound(Entity::Session))?;
        let session = find_session(&self.db, &requested)?;
        if session.runner.is_none() {
            return Err(Error::NotLinked);
        }
        Ok(session.fingerprint)
    }
}
