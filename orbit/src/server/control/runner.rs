//! Запросы runner: продление сертификата и подписка на окружение; указания server runner — клон и границы сессии.

use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use nerve::connect::server::ask;
use nerve::fingerprint::Fingerprint;
use nerve::mux::{Incoming, Mux, Requests};
use nerve::protocol::{
    self, CloneRequest, CloneResponse, EnvEvent, Error, MAX_MCP_CATALOG, MAX_OS_RELEASE, McpCatalog, RegisterRequest, RenewRequest,
    RunnerHost, RunnerRequest, SessionMessage, SessionResponse, ToRunner,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinSet;

use super::{Control, NODE_TIMEOUT, call, decode, node_error, registry_error, reply, reply_open, session, superseded, until, valid_before};
use crate::server::enroll;

/// Наибольшая длина пути и имени машины в окружении runner.
const MAX_HOST_FIELD: usize = 4096;

/// Сколько ждать клон: runner создаёт каталог и worktree.
const CLONE_TIMEOUT: Duration = Duration::from_secs(60);

/// Регистрирует runner и выдаёт сертификат. Файл первого `orbit_key` после этого удаляется.
pub(super) fn register(control: &Control, request: &RegisterRequest) -> protocol::RegisterResponse {
    let reply = enroll::register(&mut control.registry(), &control.ca, request, SystemTime::now());
    let reply = reply.map_err(|e| node_error("регистрация", &e))?;
    control.forget_first_orbit_key();
    Ok(reply)
}

pub(super) fn renew(control: &Control, request: &RenewRequest) -> protocol::RenewResponse {
    enroll::renew(&mut control.registry(), &control.ca, request, SystemTime::now()).map_err(|e| node_error("продление", &e))
}

/// Канал управления runner. Закрывается вместе с сертификатом, если runner его не продлил, с отзывом runner,
/// новым `orbit_key`, выданным позже сертификата, и когда тот же fingerprint подключился снова.
pub(super) async fn serve(
    control: &Arc<Control>,
    runner: Fingerprint,
    issued: u64,
    valid_before_unix: u64,
    mux: &Mux,
    mut requests: Requests,
) {
    let mut revoked = control.revocations();
    let orbit_key = control.registry().orbit_key_issued();
    if let Err(error) = control.registry().active_node(&runner) {
        tracing::warn!(%runner, %error, "runner не пущен в канал управления");
        return;
    }
    // Runner в сети с первого `Host`: назначение session всегда несёт его окружение.
    let mut online = None;

    let mut handlers = JoinSet::new();
    let mut deadline = pin!(tokio::time::sleep(until(valid_before_unix)));
    loop {
        tokio::select! {
            incoming = requests.recv() => {
                let Some(incoming) = incoming else { break };
                let Some(request) = decode::<RunnerRequest>(&incoming, "runner") else { break };
                match request {
                    RunnerRequest::Host(host) => {
                        let accepted = if online.is_some() {
                            Err(Error::Conflict)
                        } else {
                            check_host(&host)
                        };
                        if accepted.is_ok() {
                            online = Some(join(control, &runner, mux, &host));
                        }
                        if !reply(&incoming, &accepted).await {
                            break;
                        }
                    }
                    RunnerRequest::Renew(request) => {
                        let response = if request.fingerprint == runner {
                            renew(control, &request)
                        } else {
                            tracing::warn!(%runner, "runner просит сертификат другого узла");
                            Err(Error::Access)
                        };
                        if let Some(until_unix) = response.as_ref().ok().and_then(|reply| valid_before(&reply.certificate)) {
                            deadline.as_mut().reset((Instant::now() + until(until_unix)).into());
                        }
                        if !reply(&incoming, &response).await {
                            break;
                        }
                    }
                    RunnerRequest::Env => {
                        handlers.spawn(stream_env(Arc::clone(control), runner.clone(), mux.clone(), incoming));
                    }
                    RunnerRequest::Mcp(catalog) => {
                        handlers.spawn(receive_catalog(Arc::clone(control), runner.clone(), catalog, incoming));
                    }
                }
            }
            () = &mut deadline => {
                tracing::info!(%runner, "сертификат runner истёк, канал управления закрыт");
                break;
            }
            () = superseded(orbit_key.clone(), issued) => {
                tracing::info!(%runner, "выдан новый orbit_key, канал управления runner закрыт");
                break;
            }
            lost = revoked.recv() => match lost {
                Ok(fingerprint) if fingerprint != runner => {}
                Ok(_) => {
                    tracing::info!(%runner, "runner отозван, канал управления закрыт");
                    break;
                }
                // Пропущенные отзывы неизвестны: безопаснее закрыть, runner войдёт заново и пройдёт проверку.
                Err(RecvError::Lagged(_) | RecvError::Closed) => break,
            },
            reason = mux.closed() => {
                tracing::info!(%runner, %reason, "канал управления runner закрыт");
                break;
            }
            Some(_) = handlers.join_next() => {}
        }
    }
}

/// Окружение, которое session воспроизведёт у себя: размеры и вид полей проверяет server, session проверяет сама.
fn check_host(host: &RunnerHost) -> Result<(), Error> {
    if host.os_release.len() > MAX_OS_RELEASE {
        return Err(Error::Io("/etc/os-release слишком велик".to_owned()));
    }
    if host.project.len() > MAX_HOST_FIELD || host.hostname.len() > MAX_HOST_FIELD || host.home.len() > MAX_HOST_FIELD {
        return Err(Error::Io("поле окружения runner слишком длинное".to_owned()));
    }
    Ok(())
}

/// Запоминает окружение runner, ставит его в сеть и сообщает ему состояние сессии. Session, которой он назначен,
/// узнаёт об изменившемся окружении сразу.
fn join(control: &Arc<Control>, runner: &Fingerprint, mux: &Mux, host: &RunnerHost) -> super::links::Membership<()> {
    let changed = control.hosts().insert(runner.clone(), host.clone()).is_none_or(|old| old != *host);
    let online = control.runners.join(runner.clone(), mux.clone(), ());
    tracing::info!(%runner, "runner в сети");
    tokio::spawn(announce_session(Arc::clone(control), runner.clone(), mux.clone()));
    if changed {
        tokio::spawn(announce_host(Arc::clone(control), runner.clone()));
    }
    online
}

/// Сообщает session runner, что его окружение известно: после перезапуска server session могла подключиться раньше
/// runner и ждёт назначения; изменившееся окружение session тоже применяет. Окружение, которое server уже знал, не
/// повторяется: оно у session есть.
async fn announce_host(control: Arc<Control>, runner: Fingerprint) {
    let Ok(info) = control.registry().runner(&runner) else {
        return;
    };
    if let Some(session) = info.session {
        session::announce_runner(control, session).await;
    }
}

/// Принимает сводку MCP от runner и отвечает, как только она сохранена; session узнаёт о ней после ответа.
async fn receive_catalog(control: Arc<Control>, runner: Fingerprint, catalog: McpCatalog, incoming: Incoming) {
    let stored = control.store_catalog(&runner, &catalog);
    let response = stored.as_ref().map(|_| ()).map_err(Clone::clone);
    if !reply(&incoming, &response).await {
        return;
    }
    let Ok(session) = stored else {
        return;
    };
    if control.sessions.get(&session).is_none() {
        tracing::debug!(%runner, %session, "session ещё не в сети, она получит сводку MCP при подключении");
        return;
    }
    if let Err(error) = control.send_catalog(&session, catalog).await {
        tracing::warn!(%runner, %session, %error, "сводка MCP не доставлена session");
    }
}

impl Control {
    /// Запоминает сводку runner вместо прежней и возвращает его session. Пустая сводка тоже запоминается: по ней
    /// session, подключившаяся позже, узнаёт, что серверов у проекта нет, и не ждёт первую сводку. Нет пары — отказ:
    /// проверка и запись идут под одним замком реестра, конец пары между ними не вклинится.
    fn store_catalog(&self, runner: &Fingerprint, catalog: &McpCatalog) -> Result<Fingerprint, Error> {
        let size = protocol::encode(catalog).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_MCP_CATALOG {
            return Err(Error::Io("сводка MCP слишком велика".to_owned()));
        }
        let registry = self.registry();
        let info = registry.runner(runner).map_err(|e| registry_error("runner", &e))?;
        let session = info.session.ok_or(Error::Conflict)?;
        self.catalogs().insert(runner.clone(), catalog.clone());
        Ok(session)
    }

    /// Снимает сводку runner, если пары у него больше нет: runner, получивший новую пару, свою сводку сохраняет.
    pub(super) fn forget_catalog(&self, runner: &Fingerprint) {
        let registry = self.registry();
        if registry.runner(runner).is_ok_and(|info| info.session.is_some()) {
            return;
        }
        self.catalogs().remove(runner);
    }
}

/// Подписка runner на окружение: снимок положенных ему слоёв, затем изменения. Подписка кончилась (рассылка
/// закрыта или runner отстал) — соединение закрывается, и runner подписывается заново.
async fn stream_env(control: Arc<Control>, runner: Fingerprint, mux: Mux, incoming: Incoming) {
    let subscription = control.env.subscribe(&control.registry, &runner);
    let Ok(mut subscription) = subscription else {
        mux.close();
        return;
    };
    let snapshot = EnvEvent::Snapshot(std::mem::take(&mut subscription.snapshot));
    if !reply_open(&incoming, &snapshot).await {
        return;
    }
    while let Some(change) = subscription.next().await {
        if !reply_open(&incoming, &EnvEvent::Change(change)).await {
            return;
        }
    }
    mux.close();
}

/// После каждого подключения server сообщает runner, идёт ли для него сессия: `Start` с номером назначения, если
/// ему назначена session, иначе `End` последней пары. Очереди нет: состояние вычисляется заново при каждом
/// подключении. Пока runner не подтвердил `End`, он считается занятым.
pub(super) async fn announce_session(control: Arc<Control>, runner: Fingerprint, mux: Mux) {
    let info = match control.registry().runner(&runner) {
        Ok(info) => info,
        Err(error) => {
            tracing::warn!(%runner, %error, "состояние runner не прочитано");
            return;
        }
    };
    let ending = info.assignment.is_none();
    let message = match (info.assignment, &info.session) {
        (Some(assignment), Some(session)) => {
            let identity = match control.registry().session_identity(session) {
                Ok(identity) => identity,
                Err(error) => {
                    tracing::warn!(%runner, %error, "пользователь сессии не прочитан");
                    return;
                }
            };
            SessionMessage::Start {
                assignment,
                author: identity.author.unwrap_or_else(|| control.author.clone()),
                github_token: identity.github_token,
            }
        }
        _ => SessionMessage::End {
            assignment: info.last_assignment,
        },
    };
    if ending {
        control.ending().insert(runner.clone());
        control.forget_catalog(&runner);
    }
    match call::<_, SessionResponse>(&mux, &ToRunner::Session(message), NODE_TIMEOUT).await {
        Ok(Ok(())) if ending => {
            control.ending().remove(&runner);
            tracing::info!(%runner, assignment = ?info.last_assignment, "runner подтвердил конец сессии");
        }
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%runner, %error, "runner не принял состояние сессии"),
        Err(error) => tracing::warn!(%runner, error = format!("{error:#}"), "состояние сессии не доставлено runner"),
    }
}

/// Клон на runner. Не в сети — ошибка: клон не ставится в очередь.
pub(in crate::server) async fn clone_runner(control: &Control, runner: &Fingerprint, request: &CloneRequest) -> CloneResponse {
    let known = control.registry().active_node(runner);
    known.map_err(|e| registry_error("клон", &e))?;
    let Some((mux, ())) = control.runners.get(runner) else {
        return Err(Error::Io("runner не в сети".to_owned()));
    };
    let response = tokio::time::timeout(CLONE_TIMEOUT, ask(&mux, &ToRunner::Clone(request.clone()))).await;
    match response {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::warn!(%runner, error = format!("{error:#}"), "клон не получен от runner");
            Err(Error::Io("runner не ответил".to_owned()))
        }
        Err(_) => Err(Error::Io("runner не ответил на клон вовремя".to_owned())),
    }
}
