//! Действия session над собой: то, что ей доступно из MCP-инструментов агента. Сессия названа вызывающим,
//! чужую действия не затрагивают.

use std::sync::PoisonError;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Activity, CloneRequest, CloneResponse, Error, Status};

use super::{Control, registry_error, runner};

impl Control {
    /// Состояние `session`: то, что она сообщила последним; без сообщений — состояние и runner из реестра.
    pub(in crate::server) fn session_status(&self, session: &Fingerprint) -> Result<Status, Error> {
        let info = self.registry().session(session).map_err(|e| registry_error("session", &e))?;
        let reported = self
            .sessions
            .get(session)
            .and_then(|(_, link)| link.status.lock().unwrap_or_else(PoisonError::into_inner).clone());
        // Назначение меняет server, а сообщение session могло выйти до этого: runner берётся из реестра.
        Ok(Status {
            runner: info.runner,
            ..reported.unwrap_or_else(|| Status {
                state: info.state,
                reason: None,
                cause: None,
                limit_reset: None,
                runner: None,
                activity: Activity::default(),
                agent_running: false,
                limits: Vec::new(),
                restart_pending: false,
            })
        })
    }

    /// Клон runner, с которым работает `session`, в каталоге `dir`.
    pub(in crate::server) async fn clone_current_runner(&self, session: &Fingerprint, dir: String) -> CloneResponse {
        let current = self.registry().session(session).map_err(|e| registry_error("session", &e))?.runner;
        let Some(current) = current else {
            return Err(Error::Io("runner не назначен".to_owned()));
        };
        runner::clone_runner(self, &current, &CloneRequest { dir }).await
    }
}
