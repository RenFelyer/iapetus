//! Запросы orbit session на внутреннем слушателе: состояние и документы; указание server — назначение runner.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use nerve::fingerprint::Fingerprint;
use nerve::mux::{Incoming, Mux, Requests};
use nerve::protocol::{self, AgentRelease, AgentRun, AssignedRunner, Control as ToSession, Error, McpCatalog, SessionRequest, Status};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::env::Env;
use super::reassign;
use super::{Control, NODE_TIMEOUT, SessionLink, call, decode, registry_error, reply};

/// Канал управления session. Пока он открыт, jump ведёт к её серверу данных по адресу `ip`.
pub(super) async fn serve(control: &Arc<Control>, session: Fingerprint, ip: IpAddr, mux: &Mux, mut requests: Requests) {
    let link = Arc::new(SessionLink {
        ip,
        status: Mutex::default(),
        agent_running: watch::channel(false).0,
    });
    let online = control.sessions.join(session.clone(), mux.clone(), Arc::clone(&link));
    control.sessions_changed.send_replace(());
    tracing::info!(%session, "session в сети");
    tokio::spawn(announce_runner(Arc::clone(control), session.clone()));
    let updates = tokio::spawn(follow_agent_updates(Arc::clone(control), session.clone()));
    let watcher = tokio::spawn(reassign::watch_runner_absence(Arc::clone(control), session.clone()));

    let mut handlers = JoinSet::new();
    let reason = loop {
        tokio::select! {
            incoming = requests.recv() => {
                let Some(incoming) = incoming else { break "поток запросов закончился".to_owned() };
                let Some(request) = decode::<SessionRequest>(&incoming, "session") else { break "запрос не разобран".to_owned() };
                handlers.spawn(handle(Arc::clone(control), session.clone(), Arc::clone(&link), request, incoming));
            }
            reason = mux.closed() => break reason.to_string(),
            Some(_) = handlers.join_next() => {}
        }
    };
    tracing::info!(%session, %reason, "канал управления session закрыт");
    watcher.abort();
    updates.abort();
    drop(online);
    control.sessions_changed.send_replace(());
}

impl Control {
    /// Предлагает session обновить агента до `release`; когда обновлять, решает она. Не в сети — ошибка.
    async fn update_agent(&self, session: &Fingerprint, release: &AgentRelease) -> Result<(), Error> {
        let Some((mux, _)) = self.sessions.get(session) else {
            return Err(Error::Io("session не в сети".to_owned()));
        };
        call::<_, Result<(), Error>>(&mux, &ToSession::UpdateAgent(release.clone()), NODE_TIMEOUT)
            .await
            .map_err(|error| Error::Io(format!("указание не доставлено: {error:#}")))?
    }

    /// Передаёт session сводку MCP-серверов runner; пустая снимает прежнюю. Не в сети — ошибка: сводка дойдёт при
    /// подключении.
    pub(super) async fn send_catalog(&self, session: &Fingerprint, catalog: McpCatalog) -> Result<(), Error> {
        let Some((mux, _)) = self.sessions.get(session) else {
            return Err(Error::Io("session не в сети".to_owned()));
        };
        call::<_, Result<(), Error>>(&mux, &ToSession::McpCatalog(catalog), NODE_TIMEOUT)
            .await
            .map_err(|error| Error::Io(format!("указание не доставлено: {error:#}")))?
    }

    /// Сообщает session, что сводки MCP у неё нет: пара кончилась. Отказ остаётся в журнале.
    pub(super) async fn clear_catalog(&self, session: &Fingerprint) {
        if self.sessions.get(session).is_none() {
            return;
        }
        if let Err(error) = self.send_catalog(session, McpCatalog::default()).await {
            tracing::warn!(%session, %error, "session не приняла снятие сводки MCP");
        }
    }

    /// Просит session перезапустить агента с продолжением разговора: первое указание ждёт конца хода, повторное,
    /// пока оно ждёт, перезапускает сразу. Не в сети — ошибка: указание не ждёт подключения.
    pub(super) async fn resume_agent(&self, session: &Fingerprint) -> Result<(), Error> {
        let Some((mux, _)) = self.sessions.get(session) else {
            return Err(Error::Io("session не в сети".to_owned()));
        };
        call::<_, Result<(), Error>>(&mux, &ToSession::Resume, NODE_TIMEOUT)
            .await
            .map_err(|error| Error::Io(format!("указание не доставлено: {error:#}")))?
    }
}

/// Передаёт session предложенную версию её агента: ту, что есть к подключению, и каждую следующую. Версии других
/// агентов она не получает; агент без источника выпусков не предлагает ничего. Не принявшая session получит
/// предложение снова при следующем подключении.
async fn follow_agent_updates(control: Arc<Control>, session: Fingerprint) {
    let agent = match control.registry().session(&session) {
        Ok(info) => info.agent,
        Err(error) => return tracing::warn!(%session, %error, "агент session не прочитан, обновление агента не передано"),
    };
    let Some(mut releases) = control.agent_updates(agent).map(|releases| releases.subscribe()) else {
        return;
    };
    let agent = agent.id();
    loop {
        let release = releases.borrow_and_update().clone();
        if let Some(release) = release {
            match control.update_agent(&session, &release).await {
                Ok(()) => tracing::info!(%session, agent, version = %release.version, "session приняла обновление агента"),
                Err(Error::Io(reason)) if reason == nerve::release::NO_AGENT_COMMAND => {
                    tracing::debug!(%session, agent, version = %release.version, "session не запускает агента, обновление агента не принято");
                }
                Err(error) => tracing::warn!(%session, agent, version = %release.version, %error, "session не приняла обновление агента"),
            }
        }
        if releases.changed().await.is_err() {
            return;
        }
    }
}

/// После каждого подключения server сообщает session текущее назначение (`None` — runner не назначен) и не велит
/// ей трогать агента, кроме остановленного или оставшегося без runner: он остаётся остановленным, пока не придёт
/// явное назначение.
pub(super) async fn announce_runner(control: Arc<Control>, session: Fingerprint) {
    let announced = assigned_runner(&control, &session).and_then(|assigned| {
        let stopped = control
            .registry()
            .session(&session)
            .map_err(|e| registry_error("session", &e))?
            .agent_stopped;
        let agent = if assigned.is_some() && !stopped {
            AgentRun::Keep
        } else {
            AgentRun::Stop
        };
        Ok((assigned, agent))
    });
    match announced {
        Ok((assigned, agent)) => {
            let catalog = assigned
                .as_ref()
                .and_then(|assigned| control.catalogs().get(&assigned.runner).cloned());
            Arc::clone(&control).assign_runner(session.clone(), assigned, agent).await;
            if let Some(catalog) = catalog
                && let Err(error) = control.send_catalog(&session, catalog).await
            {
                tracing::warn!(%session, %error, "session не приняла сводку MCP");
            }
        }
        Err(error) => tracing::warn!(%session, %error, "назначение session не прочитано"),
    }
}

/// Назначение session со сведениями о runner; `None` — runner не назначен.
pub(super) fn assigned_runner(control: &Control, session: &Fingerprint) -> Result<Option<AssignedRunner>, Error> {
    let info = control.registry().session(session).map_err(|e| registry_error("session", &e))?;
    let (Some(runner), Some(assignment)) = (info.runner, info.assignment) else {
        return Ok(None);
    };
    let node = control.registry().node(&runner).map_err(|e| registry_error("runner", &e))?;
    let env_names = Env::names(&control.registry, &runner)?;
    let host = control
        .hosts()
        .get(&runner)
        .cloned()
        .ok_or_else(|| Error::Io("runner ещё не сообщил своё окружение".to_owned()))?;
    Ok(Some(AssignedRunner {
        runner,
        assignment,
        name: node.name,
        os: node.os,
        host,
        env_names,
    }))
}

async fn handle(control: Arc<Control>, session: Fingerprint, link: Arc<SessionLink>, request: SessionRequest, incoming: Incoming) {
    match request {
        SessionRequest::Hello(status) | SessionRequest::Status(status) => {
            reply(&incoming, &record_status(&control, &session, &link, &status)).await
        }
        SessionRequest::Docs => {
            stream_docs(&control, &incoming).await;
            false
        }
    };
}

/// Подписка session на документы: снимок, затем изменения, пока канал открыт.
async fn stream_docs(control: &Control, incoming: &Incoming) {
    let mut subscription = control.docs.subscribe();
    loop {
        let event = subscription.next().await;
        let Ok(body) = protocol::encode(&event) else {
            return;
        };
        if incoming.respond(body.into(), false).await.is_err() {
            return;
        }
    }
}

/// Пишет в журнал смену состояния session с причиной и запуск или остановку её агента.
fn log_status_change(session: &Fingerprint, previous: Option<&Status>, status: &Status) {
    let state_changed = previous.is_none_or(|previous| previous.state != status.state || previous.reason != status.reason);
    if state_changed {
        tracing::info!(%session, state = ?status.state, reason = status.reason.as_deref().unwrap_or("—"), cause = ?status.cause, "состояние session");
    }
    if previous.is_some_and(|previous| previous.agent_running != status.agent_running) {
        let event = if status.agent_running {
            "агент session запущен"
        } else {
            "агент session остановлен"
        };
        tracing::info!(%session, "{event}");
    }
}

/// Записывает состояние session в реестр, а запущен ли агент — в память: по нему jump решает, пускать ли worker.
fn record_status(control: &Control, session: &Fingerprint, link: &SessionLink, status: &Status) -> Result<(), Error> {
    let previous = link.status.lock().unwrap_or_else(PoisonError::into_inner).replace(status.clone());
    log_status_change(session, previous.as_ref(), status);
    link.agent_running.send_replace(status.agent_running);
    control.sessions_changed.send_replace(());
    control
        .registry()
        .set_session_state(session, status.state)
        .map_err(|e| registry_error("состояние session", &e))
}
