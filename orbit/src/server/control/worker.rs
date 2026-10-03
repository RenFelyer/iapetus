//! Запросы waldo worker: сертификат SSH, новый `orbit_key`, runner (список, клон, назначение, отзыв), окружение
//! команд и сессии (запуск, завершение, перезапуск).

use std::pin::pin;
use std::sync::{Arc, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use nerve::fingerprint::Fingerprint;
use nerve::mux::{Incoming, Mux, Requests};
use nerve::protocol::{
    Agent, EnvOp, Error, NodeInfo, NodesOp, RunnersOp, SessionEvent, SessionIdentity, SessionInfo, SessionsOp, WorkerCertificateRequest,
    WorkerCertificateResponse, WorkerRequest,
};
use tokio::task::JoinSet;

use super::env::Env;
use super::{Control, decode, node_error, registry_error, reply, reply_open, runner, superseded, unsupported, until, valid_before};
use crate::server::enroll;
use crate::server::registry;

pub(super) fn certificate(control: &Control, request: &WorkerCertificateRequest) -> WorkerCertificateResponse {
    let reply = enroll::worker_certificate(&control.registry(), &control.ca, request, SystemTime::now());
    let reply = reply.map_err(|e| node_error("сертификат worker", &e))?;
    control.forget_first_orbit_key();
    Ok(reply)
}

/// Выдаёт новый `orbit_key`: прежний перестаёт действовать. Возвращает секрет и время выдачи (секунды Unix).
fn new_orbit_key(control: &Control) -> Result<(String, u64), Error> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let mut registry = control.registry();
    let key = registry
        .issue_orbit_key(i64::try_from(now).unwrap_or(i64::MAX))
        .map_err(|e| registry_error("orbit_key", &e))?;
    let issued = (*registry.orbit_key_issued().borrow())
        .and_then(|at| u64::try_from(at).ok())
        .unwrap_or(now);
    drop(registry);
    control.forget_first_orbit_key();
    Ok((key.expose().to_owned(), issued))
}

/// Канал управления worker. Закрывается вместе с сертификатом, если worker его не продлил, и новым `orbit_key`,
/// выданным позже сертификата. Исключение — соединение, по которому выдан новый ключ: ответ с ним должен до
/// worker дойти, поэтому оно становится соединением нового ключа.
pub(super) async fn serve(control: &Arc<Control>, mut issued: u64, valid_before_unix: u64, mux: &Mux, mut requests: Requests) {
    let orbit_key = control.registry().orbit_key_issued();
    let mut deadline = pin!(tokio::time::sleep(until(valid_before_unix)));
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            incoming = requests.recv() => {
                let Some(incoming) = incoming else { break };
                let Some(request) = decode::<WorkerRequest>(&incoming, "worker") else { break };
                let answered = match request {
                    WorkerRequest::WorkerCertificate(request) => {
                        let response = certificate(control, &request);
                        if let Some(until_unix) = response.as_ref().ok().and_then(|reply| valid_before(&reply.certificate)) {
                            deadline.as_mut().reset((Instant::now() + until(until_unix)).into());
                        }
                        reply(&incoming, &response).await
                    }
                    WorkerRequest::Nodes(NodesOp::NewOrbitKey) => {
                        let response = new_orbit_key(control);
                        if let Ok((_, at)) = &response {
                            issued = issued.max(*at);
                        }
                        reply(&incoming, &response.map(|(key, _)| key)).await
                    }
                    request => {
                        handlers.spawn(handle(Arc::clone(control), request, incoming));
                        true
                    }
                };
                if !answered {
                    break;
                }
            }
            () = &mut deadline => {
                tracing::info!("сертификат worker истёк, канал управления закрыт");
                break;
            }
            () = superseded(orbit_key.clone(), issued) => {
                tracing::info!("выдан новый orbit_key, канал управления worker закрыт");
                break;
            }
            reason = mux.closed() => {
                tracing::info!(%reason, "канал управления worker закрыт");
                break;
            }
            Some(_) = handlers.join_next() => {}
        }
    }
}

async fn handle(control: Arc<Control>, request: WorkerRequest, incoming: Incoming) {
    match request {
        WorkerRequest::Runners(RunnersOp::List) => reply(&incoming, &control.runner_list()).await,
        WorkerRequest::Runners(RunnersOp::Clone(clone)) => {
            reply(&incoming, &runner::clone_runner(&control, &clone.runner, &clone.request).await).await
        }
        WorkerRequest::Runners(RunnersOp::Rename { runner, name }) => {
            let renamed = control.registry().rename_node(&runner, &name);
            reply(&incoming, &renamed.map_err(|e| registry_error("переименование", &e))).await
        }
        WorkerRequest::Nodes(NodesOp::List) => reply(&incoming, &node_list(&control)).await,
        WorkerRequest::Nodes(NodesOp::Revoke(runner)) => reply(&incoming, &control.revoke_runner(&runner)).await,
        WorkerRequest::Env(EnvOp::List) => reply(&incoming, &Env::list(&control.registry)).await,
        WorkerRequest::Env(EnvOp::Change(change)) => reply(&incoming, &control.env.change(&control.registry, &change)).await,
        WorkerRequest::Sessions(SessionsOp::List) => reply(&incoming, &session_list(&control)).await,
        WorkerRequest::Sessions(SessionsOp::Start { agent, runner, identity }) => {
            start_session(&control, agent, runner, &identity, &incoming).await
        }
        WorkerRequest::Sessions(SessionsOp::End(session)) => reply(&incoming, &end_session(&control, &session).await).await,
        WorkerRequest::Runners(RunnersOp::Assign { session, runner, resume }) => {
            reply(&incoming, &control.reassign(&session, &runner, resume).await).await
        }
        WorkerRequest::Sessions(SessionsOp::Restart(session)) => reply(&incoming, &restart_session(&control, &session).await).await,
        WorkerRequest::Sessions(SessionsOp::RestartAgent(session)) => reply(&incoming, &control.resume_agent(&session).await).await,
        // Сертификат и новый `orbit_key` обслуживает цикл соединения: они меняют срок самого соединения.
        WorkerRequest::WorkerCertificate(_) | WorkerRequest::Nodes(NodesOp::NewOrbitKey) => {
            reply(&incoming, &Err::<(), Error>(unsupported())).await
        }
    };
}

/// `Sessions.Start`: новая сессия в реестре, runner назначен, контейнер поднят; дальше — поток событий. Если
/// поднять не удалось, сессия удаляется, а единственный ответ — `Refused` с причиной.
async fn start_session(control: &Arc<Control>, agent: Agent, runner: Fingerprint, identity: &SessionIdentity, incoming: &Incoming) -> bool {
    let Some(lifecycle) = &control.lifecycle else {
        return reply(incoming, &Err::<(), Error>(unsupported())).await;
    };
    if let Err(error) = control.check_assignable(&runner) {
        tracing::warn!(%runner, %error, "runner не подходит для сессии");
        return refuse(incoming, runner_refusal(&error)).await;
    }
    let session = match prepare_session(control, agent, &runner, identity) {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, "сессия не создана");
            return refuse(incoming, "сессия не создана").await;
        }
    };
    if let Err(error) = lifecycle.start(&session, agent, &runner, None).await {
        tracing::warn!(%session, error = format!("{error:#}"), "контейнер сессии не поднят");
        drop(control.registry().remove_session(&session));
        control.forget_catalog(&runner);
        return refuse(incoming, "контейнер не поднялся").await;
    }
    announce_session(control, &runner);
    stream_start(control, &session, incoming).await;
    true
}

async fn refuse(incoming: &Incoming, reason: &str) -> bool {
    reply(incoming, &SessionEvent::Refused(reason.to_owned())).await
}

/// Причина отказа назначить runner для worker: без внутренностей реестра.
fn runner_refusal(error: &Error) -> &'static str {
    match error {
        Error::NotFound => "runner не найден",
        Error::Access => "runner отозван",
        Error::Conflict => "runner занят",
        Error::Io(_) => "runner не в сети",
        _ => "runner не подходит",
    }
}

fn prepare_session(
    control: &Control,
    agent: Agent,
    runner: &Fingerprint,
    identity: &SessionIdentity,
) -> Result<Fingerprint, registry::Error> {
    let mut registry = control.registry();
    let session = registry.create_session(agent)?;
    let recorded = registry
        .set_session_identity(&session, identity)
        .and_then(|()| registry.assign_runner(&session, runner).map(drop));
    if let Err(error) = recorded {
        drop(registry.remove_session(&session));
        return Err(error);
    }
    Ok(session)
}

/// `Sessions.End`: контейнер, том и запись в реестре удаляются; подключённый runner узнаёт, что сессии нет.
async fn end_session(control: &Arc<Control>, session: &Fingerprint) -> Result<(), Error> {
    let Some(lifecycle) = &control.lifecycle else {
        return Err(unsupported());
    };
    tracing::info!(%session, "worker просит завершить сессию");
    let runner = control
        .registry()
        .session(session)
        .map_err(|e| registry_error("сессия", &e))?
        .runner;
    lifecycle.remove(session).await.map_err(|error| {
        tracing::warn!(%session, error = format!("{error:#}"), "сессия не завершена");
        Error::Io("внутренняя ошибка server".to_owned())
    })?;
    if let Some(runner) = runner {
        control.forget_catalog(&runner);
        control.release_tunnels(&runner);
        announce_session(control, &runner);
    }
    tracing::info!(%session, "сессия завершена: контейнер, том и запись в реестре удалены");
    Ok(())
}

/// `Sessions.Restart`: сессия из `failure` получает новый контейнер на том же томе и снова ждёт входа.
async fn restart_session(control: &Control, session: &Fingerprint) -> Result<(), Error> {
    let Some(lifecycle) = &control.lifecycle else {
        return Err(unsupported());
    };
    control.registry().session(session).map_err(|e| registry_error("сессия", &e))?;
    tracing::info!(%session, "worker просит перезапустить сессию из failure");
    lifecycle.restore_failed(session).await.map_err(|error| {
        tracing::warn!(%session, error = format!("{error:#}"), "сессия не перезапущена");
        Error::Io("сессия не перезапущена".to_owned())
    })
}

/// Сообщает подключённому runner, идёт ли для него сессия; не в сети — узнает при подключении.
fn announce_session(control: &Arc<Control>, runner: &Fingerprint) {
    if let Some((mux, ())) = control.runners.get(runner) {
        tokio::spawn(runner::announce_session(Arc::clone(control), runner.clone(), mux));
    }
}

/// Ответы на `Sessions.Start`: `Ready`, когда в session запущен агент, и `Ended`, когда она пропала из сети.
async fn stream_start(control: &Control, session: &Fingerprint, incoming: &Incoming) {
    control.wait_agent_running(session).await;
    if !reply_open(incoming, &SessionEvent::Ready(session.clone())).await {
        return;
    }
    control.wait_session_gone(session).await;
    reply(incoming, &SessionEvent::Ended).await;
}

fn session_list(control: &Control) -> Result<Vec<SessionInfo>, Error> {
    let sessions = control.registry().sessions().map_err(|e| registry_error("сессии", &e))?;
    let mut list = Vec::with_capacity(sessions.len());
    for session in sessions {
        let status = control
            .sessions
            .get(&session.fingerprint)
            .and_then(|(_, link)| link.status.lock().unwrap_or_else(PoisonError::into_inner).clone());
        list.push(SessionInfo {
            agent: session.agent,
            state: session.state,
            reason: status.as_ref().and_then(|status| status.reason.clone()),
            cause: status.as_ref().and_then(|status| status.cause),
            limit_reset: status.as_ref().and_then(|status| status.limit_reset),
            runner: session.runner,
            agent_running: control.agent_running(&session.fingerprint),
            restart_pending: status.as_ref().is_some_and(|status| status.restart_pending),
            limits: status.map(|status| status.limits).unwrap_or_default(),
            fingerprint: session.fingerprint,
        });
    }
    Ok(list)
}

fn node_list(control: &Control) -> Result<Vec<NodeInfo>, Error> {
    let runners = control.registry().runners().map_err(|e| registry_error("узлы", &e))?;
    Ok(runners
        .iter()
        .map(|runner| NodeInfo {
            runner: control.runner_info(runner),
            os: runner.node.os.clone(),
            revoked: runner.node.revoked,
        })
        .collect())
}
