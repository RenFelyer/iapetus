//! Канал управления server: подсистема `iapetus-control` на SSH, внутри `nerve::mux`. Набор запросов задан
//! ролью соединения (вход узла, runner, worker, session): сообщение не из набора роли закрывает соединение. Здесь
//! состояние server для всех ролей — присутствие узлов в памяти, отзыв; в подмодулях — запросы каждой роли.
//!
//! Присутствие не хранится: runner и session в сети, пока открыт их канал управления; после запуска server
//! в сети никого нет, узлы подключаются сами.

mod actions;
pub(super) mod docs;
pub(super) mod enroll;
pub(super) mod env;
mod links;
mod reassign;
pub(super) mod runner;
pub(super) mod session;
pub(super) mod worker;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve::connect::server::ask;
use nerve::fingerprint::Fingerprint;
use nerve::mux::{Incoming, Mux};
use nerve::protocol::{self, Agent, AgentRelease, AgentRun, Author, Error, McpCatalog, RunnerHost, RunnerPresence, Status};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, watch};

use self::docs::Docs;
use self::env::Env;
use self::links::Links;
use super::enroll as certificates;
use super::lifecycle::Lifecycle;
use super::registry::{self, Registry};
use super::ssh::ca::CertificateAuthority;

/// Порт сервера данных в контейнере session: по адресу, с которого она подключилась к server, jump ведёт к нему.
const SESSION_DATA_PORT: u16 = 2222;

/// Сколько ждать ответа узла на указание server, кроме клона: тот создаётся дольше.
const NODE_TIMEOUT: Duration = Duration::from_secs(30);

/// Кто подключился к каналу управления; роль взята из входа, сообщения её не несут.
#[derive(Debug, Clone)]
pub(super) enum Peer {
    /// `iapetus-enroll`: один запрос входа и закрытие.
    Enroll,
    /// runner с сертификатом; `issued` — когда выдан сертификат, `valid_before` — его конец, секунды Unix.
    Runner {
        fingerprint: Fingerprint,
        issued: u64,
        valid_before: u64,
    },
    /// worker с сертификатом; `issued` — когда выдан сертификат, `valid_before` — его конец, секунды Unix.
    Worker { issued: u64, valid_before: u64 },
    /// session с внутреннего слушателя; `ip` — адрес, с которого она подключилась.
    Session { fingerprint: Fingerprint, ip: IpAddr },
}

/// Что server знает о session в сети.
#[derive(Debug)]
struct SessionLink {
    /// Адрес, с которого session подключилась.
    ip: IpAddr,
    /// Последние `Hello` или `Status`: причина состояния и лимиты для списка worker.
    status: Mutex<Option<Status>>,
    /// Агент запущен в pty: последнее значение `agent_running` из `Hello` или `Status`.
    agent_running: watch::Sender<bool>,
}

#[derive(Debug)]
pub(super) struct Control {
    registry: Arc<Mutex<Registry>>,
    ca: Arc<CertificateAuthority>,
    /// Файл первого `orbit_key`: удаляется после первого использования ключа.
    first_key: Option<PathBuf>,
    /// Автор коммитов на время сессии, который server сообщает runner в `Start`.
    author: Author,
    env: Env,
    docs: Docs,
    runners: Arc<Links<()>>,
    sessions: Arc<Links<Arc<SessionLink>>>,
    /// Сигнал: session подключилась, отключилась или сообщила состояние; ждущие перечитывают [`Self::agent_running`].
    sessions_changed: watch::Sender<()>,
    /// Отозванные runner: открытые каналы и туннели jump этого fingerprint закрываются.
    revoked: broadcast::Sender<Fingerprint>,
    /// Runner, у которых пара кончилась, но `End` ещё не подтверждён: пока он не подтверждён, runner занят и
    /// новую сессию не получает.
    ending: Mutex<HashSet<Fingerprint>>,
    /// Runner, потерявшие назначение: открытые туннели jump этого fingerprint закрываются.
    released: broadcast::Sender<Fingerprint>,
    /// Контейнеры session; без образа server их не поднимает.
    lifecycle: Option<Arc<Lifecycle>>,
    /// Последняя предложенная версия каждого агента: её получает каждая session этого агента при подключении и при
    /// выходе новой.
    agent_updates: Vec<(Agent, watch::Sender<Option<AgentRelease>>)>,
    /// Сводка проектных MCP-серверов каждого runner, у которого есть пара. Только в памяти: после перезапуска server
    /// runner присылает сводку заново.
    catalogs: Mutex<HashMap<Fingerprint, McpCatalog>>,
    /// Окружение каждого runner, как он сообщил его при последнем подключении. Остаётся и когда runner не в сети:
    /// session получает его в назначении, в том числе при остановке агента без runner.
    hosts: Mutex<HashMap<Fingerprint, RunnerHost>>,
}

impl Control {
    pub(super) fn new(
        registry: Arc<Mutex<Registry>>,
        ca: Arc<CertificateAuthority>,
        first_key: Option<PathBuf>,
        author: Author,
        docs_dir: PathBuf,
    ) -> Self {
        Self {
            registry,
            ca,
            first_key,
            author,
            env: Env::new(),
            docs: Docs::new(docs_dir),
            runners: Arc::new(Links::new()),
            sessions: Arc::new(Links::new()),
            sessions_changed: watch::channel(()).0,
            revoked: broadcast::channel(64).0,
            ending: Mutex::default(),
            released: broadcast::channel(64).0,
            lifecycle: None,
            agent_updates: Agent::ALL.into_iter().map(|agent| (agent, watch::channel(None).0)).collect(),
            catalogs: Mutex::default(),
            hosts: Mutex::default(),
        }
    }

    /// Куда кладут предложенную версию `agent`; session этого агента узнают о ней сами.
    pub(super) fn agent_updates(&self, agent: Agent) -> Option<watch::Sender<Option<AgentRelease>>> {
        self.agent_updates
            .iter()
            .find_map(|(candidate, releases)| (*candidate == agent).then(|| releases.clone()))
    }

    pub(super) fn with_lifecycle(mut self, lifecycle: Option<Arc<Lifecycle>>) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    pub(super) fn registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.registry)
    }

    /// Подписка на отзыв runner. Подписываться нужно до проверки реестра: отзыв между ними иначе пропал бы.
    pub(super) fn revocations(&self) -> broadcast::Receiver<Fingerprint> {
        self.revoked.subscribe()
    }

    /// Подписка на снятие назначения runner. Подписываться нужно до проверки реестра: снятие между ними иначе
    /// пропало бы.
    pub(super) fn releases(&self) -> broadcast::Receiver<Fingerprint> {
        self.released.subscribe()
    }

    /// Закрывает туннели jump runner, потерявшего назначение. Нет получателей — туннелей нет: не ошибка.
    fn release_tunnels(&self, runner: &Fingerprint) {
        drop(self.released.send(runner.clone()));
    }

    fn catalogs(&self) -> MutexGuard<'_, HashMap<Fingerprint, McpCatalog>> {
        self.catalogs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn hosts(&self) -> MutexGuard<'_, HashMap<Fingerprint, RunnerHost>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ending(&self) -> MutexGuard<'_, HashSet<Fingerprint>> {
        self.ending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Адрес сервера данных session, если её канал управления открыт.
    pub(super) fn session_addr(&self, session: &Fingerprint) -> Option<SocketAddr> {
        let (_, link) = self.sessions.get(session)?;
        Some(SocketAddr::new(link.ip, SESSION_DATA_PORT))
    }

    /// Запущен ли в session агент: она в сети и последним сообщила `agent_running`.
    pub(super) fn agent_running(&self, session: &Fingerprint) -> bool {
        self.sessions.get(session).is_some_and(|(_, link)| *link.agent_running.borrow())
    }

    /// Ждёт, пока в session запустится агент.
    pub(super) async fn wait_agent_running(&self, session: &Fingerprint) {
        let mut changed = self.sessions_changed.subscribe();
        // Источник — состояние, а не значение сигнала: условие проверяется сразу и после каждого сигнала.
        drop(changed.wait_for(|()| self.agent_running(session)).await);
    }

    /// Ждёт, пока session пропадёт из сети.
    pub(super) async fn wait_session_gone(&self, session: &Fingerprint) {
        let mut changed = self.sessions_changed.subscribe();
        drop(changed.wait_for(|()| self.sessions.get(session).is_none()).await);
    }

    /// Обслуживает канал управления, пока он жив; при выходе канал закрыт.
    pub(super) async fn serve<S>(self: &Arc<Self>, peer: Peer, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mux, requests) = Mux::new(stream);
        match peer {
            Peer::Enroll => enroll::serve(self, &mux, requests).await,
            Peer::Runner {
                fingerprint,
                issued,
                valid_before,
            } => runner::serve(self, fingerprint, issued, valid_before, &mux, requests).await,
            Peer::Worker { issued, valid_before } => worker::serve(self, issued, valid_before, &mux, requests).await,
            Peer::Session { fingerprint, ip } => session::serve(self, fingerprint, ip, &mux, requests).await,
        }
        mux.close();
    }

    /// Состояние runner для списка и выбора: в сети ли, занят ли.
    fn presence(&self, runner: &registry::RunnerInfo) -> RunnerPresence {
        let busy = runner.session.is_some() || self.ending().contains(&runner.node.fingerprint);
        match (self.runners.is_online(&runner.node.fingerprint), busy) {
            (false, _) => RunnerPresence::Offline,
            (true, true) => RunnerPresence::Busy,
            (true, false) => RunnerPresence::Free,
        }
    }

    fn runner_info(&self, runner: &registry::RunnerInfo) -> protocol::RunnerInfo {
        protocol::RunnerInfo {
            fingerprint: runner.node.fingerprint.clone(),
            name: runner.node.name.clone(),
            presence: self.presence(runner),
            temporary: runner.node.temporary,
            parent: runner.node.parent.clone(),
        }
    }

    /// Runner для списков worker и session.
    pub(super) fn runner_list(&self) -> Result<Vec<protocol::RunnerInfo>, Error> {
        let runners = self.registry().runners().map_err(|e| registry_error("runner", &e))?;
        Ok(runners
            .iter()
            .filter(|runner| !runner.node.revoked)
            .map(|runner| self.runner_info(runner))
            .collect())
    }

    /// Файл первого `orbit_key` удаляется после первого использования ключа.
    fn forget_first_orbit_key(&self) {
        if let Some(path) = &self.first_key {
            certificates::forget_first_orbit_key(path);
        }
    }

    /// Отзывает runner и его клоны: пометка в реестре, закрытие каналов и туннелей, session теряет назначение.
    /// Сертификат им больше не продлевается.
    pub(super) fn revoke_runner(self: &Arc<Self>, runner: &Fingerprint) -> Result<(), Error> {
        let revocation = self.registry().revoke_node(runner).map_err(|e| registry_error("отзыв", &e))?;
        for node in revocation.nodes {
            self.forget_catalog(&node);
            // Нет получателей — никто из отозванного не подключён: это не ошибка.
            drop(self.revoked.send(node));
        }
        for session in revocation.sessions {
            let control = Arc::clone(self);
            tokio::spawn(async move {
                Arc::clone(&control).assign_runner(session.clone(), None, AgentRun::Stop).await;
                control.clear_catalog(&session).await;
            });
        }
        Ok(())
    }
}

fn lock(registry: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    registry.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Запрос узлу с ответом; без ответа за `timeout` — ошибка.
async fn call<Req: Serialize, Resp: DeserializeOwned>(mux: &Mux, request: &Req, timeout: Duration) -> anyhow::Result<Resp> {
    tokio::time::timeout(timeout, ask(mux, request))
        .await
        .map_err(|_| anyhow::anyhow!("нет ответа за {timeout:?}"))?
}

/// Ждёт нового `orbit_key`, выданного позже `issued` (секунды Unix).
pub(super) async fn superseded(mut key: watch::Receiver<Option<i64>>, issued: u64) {
    let newer = |key: &Option<i64>| key.is_some_and(|key| u64::try_from(key).is_ok_and(|key| key > issued));
    if key.wait_for(newer).await.is_ok() {
        return;
    }
    std::future::pending::<()>().await;
}

/// Разбирает запрос как сообщение роли. `None` — не из набора роли: соединение закрывают.
fn decode<T: DeserializeOwned>(incoming: &Incoming, role: &str) -> Option<T> {
    match protocol::decode_body(incoming.body()) {
        Ok(request) => Some(request),
        Err(error) => {
            tracing::warn!(%error, role, "сообщение не из набора роли, соединение закрывается");
            None
        }
    }
}

/// Последний ответ на запрос; `false` — канал закрыт.
async fn reply<T: Serialize>(incoming: &Incoming, response: &T) -> bool {
    let body = match protocol::encode(response) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "ответ не закодирован");
            return false;
        }
    };
    incoming.respond(body.into(), true).await.is_ok()
}

/// Ответ, после которого поток ответов остаётся открытым.
async fn reply_open<T: Serialize>(incoming: &Incoming, response: &T) -> bool {
    let Ok(body) = protocol::encode(response) else {
        return false;
    };
    incoming.respond(body.into(), false).await.is_ok()
}

/// Отказ в запросе, который server не обслуживает.
fn unsupported() -> Error {
    Error::Io("не поддерживается".to_owned())
}

/// Сколько осталось до момента `unix` (секунды Unix).
fn until(unix: u64) -> Duration {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    Duration::from_secs(unix).saturating_sub(now)
}

/// Конец срока сертификата из его текста.
fn valid_before(certificate: &str) -> Option<u64> {
    russh::keys::Certificate::from_openssh(certificate).ok().map(|c| c.valid_before())
}

/// Причина отказа остаётся в журнале; узел узнаёт только вид ошибки.
fn node_error(what: &str, error: &certificates::Error) -> Error {
    tracing::warn!(%error, "{what} отклонена");
    match error {
        certificates::Error::Registry(registry::Error::FingerprintTaken | registry::Error::KeyTaken) => Error::Conflict,
        certificates::Error::Registry(registry::Error::BadCredentials | registry::Error::NotFound(_) | registry::Error::Revoked)
        | certificates::Error::Signature
        | certificates::Error::Stale => Error::Access,
        certificates::Error::PublicKey => Error::Io("открытый ключ не годится".to_owned()),
        certificates::Error::WorkerName => Error::Io("имя worker не годится для входа".to_owned()),
        _ => Error::Io("внутренняя ошибка server".to_owned()),
    }
}

/// Ошибка реестра в ответе узлу: подробности — в журнале.
fn registry_error(what: &str, error: &registry::Error) -> Error {
    match error {
        registry::Error::NotFound(_) => Error::NotFound,
        registry::Error::Revoked | registry::Error::BadCredentials => Error::Access,
        registry::Error::RunnerBusy
        | registry::Error::KeyTaken
        | registry::Error::FingerprintTaken
        | registry::Error::InvalidTransition { .. } => Error::Conflict,
        registry::Error::InvalidName | registry::Error::NotLinked => Error::Io(format!("{what}: {error}")),
        other => {
            tracing::error!(error = %other, "{what}: реестр");
            Error::Io("внутренняя ошибка server".to_owned())
        }
    }
}

#[cfg(test)]
#[path = "../../tests/server/control.rs"]
mod tests;
