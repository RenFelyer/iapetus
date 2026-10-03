//! Смена runner у сессии (`Runners.Assign` от worker, `runner_use` агента через HTTP MCP server) и остановка
//! агента, когда runner не вернулся.

use std::sync::Arc;
use std::time::Duration;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{AgentRun, AssignedRunner, Assignment, Control as ToSession, Error};
use tokio::time::Instant;

use super::{Control, NODE_TIMEOUT, call, registry_error, runner, session};

/// Сколько runner может отсутствовать, прежде чем агент остановится и будет ждать нового назначения.
const RUNNER_ABSENCE_LIMIT: Duration = Duration::from_mins(5);

/// Как часто проверяется, на месте ли runner. Остановка наступает не позже, чем через этот срок после лимита.
const ABSENCE_CHECK: Duration = Duration::from_secs(5);

/// Что сделала запись назначения.
pub(super) enum Pair {
    /// Тот же runner: ничего не изменилось.
    Same,
    /// Новая пара; `old` — runner прежней пары сессии, если она была.
    New { old: Option<Fingerprint> },
}

impl Control {
    /// Проверки назначения runner, до любых действий с Docker: runner есть, не отозван, в сети и свободен.
    /// Не прошло — отказ тому, кто выбирал; ничего не изменилось.
    pub(super) fn check_assignable(&self, runner: &Fingerprint) -> Result<(), Error> {
        let info = self.registry().runner(runner).map_err(|e| registry_error("runner", &e))?;
        if info.node.revoked {
            return Err(Error::Access);
        }
        if self.runners.get(runner).is_none() {
            return Err(Error::Io("runner не в сети".to_owned()));
        }
        if info.session.is_some() || self.ending().contains(runner) {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Назначает session runner `runner` и велит ей перезапустить агента (`resume` — с продолжением разговора).
    /// Docker не трогается: контейнер живёт, session переключается сама.
    ///
    /// Сначала проверки ([`Self::check_assignable`]) и присутствие session: указание не ждёт подключения. Тот же
    /// runner при работающем агенте — не изменение, при остановленном — агент поднимается заново. Новая пара
    /// записывается в реестр с новым номером, затем session получает указание. Не принято — прежняя пара
    /// возвращается, ни прежний, ни новый runner ничего не узнают. Принято — прежнему runner уходит `End` его номера (он свободен после
    /// ответа), новому — `Start`.
    pub(in crate::server) async fn reassign(
        self: &Arc<Self>,
        session: &Fingerprint,
        runner: &Fingerprint,
        resume: bool,
    ) -> Result<(), Error> {
        let info = self.registry().session(session).map_err(|e| registry_error("session", &e))?;
        if info.runner.as_ref() == Some(runner) {
            if !info.agent_stopped {
                return Ok(());
            }
            return self.restart_stopped(session, runner, resume).await;
        }
        self.check_assignable(runner)?;
        self.require_online(session)?;
        let Pair::New { old } = self.assign_pair(session, runner)? else {
            return Ok(());
        };
        let delivered = async {
            let assigned = session::assigned_runner(self, session)?;
            self.send_assignment(session, assigned, AgentRun::Restart { resume }).await
        };
        if let Err(error) = delivered.await {
            self.undo_pair(session, old.as_ref(), info.agent_stopped);
            return Err(error);
        }
        self.clear_catalog(session).await;
        if let Some(old) = old {
            self.forget_catalog(&old);
            self.release_tunnels(&old);
            if let Some((mux, ())) = self.runners.get(&old) {
                runner::announce_session(Arc::clone(self), old, mux).await;
            }
        }
        if let Some((mux, ())) = self.runners.get(runner) {
            runner::announce_session(Arc::clone(self), runner.clone(), mux).await;
        }
        Ok(())
    }

    /// Остановленный агент поднимается на прежнем runner: пара не меняется, признак остановки снимается.
    async fn restart_stopped(&self, session: &Fingerprint, runner: &Fingerprint, resume: bool) -> Result<(), Error> {
        self.require_online(session)?;
        if self.runners.get(runner).is_none() {
            return Err(Error::Io("runner не в сети".to_owned()));
        }
        self.registry()
            .set_agent_stopped(session, false)
            .map_err(|e| registry_error("session", &e))?;
        let delivered = async {
            let assigned = session::assigned_runner(self, session)?;
            self.send_assignment(session, assigned, AgentRun::Restart { resume }).await
        };
        if let Err(error) = delivered.await {
            drop(self.registry().set_agent_stopped(session, true));
            return Err(error);
        }
        Ok(())
    }

    fn require_online(&self, session: &Fingerprint) -> Result<(), Error> {
        if self.sessions.get(session).is_none() {
            return Err(Error::Io("session не в сети".to_owned()));
        }
        Ok(())
    }

    /// Указание session: назначение и что делать с агентом. Не в сети или не приняла — ошибка.
    pub(super) async fn send_assignment(
        &self,
        session: &Fingerprint,
        runner: Option<AssignedRunner>,
        agent: AgentRun,
    ) -> Result<(), Error> {
        let Some((mux, _)) = self.sessions.get(session) else {
            return Err(Error::Io("session не в сети".to_owned()));
        };
        match call::<_, Result<(), Error>>(&mux, &ToSession::AssignRunner { runner, agent }, NODE_TIMEOUT).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%session, error = format!("{error:#}"), "назначение не доставлено session");
                Err(Error::Io("указание не доставлено session".to_owned()))
            }
        }
    }

    /// Сообщает session назначение; не в сети — ничего: назначение дойдёт при подключении. Отказ session остаётся в
    /// журнале.
    pub(super) async fn assign_runner(self: Arc<Self>, session: Fingerprint, runner: Option<AssignedRunner>, agent: AgentRun) {
        if self.sessions.get(&session).is_none() {
            return;
        }
        if let Err(error) = self.send_assignment(&session, runner, agent).await {
            tracing::warn!(%session, %error, "session не приняла назначение");
        }
    }

    /// Возвращает прежнюю пару и признак остановки, если session не приняла новую. Прежний runner сразу свободен: `End`
    /// ему ещё не уходил.
    fn undo_pair(&self, session: &Fingerprint, old: Option<&Fingerprint>, stopped: bool) {
        let mut ending = self.ending();
        let mut registry = self.registry();
        let restored = match old {
            Some(old) => registry.assign_runner(session, old).map(drop),
            None => registry.release_runner(session),
        };
        if let Err(error) = restored.and_then(|()| registry.set_agent_stopped(session, stopped)) {
            tracing::error!(%session, %error, "прежняя пара session не возвращена");
        }
        if let Some(old) = old {
            ending.remove(old);
        }
    }

    /// Записывает новую пару. Прежний runner попадает в занятые под тем же замком, под которым проверяется новый: его нельзя выбрать,
    /// пока он не подтвердил `End`.
    pub(super) fn assign_pair(&self, session: &Fingerprint, runner: &Fingerprint) -> Result<Pair, Error> {
        let mut ending = self.ending();
        if ending.contains(runner) {
            return Err(Error::Conflict);
        }
        let mut registry = self.registry();
        registry.active_node(runner).map_err(|e| registry_error("назначение", &e))?;
        let assigned = registry
            .assign_runner(session, runner)
            .map_err(|e| registry_error("назначение", &e))?;
        drop(registry);
        if !assigned.changed {
            return Ok(Pair::Same);
        }
        let old = assigned.previous.map(|(old, _)| old);
        if let Some(old) = &old {
            ending.insert(old.clone());
        }
        Ok(Pair::New { old })
    }
}

/// Следит за runner сессии, пока открыт её канал управления: не в сети дольше [`RUNNER_ABSENCE_LIMIT`] подряд —
/// агент останавливается и ждёт назначения. Сначала в реестре отмечается остановка, затем указание идёт session.
/// Пара не снимается, runner остаётся занятым. Отсчёт идёт с момента, когда отсутствие замечено (после перезапуска
/// server — с подключения session); обрыв короче лимита и смена пары его сбрасывают.
///
/// Присутствие runner видно по его каналу управления, а решает server, поэтому остановка переживает
/// переподключение session и перезапуск server.
pub(super) async fn watch_runner_absence(control: Arc<Control>, session: Fingerprint) {
    let mut ticker = tokio::time::interval(ABSENCE_CHECK);
    let mut absent: Option<(Assignment, Instant)> = None;
    loop {
        ticker.tick().await;
        let Ok(info) = control.registry().session(&session) else {
            return;
        };
        let away = match (&info.runner, info.assignment) {
            (Some(runner), Some(assignment)) if !info.agent_stopped && !control.runners.is_online(runner) => Some(assignment),
            _ => None,
        };
        let Some(assignment) = away else {
            absent = None;
            continue;
        };
        let since = match absent {
            Some((counted, since)) if counted == assignment => since,
            _ => absent.insert((assignment, Instant::now())).1,
        };
        if since.elapsed() < RUNNER_ABSENCE_LIMIT {
            continue;
        }
        absent = None;
        if let Err(error) = stop_agent(&control, &session, assignment).await {
            tracing::warn!(%session, %error, "агент без runner не остановлен");
        }
    }
}

/// Останавливает агента сессии, пара которой `assignment` осталась без runner. Если пара за это время сменилась или
/// агент уже остановлен, ничего не меняется: проверка и запись признака идут под одним замком реестра.
async fn stop_agent(control: &Arc<Control>, session: &Fingerprint, assignment: Assignment) -> Result<(), Error> {
    {
        let mut registry = control.registry();
        let info = registry.session(session).map_err(|e| registry_error("session", &e))?;
        if info.assignment != Some(assignment) || info.agent_stopped {
            return Ok(());
        }
        registry
            .set_agent_stopped(session, true)
            .map_err(|e| registry_error("session", &e))?;
    }
    tracing::info!(%session, "runner не вернулся за отведённое время, агент останавливается");
    let assigned = session::assigned_runner(control, session)?;
    control.send_assignment(session, assigned, AgentRun::Stop).await
}
