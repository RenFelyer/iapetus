//! Канал управления session: набор запросов, назначение при подключении, признак «агент запущен».

use nerve::protocol::{
    Activity, Agent, AgentRelease, AgentRun, Artifact, ArtifactFormat, AssignedRunner, Cause, Control as ToSession, Digest, EnvChange,
    NodesOp, Platform, SessionRequest, Status, WorkerRequest,
};
use nerve::session_state::SessionState;

use super::*;

fn status(state: SessionState, agent_running: bool) -> Status {
    Status {
        state,
        reason: None,
        cause: None,
        limit_reset: None,
        runner: None,
        agent_running,
        activity: Activity::default(),
        limits: Vec::new(),
        restart_pending: false,
    }
}

async fn report(channel: &Channel, request: SessionRequest) -> Result<(), Error> {
    ask_once(channel, &request).await
}

/// Указание «runner не назначен»: агент без runner остановлен.
fn no_runner() -> ToSession {
    ToSession::AssignRunner {
        runner: None,
        agent: AgentRun::Stop,
    }
}

/// Сообщение о текущем назначении после подключения: агента не трогать.
fn keep(assigned: &AssignedRunner) -> ToSession {
    ToSession::AssignRunner {
        runner: Some(assigned.clone()),
        agent: AgentRun::Keep,
    }
}

/// Принимает указание server session и подтверждает его.
async fn accept_control(channel: &mut Channel) -> ToSession {
    let (control, incoming): (ToSession, _) = next_request(channel).await;
    respond(&incoming, &Ok::<(), Error>(())).await;
    control
}

#[tokio::test]
async fn session_sending_a_worker_message_closes_the_connection() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let channel = session_channel(&stand, &session, &secret).await;

    assert_foreign_message_closes(&channel, &WorkerRequest::Nodes(NodesOp::NewOrbitKey)).await;
}

#[tokio::test]
async fn session_without_a_runner_gets_an_empty_assignment_after_connecting() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let mut channel = session_channel(&stand, &session, &secret).await;

    assert_eq!(accept_control(&mut channel).await, no_runner());
}

#[tokio::test]
async fn session_gets_its_current_runner_after_every_connection() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let other = stand.env.enroll("other");
    for key in [
        "all.EDITOR",
        &format!("{}.TOKEN", node.fingerprint),
        &format!("{}.FOREIGN", other.fingerprint),
    ] {
        let change = EnvChange::Set {
            key: key.to_owned(),
            value: "секретное-значение".to_owned(),
        };
        stand.control.env.change(&stand.env.registry, &change).unwrap();
    }
    let (session, secret) = stand.session();
    stand
        .control
        .hosts()
        .insert(node.fingerprint.clone(), crate::server::testing::runner_host());
    let assigned = AssignedRunner {
        runner: node.fingerprint.clone(),
        assignment: stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap().assignment,
        name: "r".to_owned(),
        os: "linux".to_owned(),
        host: crate::server::testing::runner_host(),
        env_names: vec!["EDITOR".to_owned(), "TOKEN".to_owned()],
    };

    let mut first = session_channel(&stand, &session, &secret).await;
    assert_eq!(accept_control(&mut first).await, keep(&assigned));
    first.mux.close();
    drop(first);
    let mut second = session_channel(&stand, &session, &secret).await;

    assert_eq!(
        accept_control(&mut second).await,
        keep(&assigned),
        "после переподключения session не получила назначение заново"
    );
}

#[tokio::test]
async fn resume_reaches_the_session_that_is_in_the_network() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let mut channel = session_channel(&stand, &session, &secret).await;
    assert_eq!(accept_control(&mut channel).await, no_runner());

    let (sent, received) = tokio::join!(stand.control.resume_agent(&session), accept_control(&mut channel));

    assert_eq!(sent, Ok(()), "указание не доставлено session");
    assert!(received == ToSession::Resume, "session получила не Resume: {received:?}");
}

#[tokio::test]
async fn resume_for_a_session_outside_the_network_is_an_error() {
    let stand = Stand::start().await;
    let (session, _secret) = stand.session();

    assert!(
        stand.control.resume_agent(&session).await.is_err(),
        "указание принято для session, которой нет в сети"
    );
}

#[tokio::test]
async fn agent_runs_while_the_session_reports_it_and_stays_in_the_network() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let channel = session_channel(&stand, &session, &secret).await;
    assert!(!stand.control.agent_running(&session), "агент запущен до первого сообщения session");

    report(&channel, SessionRequest::Hello(status(SessionState::Success, false)))
        .await
        .unwrap();
    assert!(
        !stand.control.agent_running(&session),
        "Hello без agent_running сделал агента запущенным"
    );

    report(&channel, SessionRequest::Status(status(SessionState::Success, true)))
        .await
        .unwrap();
    assert!(
        stand.control.agent_running(&session),
        "session сообщила agent_running, а агент не запущен"
    );
    tokio::time::timeout(WAIT, stand.control.wait_agent_running(&session))
        .await
        .expect("ожидание запуска не вернулось");

    report(&channel, SessionRequest::Status(status(SessionState::Success, false)))
        .await
        .unwrap();
    assert!(!stand.control.agent_running(&session), "агент остановлен, а считается запущенным");

    report(&channel, SessionRequest::Status(status(SessionState::Success, true)))
        .await
        .unwrap();
    channel.mux.close();
    drop(channel);
    tokio::time::timeout(WAIT, stand.control.wait_session_gone(&session))
        .await
        .expect("ожидание ухода session не вернулось");
    assert!(
        !stand.control.agent_running(&session),
        "агент session, которой нет в сети, считается запущенным"
    );
}

#[tokio::test]
async fn session_state_that_the_registry_does_not_allow_is_refused() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let channel = session_channel(&stand, &session, &secret).await;

    // Новая session ждёт авторизации: из `waiting` в `warning` переход не предусмотрен.
    let answer = report(&channel, SessionRequest::Hello(status(SessionState::Warning, false))).await;

    assert_eq!(answer, Err(Error::Conflict), "недопустимый переход принят");
}

#[tokio::test]
async fn second_connection_of_a_session_displaces_the_first() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let first = session_channel(&stand, &session, &secret).await;

    let second = session_channel(&stand, &session, &secret).await;

    assert_closed(&first.mux, "второе соединение той же session должно вытеснить первое").await;
    assert_open(&second.mux, "новое соединение закрыто вместе с прежним");
}

#[tokio::test]
async fn login_required_keeps_the_session_waiting_until_the_first_success() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let channel = session_channel(&stand, &session, &secret).await;

    let login = Status {
        cause: Some(Cause::LoginRequired),
        reason: Some("нужен вход".to_owned()),
        ..status(SessionState::Waiting, true)
    };
    report(&channel, SessionRequest::Status(login)).await.unwrap();
    assert_eq!(
        stand.env.reg().session(&session).unwrap().state,
        SessionState::Waiting,
        "сессия без входа не ждёт"
    );

    report(&channel, SessionRequest::Status(status(SessionState::Success, true)))
        .await
        .unwrap();
    assert_eq!(
        stand.env.reg().session(&session).unwrap().state,
        SessionState::Success,
        "после входа успешный ответ не вернул success"
    );
}

fn release(version: &str) -> AgentRelease {
    AgentRelease {
        agent: Agent::ClaudeCode,
        version: version.to_owned(),
        artifacts: vec![Artifact {
            platform: Platform::LinuxX64,
            url: format!("https://example.org/{version}/linux-x64/claude"),
            digest: Digest::Sha256("ab".repeat(32)),
            format: ArtifactFormat::Binary,
        }],
    }
}

fn update_agent(version: &str) -> ToSession {
    ToSession::UpdateAgent(release(version))
}

#[tokio::test]
async fn session_gets_the_offered_agent_version_on_every_connection() {
    let stand = Stand::start().await;
    stand
        .control
        .agent_updates(Agent::ClaudeCode)
        .unwrap()
        .send_replace(Some(release("2.1.286")));
    let (session, secret) = stand.session();

    for _ in 0..2 {
        let mut channel = session_channel(&stand, &session, &secret).await;
        let (first, second) = (accept_control(&mut channel).await, accept_control(&mut channel).await);

        let received = [first, second];
        assert!(received.contains(&no_runner()), "session не получила назначение: {received:?}");
        assert!(
            received.contains(&update_agent("2.1.286")),
            "session не получила предложенную версию: {received:?}"
        );
        channel.mux.close();
    }
}

#[tokio::test]
async fn session_in_the_network_gets_each_new_agent_version() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    let mut channel = session_channel(&stand, &session, &secret).await;
    assert_eq!(accept_control(&mut channel).await, no_runner());

    for version in ["2.1.286", "2.1.287"] {
        stand
            .control
            .agent_updates(Agent::ClaudeCode)
            .unwrap()
            .send_replace(Some(release(version)));
        assert_eq!(
            accept_control(&mut channel).await,
            update_agent(version),
            "session в сети не получила версию {version}"
        );
    }
}

#[tokio::test]
async fn session_of_another_agent_gets_no_update_of_this_one() {
    let stand = Stand::start().await;
    stand
        .control
        .agent_updates(Agent::ClaudeCode)
        .unwrap()
        .send_replace(Some(release("2.1.286")));
    let (session, secret) = {
        let mut registry = stand.env.reg();
        let session = registry.create_session(Agent::Codex).unwrap();
        let secret = registry.issue_session_secret(&session).unwrap();
        (session, secret.expose().to_owned())
    };
    let mut channel = session_channel(&stand, &session, &secret).await;
    assert_eq!(accept_control(&mut channel).await, no_runner());

    let extra = tokio::time::timeout(Duration::from_millis(300), next_request::<ToSession>(&mut channel)).await;

    assert!(extra.is_err(), "session Codex получила указание обновить Claude Code");
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Журнал, который пишут задачи сервера на этом потоке (среда теста однопоточная), пока жив возвращённый guard.
fn capture_log() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let log = LogBuffer::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (log, tracing::subscriber::set_default(subscriber))
}

/// Подключает session, отвечает отказом `reason` на указание обновить и отдаёт журнал server.
async fn log_of_refused_update(reason: &str) -> String {
    let (log, _guard) = capture_log();
    let stand = Stand::start().await;
    stand
        .control
        .agent_updates(Agent::ClaudeCode)
        .unwrap()
        .send_replace(Some(release("2.1.286")));
    let (session, secret) = stand.session();
    let mut channel = session_channel(&stand, &session, &secret).await;
    let mut refused = false;
    while !refused {
        let (control, incoming): (ToSession, _) = next_request(&mut channel).await;
        refused = matches!(control, ToSession::UpdateAgent(_));
        let answer = if refused { Err(Error::Io(reason.to_owned())) } else { Ok(()) };
        respond(&incoming, &answer).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned()
}

#[tokio::test]
async fn session_without_an_agent_command_is_refused_at_debug_and_another_refusal_is_a_warning() {
    let quiet = log_of_refused_update(nerve::release::NO_AGENT_COMMAND).await;
    assert!(
        quiet
            .lines()
            .any(|line| line.contains("DEBUG") && line.contains("обновление агента не принято")),
        "отказ session без команды агента не записан на debug: {quiet}"
    );
    assert!(
        !quiet.contains("WARN") || !quiet.contains("обновление агента"),
        "отказ session без команды агента — warn: {quiet}"
    );

    let loud = log_of_refused_update(nerve::release::OTHER_AGENT).await;
    assert!(
        loud.lines()
            .any(|line| line.contains("WARN") && line.contains("session не приняла обновление агента")),
        "прочий отказ session не записан на warn: {loud}"
    );
}
