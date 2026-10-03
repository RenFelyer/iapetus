//! Сервер канала данных через настоящий SSH на 127.0.0.1: runner входит сертификатом узла, каждое условие входа и
//! каждый запрещённый запрос канала нарушаются отдельным тестом, остальное при этом верно.

use std::time::Duration;

use bytes::Bytes;
use nerve::mux::{Closed, Requests};
use nerve::protocol::{self, Attr, Change, ChangeKind, Kind, McpCatalog, Root, WatchBatch};
use nerve::ssh::{CLOCK_SKEW, DATA_HOST};
use russh::ChannelMsg;
use russh::client::{self, AuthResult, Handle};
use russh::keys::ssh_key::Algorithm;
use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};

use super::*;
use crate::session::agent::Profile;
use crate::session::data::{Event, Link};
use crate::session::host::Environment;
use crate::session::launch::{AgentCommand, Restarts, run_after_runner};
use crate::session::mcp::Project;
use crate::session::state::{AgentEvent, Input, Machine};
use crate::session::update::{Staged, UpdateReply, UpdateRequest};
use nerve::agent::Agent;
use nerve::session_state::SessionState;
use pty_process::Pty;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use tokio::sync::oneshot;

const HOUR: Duration = Duration::from_secs(3600);

/// Проектный MCP, у которого сводка уже есть: запуск агента не ждёт её.
fn ready_project(stand: &Stand) -> Project {
    let project = Project::new(stand.link.clone(), 64);
    project.set(McpCatalog::default());
    project
}

fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

/// Момент выдачи и срок жизни сертификата.
#[derive(Clone, Copy)]
struct Validity {
    now: SystemTime,
    lifetime: Duration,
}

impl Validity {
    fn from_now(lifetime: Duration) -> Self {
        Self {
            now: SystemTime::now(),
            lifetime,
        }
    }
}

/// Центр сертификации стенда: подписывает так же, как server, — окно начинается на допуск часов раньше выдачи.
struct TestCa {
    key: PrivateKey,
}

/// Runner с сертификатом узла: `key_id` — его fingerprint, принципалы — оба имени входа.
struct TestNode {
    fingerprint: Fingerprint,
    key: PrivateKey,
    certificate: Certificate,
}

impl TestCa {
    fn new() -> Self {
        Self { key: random_key() }
    }

    fn public_key(&self) -> PublicKey {
        self.key.public_key().clone()
    }

    fn issue(&self, subject: &PublicKey, cert_type: CertType, key_id: &str, principals: &[&str], validity: Validity) -> Certificate {
        let now = validity.now.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let valid_after = now.saturating_sub(CLOCK_SKEW).as_secs();
        let valid_before = (now + validity.lifetime).as_secs();
        let mut builder = Builder::new(rand::random::<[u8; 16]>(), subject.key_data().clone(), valid_after, valid_before).unwrap();
        builder.cert_type(cert_type).unwrap();
        builder.key_id(key_id).unwrap();
        for principal in principals {
            builder.valid_principal(*principal).unwrap();
        }
        builder.sign(&self.key).unwrap()
    }

    fn issue_user(&self, subject: &PublicKey, key_id: &str, principals: &[&str], validity: Validity) -> Certificate {
        self.issue(subject, CertType::User, key_id, principals, validity)
    }

    fn issue_host(&self, subject: &PublicKey, key_id: &str, principals: &[&str], validity: Validity) -> Certificate {
        self.issue(subject, CertType::Host, key_id, principals, validity)
    }

    /// Сертификат worker: `key_id` `worker`, принципалы server и `name`; `permit-pty` — по `pty`.
    fn issue_worker(&self, subject: &PublicKey, name: &str, pty: bool) -> Certificate {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let valid_after = now.saturating_sub(CLOCK_SKEW).as_secs();
        let valid_before = (now + Duration::from_mins(15)).as_secs();
        let mut builder = Builder::new(rand::random::<[u8; 16]>(), subject.key_data().clone(), valid_after, valid_before).unwrap();
        builder.cert_type(CertType::User).unwrap();
        builder.key_id(WORKER_KEY_ID).unwrap();
        for principal in ["iapetus-control", "iapetus-jump", name] {
            builder.valid_principal(principal).unwrap();
        }
        if pty {
            builder.extension("permit-pty", "").unwrap();
        }
        builder.sign(&self.key).unwrap()
    }

    fn issue_node(&self, subject: &PublicKey, fingerprint: &str, validity: Validity) -> Certificate {
        self.issue_user(subject, fingerprint, &[NODE_LOGIN, DATA_LOGIN], validity)
    }

    fn enroll_at(&self, now: SystemTime) -> TestNode {
        let id: u128 = rand::random();
        let fingerprint = format!("{id:032x}").parse::<Fingerprint>().unwrap();
        let key = random_key();
        let validity = Validity {
            now,
            lifetime: Duration::from_mins(15),
        };
        let certificate = self.issue_node(key.public_key(), fingerprint.as_str(), validity);
        TestNode {
            fingerprint,
            key,
            certificate,
        }
    }
}

/// Сколько ждать того, что должно случиться сразу: дольше — зависание.
const HANG: Duration = Duration::from_secs(20);

async fn within<F: Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(HANG, future)
        .await
        .unwrap_or_else(|_| panic!("зависание: {what}"))
}

/// Проверка хоста — дело waldo; здесь клиент принимает любой.
struct AnyHost;

impl client::Handler for AnyHost {
    type Error = russh::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Stand {
    ca: TestCa,
    data: Data,
    link: Link,
    events: mpsc::UnboundedReceiver<Event>,
    /// Слот терминала агента, из которого читает сервер.
    slot: watch::Sender<Option<Terminal>>,
    /// Сколько worker смотрят терминал.
    viewers: watch::Receiver<usize>,
    addr: SocketAddr,
}

async fn stand() -> Stand {
    stand_with(None).await
}

async fn stand_with(terminal: Option<Terminal>) -> Stand {
    let ca = TestCa::new();
    let host_key = random_key();
    let host_cert = ca.issue_host(host_key.public_key(), "session", &[DATA_HOST], Validity::from_now(HOUR));
    let (data, link, events) = Data::new();
    let mut config = config(host_key, host_cert);
    // Отказ в тестах не ждёт секунду, как на боевом сервере.
    config.auth_rejection_time = Duration::from_millis(1);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (slot, slot_rx) = watch::channel(terminal);
    let server = Listener::new(data.clone(), Trust::new(&[ca.public_key()]), config, slot_rx);
    let viewers = server.viewers();
    tokio::spawn(async move { server.serve(listener).await });
    Stand {
        ca,
        data,
        link,
        events,
        slot,
        viewers,
        addr,
    }
}

/// Открытый канал данных со стороны runner.
struct Runner {
    ssh: Handle<AnyHost>,
    mux: Mux,
    requests: Requests,
}

impl Stand {
    fn runner(&self) -> TestNode {
        self.ca.enroll_at(SystemTime::now())
    }

    async fn connect_to(&self, addr: SocketAddr) -> Handle<AnyHost> {
        client::connect(Arc::new(client::Config::default()), addr, AnyHost).await.unwrap()
    }

    /// Вход сертификатом; `Some`, если сессия его приняла.
    async fn login(&self, user: &str, key: &PrivateKey, cert: Certificate) -> Option<Handle<AnyHost>> {
        let mut handle = self.connect_to(self.addr).await;
        let result = handle.authenticate_openssh_cert(user, Arc::new(key.clone()), cert).await.unwrap();
        matches!(result, AuthResult::Success).then_some(handle)
    }

    async fn login_as(&self, node: &TestNode) -> Option<Handle<AnyHost>> {
        self.login(DATA_LOGIN, &node.key, node.certificate.clone()).await
    }

    async fn assert_login_rejected(&self, user: &str, key: &PrivateKey, cert: Certificate, why: &str) {
        assert!(self.login(user, key, cert).await.is_none(), "сессия приняла вход: {why}");
        assert!(self.link.current().is_none(), "отклонённый вход открыл канал данных: {why}");
    }

    /// Вход и открытие канала данных назначенным runner.
    async fn open(&self, node: &TestNode) -> Runner {
        let ssh = self.login_as(node).await.expect("назначенный runner не принят");
        let (mux, requests) = open_data(&ssh).await.expect("подсистема данных не открылась");
        Runner { ssh, mux, requests }
    }

    async fn next_event(&mut self) -> Event {
        within("событие канала данных", self.events.recv())
            .await
            .expect("поток событий закрыт")
    }

    /// События, пришедшие к этому моменту.
    fn pending_events(&mut self) -> Vec<Event> {
        std::iter::from_fn(|| self.events.try_recv().ok()).collect()
    }
}

/// Открывает канал `session` и запрашивает подсистему; `None`, если сессия отказала.
async fn open_data(ssh: &Handle<AnyHost>) -> Option<(Mux, Requests)> {
    let mut channel = ssh.channel_open_session().await.ok()?;
    channel.request_subsystem(true, DATA_SUBSYSTEM).await.ok()?;
    loop {
        match within("ответ на запрос подсистемы", channel.wait()).await {
            Some(ChannelMsg::Success) => return Some(Mux::new(channel.into_stream())),
            Some(ChannelMsg::Failure) | None => return None,
            Some(_) => {}
        }
    }
}

fn batch(path: &str) -> WatchBatch {
    WatchBatch {
        root: Root::Project,
        changes: vec![Change {
            path: path.to_owned(),
            kind: ChangeKind::Updated(Attr {
                kind: Kind::File,
                size: 1,
                mtime_ns: 1,
                mode: 0o644,
            }),
        }],
    }
}

// ---------------------------------------------------------------- вход

#[tokio::test]
async fn assigned_runner_exchanges_requests_in_both_directions() {
    let mut stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let mut runner = stand.open(&node).await;
    assert_eq!(stand.next_event().await, Event::Connected);

    // сессия → runner
    let mux = stand.link.current().expect("после входа канал данных не опубликован");
    let asking = tokio::spawn(async move { mux.request(Bytes::from_static(b"ping")).await });
    let incoming = within("запрос сессии", runner.requests.recv()).await.unwrap();
    assert_eq!(incoming.body().as_ref(), b"ping");
    incoming.respond(Bytes::from_static(b"pong"), true).await.unwrap();
    assert_eq!(asking.await.unwrap().unwrap(), Bytes::from_static(b"pong"));

    // runner → сессия
    runner
        .mux
        .notify(Bytes::from(protocol::encode(&batch("a")).unwrap()))
        .await
        .unwrap();
    assert_eq!(stand.next_event().await, Event::Watch(batch("a")));
}

#[tokio::test]
async fn runner_other_than_the_assigned_one_is_rejected() {
    let stand = stand().await;
    let assigned = stand.runner();
    let other = stand.runner();
    stand.data.assign(Some(assigned.fingerprint));
    stand
        .assert_login_rejected(DATA_LOGIN, &other.key, other.certificate, "сертификат другого runner")
        .await;
}

#[tokio::test]
async fn nobody_is_admitted_while_no_runner_is_assigned() {
    let stand = stand().await;
    let node = stand.runner();
    stand
        .assert_login_rejected(DATA_LOGIN, &node.key, node.certificate, "runner не назначен")
        .await;
}

#[tokio::test]
async fn login_name_of_the_server_is_rejected_though_the_certificate_allows_it() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    // В принципалах сертификата есть и имя входа на SSH server: канал данных пускает только под своим.
    assert!(node.certificate.valid_principals().iter().any(|p| p == NODE_LOGIN));
    stand
        .assert_login_rejected(NODE_LOGIN, &node.key, node.certificate, "имя входа не канала данных")
        .await;
}

#[tokio::test]
async fn certificate_from_another_ca_is_rejected() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let other_ca = TestCa::new();
    let key = random_key();
    let cert = other_ca.issue_node(
        key.public_key(),
        node.fingerprint.as_str(),
        Validity::from_now(Duration::from_secs(60)),
    );
    stand
        .assert_login_rejected(DATA_LOGIN, &key, cert, "подпись чужого CA при верном fingerprint")
        .await;
}

#[tokio::test]
async fn expired_certificate_is_rejected() {
    let stand = stand().await;
    let node = stand.ca.enroll_at(SystemTime::now() - HOUR);
    stand.data.assign(Some(node.fingerprint.clone()));
    stand
        .assert_login_rejected(DATA_LOGIN, &node.key, node.certificate, "сертификат выпущен час назад на 15 минут")
        .await;
}

#[tokio::test]
async fn plain_key_without_certificate_is_rejected() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint));
    let mut handle = stand.connect_to(stand.addr).await;
    let result = handle
        .authenticate_publickey(DATA_LOGIN, PrivateKeyWithHashAlg::new(Arc::new(node.key), None))
        .await
        .unwrap();
    assert!(
        !matches!(result, AuthResult::Success),
        "сессия приняла обычный ключ без сертификата"
    );
}

#[tokio::test]
async fn certificate_whose_key_id_is_not_a_fingerprint_is_rejected() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint));
    let key = random_key();
    let cert = stand.ca.issue_user(
        key.public_key(),
        "не fingerprint!",
        &[DATA_LOGIN],
        Validity::from_now(Duration::from_secs(60)),
    );
    stand
        .assert_login_rejected(DATA_LOGIN, &key, cert, "key_id не разбирается как fingerprint")
        .await;
}

// ---------------------------------------------------------------- запрещённые запросы

/// Сессия отвечает отказом на запрос канала `session`, кроме подсистемы данных.
async fn refused(request: impl AsyncFnOnce(&mut Channel<client::Msg>) -> Result<(), russh::Error>) -> bool {
    let mut stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let ssh = stand.login_as(&node).await.unwrap();
    let mut channel = ssh.channel_open_session().await.unwrap();
    request(&mut channel).await.unwrap();
    let verdict = loop {
        match within("ответ на запрос", channel.wait()).await {
            Some(ChannelMsg::Success) => break false,
            Some(ChannelMsg::Failure) | None => break true,
            Some(_) => {}
        }
    };
    assert!(
        stand.pending_events().iter().all(|event| *event != Event::Connected),
        "запрещённый запрос открыл канал данных"
    );
    verdict
}

#[tokio::test]
async fn shell_is_refused() {
    assert!(
        refused(async |channel| channel.request_shell(true).await).await,
        "сессия дала shell"
    );
}

#[tokio::test]
async fn exec_is_refused() {
    assert!(
        refused(async |channel| channel.exec(true, "id").await).await,
        "сессия выполнила exec"
    );
}

#[tokio::test]
async fn pty_is_refused() {
    assert!(
        refused(async |channel| channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await).await,
        "сессия дала pty"
    );
}

#[tokio::test]
async fn environment_is_refused() {
    assert!(
        refused(async |channel| channel.set_env(true, "LD_PRELOAD", "/x").await).await,
        "сессия приняла переменную окружения"
    );
}

#[tokio::test]
async fn subsystem_other_than_data_is_refused() {
    assert!(
        refused(async |channel| channel.request_subsystem(true, "sftp").await).await,
        "сессия дала подсистему sftp"
    );
}

#[tokio::test]
async fn direct_tcpip_is_refused() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let ssh = stand.login_as(&node).await.unwrap();
    assert!(
        ssh.channel_open_direct_tcpip("127.0.0.1", 22, "127.0.0.1", 0).await.is_err(),
        "сессия открыла туннель наружу"
    );
}

#[tokio::test]
async fn second_data_channel_is_refused() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let runner = stand.open(&node).await;
    assert!(
        open_data(&runner.ssh).await.is_none(),
        "сессия открыла второй канал данных на одном соединении"
    );
    assert!(!runner.mux.is_closed(), "второй канал данных нарушил первый");
}

// ---------------------------------------------------------------- пара один-к-одному

#[tokio::test]
async fn new_connection_of_the_same_runner_evicts_the_old_one() {
    let mut stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let old = stand.open(&node).await;
    assert_eq!(stand.next_event().await, Event::Connected);

    let new = stand.open(&node).await;
    assert!(
        matches!(within("закрытие старого канала", old.mux.closed()).await, Closed::Io(_)),
        "старое соединение не закрыто"
    );
    assert_eq!(stand.next_event().await, Event::Disconnected);
    assert_eq!(stand.next_event().await, Event::Connected);
    assert!(!new.mux.is_closed(), "новое соединение закрыто вместе со старым");
    let current = stand.link.current().expect("канала нет после вытеснения");
    let asking = tokio::spawn(async move { current.request(Bytes::from_static(b"who")).await });
    let mut new_requests = new.requests;
    let incoming = within("запрос в новое соединение", new_requests.recv()).await.unwrap();
    incoming.respond(Bytes::from_static(b"new"), true).await.unwrap();
    assert_eq!(
        asking.await.unwrap().unwrap(),
        Bytes::from_static(b"new"),
        "запрос ушёл не в новое соединение"
    );
}

#[tokio::test]
async fn failed_login_does_not_evict_the_active_connection() {
    let stand = stand().await;
    let node = stand.runner();
    let stranger = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let runner = stand.open(&node).await;
    assert!(
        stand.login(DATA_LOGIN, &stranger.key, stranger.certificate).await.is_none(),
        "сессия приняла чужой runner"
    );
    assert!(!runner.mux.is_closed(), "неудачный вход чужого закрыл рабочее соединение");
}

#[tokio::test]
async fn assigning_another_runner_closes_the_current_connection_and_admits_the_new_one() {
    let mut stand = stand().await;
    let (first, second) = (stand.runner(), stand.runner());
    stand.data.assign(Some(first.fingerprint.clone()));
    let runner = stand.open(&first).await;
    assert_eq!(stand.next_event().await, Event::Connected);

    stand.data.assign(Some(second.fingerprint.clone()));
    assert!(matches!(within("закрытие", runner.mux.closed()).await, Closed::Io(_)));
    assert_eq!(stand.next_event().await, Event::Disconnected);
    assert!(stand.link.current().is_none(), "после смены runner остался канал прежнего");
    assert!(stand.login_as(&first).await.is_none(), "прежний runner пущен после смены");
    let _second = stand.open(&second).await;
    assert_eq!(stand.next_event().await, Event::Connected);
}

#[tokio::test]
async fn unassigning_closes_the_connection() {
    let mut stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let runner = stand.open(&node).await;
    assert_eq!(stand.next_event().await, Event::Connected);
    stand.data.assign(None);
    assert!(matches!(within("закрытие", runner.mux.closed()).await, Closed::Io(_)));
    assert_eq!(stand.next_event().await, Event::Disconnected);
}

#[tokio::test]
async fn runner_sending_garbage_loses_its_connection() {
    let mut stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let runner = stand.open(&node).await;
    assert_eq!(stand.next_event().await, Event::Connected);
    runner.mux.notify(Bytes::from_static(&[0xFF, 0xFF, 0xFF])).await.unwrap();
    assert_eq!(stand.next_event().await, Event::Disconnected);
    assert!(matches!(within("закрытие", runner.mux.closed()).await, Closed::Io(_)));
}

// ---------------------------------------------------------------- терминал worker

/// Терминал с агентом, который печатает метку и ждёт.
fn agent_terminal() -> (Terminal, tokio::process::Child) {
    let (pty, pts) = pty_process::open().unwrap();
    let child = pty_process::Command::new("sh")
        .arg("-c")
        .arg("echo hello-from-agent; sleep 60")
        .kill_on_drop(true)
        .spawn(pts)
        .unwrap();
    (Terminal::start(pty), child)
}

/// Открытый канал `session` worker и его соединение (оно живёт, пока жив `Handle`); `None` — вход или канал отклонены.
async fn worker_channel(stand: &Stand, name: &str, cert_name: &str, pty: bool) -> Option<(Handle<AnyHost>, Channel<client::Msg>)> {
    let key = random_key();
    let cert = stand.ca.issue_worker(key.public_key(), cert_name, pty);
    let ssh = stand.login(name, &key, cert).await?;
    let channel = ssh.channel_open_session().await.ok()?;
    Some((ssh, channel))
}

/// Ждёт ответа на запрос канала: `true` — успех.
async fn reply(channel: &mut Channel<client::Msg>) -> bool {
    loop {
        match within("ответ на запрос канала", channel.wait()).await {
            Some(ChannelMsg::Success) => return true,
            Some(ChannelMsg::Failure) | None => return false,
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn worker_with_permit_pty_gets_the_agent_screen() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    let (_ssh, mut channel) = worker_channel(&stand, "alice", "alice", true)
        .await
        .expect("worker с permit-pty не принят");
    channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.unwrap();
    assert!(reply(&mut channel).await, "pty с permit-pty отклонён");
    channel.request_shell(true).await.unwrap();
    assert!(reply(&mut channel).await, "shell после pty отклонён");
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("hello-from-agent") {
        match within("экран агента", channel.wait()).await {
            Some(ChannelMsg::Data { data }) => seen.extend_from_slice(&data),
            Some(_) => {}
            None => panic!("канал закрыт до того, как пришёл экран агента"),
        }
    }
}

#[tokio::test]
async fn pty_is_refused_without_permit_pty() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    // Остальное верно: сертификат worker действителен, имя входа — его `name`.
    let (_ssh, mut channel) = worker_channel(&stand, "alice", "alice", false).await.expect("worker не принят");
    channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.unwrap();
    assert!(!reply(&mut channel).await, "pty выдан сертификату без permit-pty");
    channel.request_shell(true).await.unwrap();
    assert!(!reply(&mut channel).await, "shell открыт без pty");
}

#[tokio::test]
async fn worker_logs_in_only_under_the_name_from_its_certificate() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    assert!(
        worker_channel(&stand, "mallory", "alice", true).await.is_none(),
        "вход принят под именем, которого нет в сертификате"
    );
}

#[tokio::test]
async fn worker_cannot_use_a_login_of_iapetus() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    // `iapetus-control` есть среди принципалов сертификата worker, но это имя входа на server.
    assert!(
        worker_channel(&stand, "iapetus-control", "alice", true).await.is_none(),
        "вход принят под служебным именем"
    );
}

/// Читает экран агента, пока в нём не появится `label`.
async fn watch_screen(channel: &mut Channel<client::Msg>, label: &str) {
    channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.unwrap();
    assert!(reply(channel).await, "pty отклонён");
    channel.request_shell(true).await.unwrap();
    assert!(reply(channel).await, "shell отклонён");
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains(label) {
        match within("экран агента", channel.wait()).await {
            Some(ChannelMsg::Data { data }) => seen.extend_from_slice(&data),
            Some(_) => {}
            None => panic!("канал закрыт до того, как пришёл экран агента"),
        }
    }
}

#[tokio::test]
async fn worker_is_refused_until_the_agent_is_started_and_admitted_after() {
    let stand = stand().await;
    assert!(
        worker_channel(&stand, "alice", "alice", true).await.is_none(),
        "worker принят, пока агент не запущен"
    );
    let (terminal, _agent) = agent_terminal();
    stand.slot.send_replace(Some(terminal));
    let (_ssh, mut channel) = worker_channel(&stand, "alice", "alice", true)
        .await
        .expect("worker не принят после запуска агента");
    watch_screen(&mut channel, "hello-from-agent").await;
}

#[tokio::test]
async fn worker_counts_as_a_viewer_from_the_shell_until_it_closes_the_channel() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    let (ssh, mut channel) = worker_channel(&stand, "alice", "alice", true).await.unwrap();
    assert_eq!(*stand.viewers.borrow(), 0, "worker без shell числится зрителем");

    watch_screen(&mut channel, "hello-from-agent").await;
    assert_eq!(*stand.viewers.borrow(), 1, "смотрящий worker не посчитан");

    channel.close().await.unwrap();
    within("уход зрителя", stand.viewers.clone().wait_for(|viewers| *viewers == 0))
        .await
        .unwrap();

    let (_ssh2, mut again) = worker_channel(&stand, "bob", "bob", true).await.unwrap();
    watch_screen(&mut again, "hello-from-agent").await;
    drop(ssh);
    assert_eq!(*stand.viewers.borrow(), 1, "уход одного worker сбросил счётчик другого");
}

#[tokio::test]
async fn viewer_stops_counting_when_the_agent_exits_without_the_client_closing_the_channel() {
    let (pty, pts) = pty_process::open().unwrap();
    let agent = pty_process::Command::new("sh")
        .arg("-c")
        .arg("echo hello-from-agent; sleep 1")
        .kill_on_drop(true)
        .spawn(pts)
        .unwrap();
    let stand = stand_with(Some(Terminal::start(pty))).await;
    let (_ssh, mut channel) = worker_channel(&stand, "alice", "alice", true).await.unwrap();
    watch_screen(&mut channel, "hello-from-agent").await;
    assert_eq!(*stand.viewers.borrow(), 1);

    // Клиент канал не закрывает: он держит `channel` открытым, пока агент не выйдет и session не закроет канал сама.
    within(
        "уход зрителя после выхода агента",
        stand.viewers.clone().wait_for(|viewers| *viewers == 0),
    )
    .await
    .unwrap();
    drop(agent);
}

#[tokio::test]
async fn dropped_connection_stops_counting_as_a_viewer() {
    let (terminal, _agent) = agent_terminal();
    let stand = stand_with(Some(terminal)).await;
    let (ssh, mut channel) = worker_channel(&stand, "alice", "alice", true).await.unwrap();
    watch_screen(&mut channel, "hello-from-agent").await;
    assert_eq!(*stand.viewers.borrow(), 1);

    ssh.disconnect(Disconnect::ByApplication, "", "").await.unwrap();

    within("уход зрителя", stand.viewers.clone().wait_for(|viewers| *viewers == 0))
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_starts_only_after_the_runner_connects() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let (running, _) = watch::channel(false);
    let (events, mut events_rx) = mpsc::unbounded_channel();
    let planned = AtomicBool::new(false);
    let (_commands, commands) = mpsc::channel(1);
    let (_turn, turn) = watch::channel(false);
    let (_viewers, viewers) = watch::channel(0);
    let restarts = Restarts {
        commands,
        turn,
        viewers,
        planned_exit: &planned,
        project: ready_project(&stand),
        environment: watch::channel(Some(Environment {
            project: "/srv/project".into(),
            home: "/home/runner".into(),
            home_fs: false,
            home_alias: None,
        }))
        .1,
    };
    let mut link = stand.link.clone();
    let start = |_resumed: bool| {
        let (pty, pts) = pty_process::open().unwrap();
        let child = pty_process::Command::new("sh")
            .arg("-c")
            .arg("echo hello-from-agent; sleep 60")
            .kill_on_drop(true)
            .spawn(pts)
            .unwrap();
        Ok((child, pty))
    };
    let launch = run_after_runner(&mut link, start, false, &stand.slot, &running, &events, restarts);
    let checks = async {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(!*running.borrow(), "агент объявлен запущенным до подключения runner");
        assert!(stand.slot.borrow().is_none(), "терминал агента есть до подключения runner");
        assert!(events_rx.try_recv().is_err(), "Started отправлен до подключения runner");

        let _runner = stand.open(&node).await;
        within("запуск агента", running.subscribe().wait_for(|running| *running))
            .await
            .unwrap();
        assert!(stand.slot.borrow().is_some(), "агент запущен, а терминала в слоте нет");
        assert!(matches!(events_rx.recv().await, Some(AgentEvent::Started)), "нет события Started");

        let (_ssh, mut channel) = worker_channel(&stand, "alice", "alice", true)
            .await
            .expect("worker не принят после запуска агента");
        watch_screen(&mut channel, "hello-from-agent").await;
    };
    tokio::select! {
        never = launch => { never.unwrap(); }
        () = checks => {}
    }
}
/// Агент-заглушка: печатает свои аргументы и ждёт.
const STUB_AGENT: &str = r#"echo "started-with:$*"; exec sleep 60"#;

/// Как запущен агент: с продолжением разговора ли и был ли флаг `running` опущен к этому моменту.
#[derive(Debug, PartialEq)]
struct Start {
    resumed: bool,
    running: bool,
}

/// Что видит проверка перезапуска агента.
struct Probe<'a> {
    stand: &'a Stand,
    /// Назначенный runner и его открытый канал данных; `None` после обрыва.
    node: &'a TestNode,
    runner: Option<Runner>,
    commands: mpsc::Sender<AgentCommand>,
    turn: watch::Sender<bool>,
    viewers: watch::Sender<usize>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    starts: Rc<RefCell<Vec<Start>>>,
}

impl Probe<'_> {
    async fn event(&mut self) -> AgentEvent {
        within("событие агента", self.events.recv()).await.expect("события агента закрыты")
    }

    /// Ждёт, не случится ли что-нибудь ещё: заглушка и ядро работают в реальном времени, паузе часов их не подчинить.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Запускает `run_after_runner` с заглушкой вместо агента после подключения runner и отдаёт проверке управление им.
async fn supervise(queue: usize, checks: impl AsyncFnOnce(&mut Probe<'_>)) {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let runner = stand.open(&node).await;
    let (running, _) = watch::channel(false);
    let (events, events_rx) = mpsc::unbounded_channel();
    let planned = AtomicBool::new(false);
    let (commands, commands_rx) = mpsc::channel(queue);
    let (turn, turn_rx) = watch::channel(false);
    let (viewers, viewers_rx) = watch::channel(0);
    let restarts = Restarts {
        commands: commands_rx,
        turn: turn_rx,
        viewers: viewers_rx,
        planned_exit: &planned,
        project: ready_project(&stand),
        environment: watch::channel(Some(Environment {
            project: "/srv/project".into(),
            home: "/home/runner".into(),
            home_fs: false,
            home_alias: None,
        }))
        .1,
    };
    let starts = Rc::new(RefCell::new(Vec::new()));
    let start = |resumed: bool| {
        starts.borrow_mut().push(Start {
            resumed,
            running: *running.borrow(),
        });
        let base = vec!["agent".to_owned()];
        let command = if resumed {
            Profile(Agent::ClaudeCode).resume_command(&base)
        } else {
            base
        };
        let (pty, pts) = pty_process::open()?;
        let child = pty_process::Command::new("sh")
            .arg("-c")
            .arg(STUB_AGENT)
            .arg("agent")
            .args(&command[1..])
            .kill_on_drop(true)
            .spawn(pts)?;
        Ok::<(tokio::process::Child, Pty), anyhow::Error>((child, pty))
    };
    let mut link = stand.link.clone();
    let launch = run_after_runner(&mut link, start, false, &stand.slot, &running, &events, restarts);
    let mut probe = Probe {
        stand: &stand,
        node: &node,
        runner: Some(runner),
        commands,
        turn,
        viewers,
        events: events_rx,
        starts: Rc::clone(&starts),
    };
    tokio::select! {
        never = launch => { never.unwrap(); }
        () = checks(&mut probe) => {}
    }
}

#[tokio::test]
async fn resume_restarts_the_agent_with_the_continue_argument_and_is_not_a_failure() {
    supervise(1, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        let mut machine = Machine::default();
        machine.apply(Input::Agent(AgentEvent::Started), 0);
        {
            let (_ssh, mut channel) = worker_channel(probe.stand, "alice", "alice", true).await.unwrap();
            watch_screen(&mut channel, "started-with:\r").await;
        }

        probe.commands.send(AgentCommand::Resume).await.unwrap();
        let stopped = probe.event().await;
        assert_eq!(stopped, AgentEvent::Stopped, "плановый перезапуск не объявлен плановой остановкой");
        machine.apply(Input::Agent(stopped), 0);
        assert_ne!(machine.verdict().state, SessionState::Failure, "плановый перезапуск дал failure");
        assert_eq!(probe.event().await, AgentEvent::Started);

        assert_eq!(
            *probe.starts.borrow(),
            [
                Start {
                    resumed: false,
                    running: false
                },
                Start {
                    resumed: true,
                    running: false
                }
            ],
            "агент не перезапущен с продолжением, или флаг запуска не опускался на время перезапуска"
        );
        let (_ssh, mut channel) = worker_channel(probe.stand, "alice", "alice", true)
            .await
            .expect("worker не принят к перезапущенному агенту");
        watch_screen(&mut channel, "started-with:--continue").await;
    })
    .await;
}

#[tokio::test]
async fn resume_in_the_middle_of_a_turn_waits_for_the_end_of_the_turn() {
    supervise(1, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        probe.turn.send_replace(true);
        probe.commands.send(AgentCommand::Resume).await.unwrap();

        assert_eq!(probe.event().await, AgentEvent::RestartPending);
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "агент остановлен посреди хода");
        assert_eq!(probe.starts.borrow().len(), 1, "агент запущен заново посреди хода");

        probe.turn.send_replace(false);
        assert_eq!(probe.event().await, AgentEvent::Stopped);
        assert_eq!(probe.event().await, AgentEvent::Started);
    })
    .await;
}

#[tokio::test]
async fn second_resume_while_the_first_waits_for_the_end_of_the_turn_restarts_at_once() {
    supervise(4, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        probe.turn.send_replace(true);

        probe.commands.send(AgentCommand::Resume).await.unwrap();
        assert_eq!(probe.event().await, AgentEvent::RestartPending, "ожидание конца хода не объявлено");
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "первый Resume остановил агента посреди хода");

        probe.commands.send(AgentCommand::Resume).await.unwrap();
        let stopped = probe.event().await;
        assert_eq!(stopped, AgentEvent::Stopped, "повторный Resume не перезапустил агента посреди хода");
        assert_ne!(
            verdict_after(&[AgentEvent::Started, stopped]).state,
            SessionState::Failure,
            "немедленный перезапуск дал failure"
        );
        assert_eq!(probe.event().await, AgentEvent::Started);
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "ход был открыт, а агент перезапущен не один раз");
        let resumed: Vec<bool> = probe.starts.borrow().iter().map(|start| start.resumed).collect();
        assert_eq!(resumed, [false, true], "перезапуск не продолжил разговор");
    })
    .await;
}

#[tokio::test]
async fn resume_requests_by_demand_never_count_as_the_exit_loop() {
    supervise(1, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        // Выходов самого агента для цикла хватило бы уже на третьем перезапуске.
        for _ in 0..4 {
            probe.commands.send(AgentCommand::Resume).await.unwrap();
            assert_eq!(probe.event().await, AgentEvent::Stopped);
            assert_eq!(
                probe.event().await,
                AgentEvent::Started,
                "перезапуск по требованию остановил перезапуски"
            );
        }
        assert_eq!(probe.starts.borrow().len(), 5, "агент перезапущен не по каждому требованию");
    })
    .await;
}

/// Состояние сессии после череды событий агента.
fn verdict_after(events: &[AgentEvent]) -> crate::session::state::Verdict {
    let mut machine = Machine::default();
    for event in events {
        machine.apply(Input::Agent(*event), 0);
    }
    machine.verdict()
}

#[tokio::test]
async fn restart_stops_the_agent_as_planned_at_once_and_starts_it_with_the_chosen_resume() {
    for resume in [true, false] {
        supervise(1, async |probe| {
            assert_eq!(probe.event().await, AgentEvent::Started);
            // Смена runner не ждёт конца хода: договорить его уже не с кем.
            probe.turn.send_replace(true);

            probe.commands.send(AgentCommand::Restart { resume }).await.unwrap();

            let stopped = probe.event().await;
            assert_eq!(stopped, AgentEvent::Stopped, "остановка по смене runner не плановая");
            assert_ne!(
                verdict_after(&[AgentEvent::Started, stopped]).state,
                SessionState::Failure,
                "смена runner дала failure"
            );
            assert_eq!(probe.event().await, AgentEvent::Started);
            let resumed: Vec<bool> = probe.starts.borrow().iter().map(|start| start.resumed).collect();
            assert_eq!(
                resumed,
                [false, resume],
                "агент запущен не так, как просил Restart (resume = {resume})"
            );
        })
        .await;
    }
}

#[tokio::test]
async fn restart_waits_for_the_data_channel_of_the_new_runner() {
    supervise(1, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        let new = probe.stand.runner();
        probe.stand.data.assign(Some(new.fingerprint.clone()));
        probe.runner = None;

        probe.commands.send(AgentCommand::Restart { resume: true }).await.unwrap();

        assert_eq!(probe.event().await, AgentEvent::Stopped);
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "агент запущен, пока новый runner не подключился");
        assert_eq!(probe.starts.borrow().len(), 1, "агент запущен без runner");

        probe.runner = Some(probe.stand.open(&new).await);
        assert_eq!(probe.event().await, AgentEvent::Started);
        assert_eq!(probe.starts.borrow().len(), 2);
    })
    .await;
}

#[tokio::test]
async fn stopped_agent_stays_down_when_the_runner_returns_and_starts_only_on_restart() {
    supervise(2, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);

        probe.commands.send(AgentCommand::Stop).await.unwrap();

        let (stopped, halted) = (probe.event().await, probe.event().await);
        assert_eq!(
            (stopped, halted),
            (AgentEvent::Stopped, AgentEvent::Halted),
            "остановка без назначения не объявлена"
        );
        let verdict = verdict_after(&[AgentEvent::Started, stopped, halted]);
        assert_eq!(
            (verdict.state, verdict.cause, verdict.reason.as_deref()),
            (
                SessionState::Waiting,
                Some(protocol::Cause::RunnerRequired),
                Some("нужно назначить runner")
            ),
            "остановленный агент не ждёт назначения runner"
        );
        assert!(probe.stand.slot.borrow().is_none(), "у остановленного агента остался терминал");

        probe.runner = None;
        probe.runner = Some(probe.stand.open(probe.node).await);
        // После переподключения server повторяет остановку, и она не поднимает агента.
        probe.commands.send(AgentCommand::Stop).await.unwrap();
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "вернувшийся runner поднял агента без назначения");
        assert_eq!(probe.starts.borrow().len(), 1, "агент запущен без назначения");

        probe.commands.send(AgentCommand::Restart { resume: false }).await.unwrap();
        assert_eq!(probe.event().await, AgentEvent::Started);
        assert_eq!(probe.starts.borrow().len(), 2);
    })
    .await;
}

#[tokio::test]
async fn stop_before_the_runner_connects_cancels_the_start() {
    let stand = stand().await;
    let node = stand.runner();
    stand.data.assign(Some(node.fingerprint.clone()));
    let (running, _) = watch::channel(false);
    let (events, mut events_rx) = mpsc::unbounded_channel();
    let planned = AtomicBool::new(false);
    let (commands, commands_rx) = mpsc::channel(1);
    let (_turn, turn) = watch::channel(false);
    let (_viewers, viewers) = watch::channel(0);
    let restarts = Restarts {
        commands: commands_rx,
        turn,
        viewers,
        planned_exit: &planned,
        project: ready_project(&stand),
        environment: watch::channel(Some(Environment {
            project: "/srv/project".into(),
            home: "/home/runner".into(),
            home_fs: false,
            home_alias: None,
        }))
        .1,
    };
    let starts = Rc::new(RefCell::new(0));
    let start = |_resumed: bool| {
        *starts.borrow_mut() += 1;
        let (pty, pts) = pty_process::open()?;
        let child = pty_process::Command::new("sh")
            .arg("-c")
            .arg(STUB_AGENT)
            .kill_on_drop(true)
            .spawn(pts)?;
        Ok::<(tokio::process::Child, Pty), anyhow::Error>((child, pty))
    };
    let mut link = stand.link.clone();
    let launch = run_after_runner(&mut link, start, false, &stand.slot, &running, &events, restarts);
    let checks = async {
        commands.send(AgentCommand::Stop).await.unwrap();
        assert_eq!(within("Halted", events_rx.recv()).await, Some(AgentEvent::Halted));

        let _runner = stand.open(&node).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(*starts.borrow(), 0, "агент запущен после Stop, когда runner подключился");
        assert!(!*running.borrow());
    };
    tokio::select! {
        never = launch => { never.unwrap(); }
        () = checks => {}
    }
}

/// Бинарник `claude` в каталоге `dir` и загруженный рядом с ним `new`; замена — `Update` с ним, ответ на неё — второй
/// элемент.
fn staged_update(dir: &std::path::Path) -> (std::path::PathBuf, AgentCommand, oneshot::Receiver<UpdateReply>) {
    let binary = dir.join("claude");
    std::fs::write(&binary, "old").unwrap();
    let mut new = tempfile::Builder::new().prefix(".claude-update-").tempfile_in(dir).unwrap();
    std::io::Write::write_all(&mut new, b"new").unwrap();
    let staged = Staged::new("2.1.286".to_owned(), new.into_temp_path(), binary.clone());
    let (request, reply) = UpdateRequest::new(staged);
    (binary, AgentCommand::Update(request), reply)
}

#[tokio::test]
async fn update_stops_the_agent_as_planned_replaces_the_binary_and_restarts_it_with_resume() {
    supervise(1, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        let dir = tempfile::tempdir().unwrap();
        let (binary, update, reply) = staged_update(dir.path());

        probe.commands.send(update).await.unwrap();

        let stopped = probe.event().await;
        assert_eq!(stopped, AgentEvent::Stopped, "остановка для обновления не плановая");
        assert_ne!(
            verdict_after(&[AgentEvent::Started, stopped]).state,
            SessionState::Failure,
            "обновление дало failure"
        );
        assert_eq!(probe.event().await, AgentEvent::Started);
        assert!(matches!(reply.await, Ok(UpdateReply::Installed)), "замена не подтверждена");
        assert_eq!(std::fs::read(&binary).unwrap(), b"new", "бинарник не заменён к запуску агента");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "рядом с бинарником остался временный файл"
        );
        let resumed: Vec<bool> = probe.starts.borrow().iter().map(|start| start.resumed).collect();
        assert_eq!(resumed, [false, true], "агент перезапущен не с продолжением разговора");
    })
    .await;
}

#[tokio::test]
async fn update_does_not_stop_the_agent_while_a_worker_watches_or_a_turn_runs_and_goes_through_when_they_end() {
    supervise(2, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        let dir = tempfile::tempdir().unwrap();
        let (binary, update, reply) = staged_update(dir.path());

        // Условие изменилось после проверки в Updater: worker вошёл, пока команда шла к агенту.
        probe.viewers.send_replace(1);
        probe.commands.send(update).await.unwrap();
        let UpdateReply::Busy(staged) = within("отказ", reply).await.unwrap() else {
            panic!("обновление не отклонено при зрителе");
        };
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "агент остановлен при подключённом worker");
        assert_eq!(std::fs::read(&binary).unwrap(), b"old", "бинарник заменён при подключённом worker");

        probe.viewers.send_replace(0);
        probe.turn.send_replace(true);
        let (request, reply) = UpdateRequest::new(staged);
        probe.commands.send(AgentCommand::Update(request)).await.unwrap();
        let UpdateReply::Busy(staged) = within("отказ", reply).await.unwrap() else {
            panic!("обновление не отклонено посреди хода");
        };
        probe.settle().await;
        assert!(probe.events.try_recv().is_err(), "агент остановлен посреди хода");

        probe.turn.send_replace(false);
        let (request, reply) = UpdateRequest::new(staged);
        probe.commands.send(AgentCommand::Update(request)).await.unwrap();
        assert_eq!(probe.event().await, AgentEvent::Stopped);
        assert_eq!(probe.event().await, AgentEvent::Started);
        assert!(matches!(reply.await, Ok(UpdateReply::Installed)));
        assert_eq!(std::fs::read(&binary).unwrap(), b"new");
    })
    .await;
}

#[tokio::test]
async fn update_of_a_stopped_agent_replaces_the_binary_and_does_not_start_it() {
    supervise(2, async |probe| {
        assert_eq!(probe.event().await, AgentEvent::Started);
        probe.commands.send(AgentCommand::Stop).await.unwrap();
        assert_eq!(probe.event().await, AgentEvent::Stopped);
        assert_eq!(probe.event().await, AgentEvent::Halted);
        let dir = tempfile::tempdir().unwrap();
        let (binary, update, _reply) = staged_update(dir.path());

        probe.commands.send(update).await.unwrap();
        probe.settle().await;

        assert_eq!(std::fs::read(&binary).unwrap(), b"new", "бинарник остановленного агента не заменён");
        assert_eq!(probe.starts.borrow().len(), 1, "обновление подняло остановленного агента");
    })
    .await;
}
