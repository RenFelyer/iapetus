//! Канал управления worker: набор запросов, отзыв runner, новый `orbit_key`, список runner, срок сертификата.

use std::time::SystemTime;

use nerve::connect::server::{ConnectError, enroll as enroll_request};
use nerve::protocol::{
    Activity, Cause, EnrollRequest, NodeInfo, NodesOp, RunnerInfo, RunnerPresence, RunnerRequest, RunnersOp, SessionInfo, SessionRequest,
    SessionResponse, SessionsOp, Status, ToRunner, UsageWindow, WorkerCertificateRequest, WorkerCertificateResponse, WorkerRequest,
};
use nerve::session_state::SessionState;

use super::*;
use crate::server::testing::{random_key, unix};

/// Список runner глазами worker.
async fn runner_list(worker: &Channel) -> Vec<RunnerInfo> {
    let answer: Result<Vec<RunnerInfo>, Error> = ask_once(worker, &WorkerRequest::Runners(RunnersOp::List)).await;
    answer.expect("список runner не получен")
}

fn presence_of(list: &[RunnerInfo], runner: &Fingerprint) -> Option<RunnerPresence> {
    list.iter().find(|info| &info.fingerprint == runner).map(|info| info.presence)
}

#[tokio::test]
async fn worker_sending_a_runner_message_closes_the_connection() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;

    assert_foreign_message_closes(&worker, &RunnerRequest::Env).await;
}

#[tokio::test]
async fn runner_is_offline_until_it_connects_free_when_connected_and_busy_when_assigned() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let worker = worker_channel(&stand, "alice").await;
    assert_eq!(
        presence_of(&runner_list(&worker).await, &node.fingerprint),
        Some(RunnerPresence::Offline),
        "после запуска server runner не в сети"
    );

    let mut runner = runner_channel(&stand, &node).await;
    let (_, end): (ToRunner, _) = next_request(&mut runner).await;
    assert_eq!(
        presence_of(&runner_list(&worker).await, &node.fingerprint),
        Some(RunnerPresence::Busy),
        "runner не подтвердил End, а уже свободен"
    );
    respond(&end, &SessionResponse::Ok(())).await;
    let free = async {
        while presence_of(&runner_list(&worker).await, &node.fingerprint) != Some(RunnerPresence::Free) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    assert!(
        tokio::time::timeout(WAIT, free).await.is_ok(),
        "runner в сети, сессии нет, End подтверждён, а не свободен"
    );

    let session = stand.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap();
    assert_eq!(
        presence_of(&runner_list(&worker).await, &node.fingerprint),
        Some(RunnerPresence::Busy),
        "с runner работает session"
    );
}

#[tokio::test]
async fn runner_goes_offline_when_its_channel_closes_and_keeps_its_assignment() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let session = stand.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap();
    let worker = worker_channel(&stand, "alice").await;
    let runner = runner_channel(&stand, &node).await;
    assert_eq!(
        presence_of(&runner_list(&worker).await, &node.fingerprint),
        Some(RunnerPresence::Busy)
    );

    runner.mux.close();
    drop(runner);

    let offline = tokio::time::timeout(WAIT, async {
        while presence_of(&runner_list(&worker).await, &node.fingerprint) != Some(RunnerPresence::Offline) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(offline.is_ok(), "runner с закрытым каналом остался в сети");
    assert_eq!(
        stand.env.reg().runner(&node.fingerprint).unwrap().session,
        Some(session),
        "обрыв снял назначение"
    );
}

#[tokio::test]
async fn revoking_a_runner_closes_its_channel_and_it_cannot_come_back() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let runner = runner_channel(&stand, &node).await;
    let worker = worker_channel(&stand, "alice").await;
    assert_open(&runner.mux, "runner закрыт сам");

    let answer: Result<(), Error> = ask_once(&worker, &WorkerRequest::Nodes(NodesOp::Revoke(node.fingerprint.clone()))).await;
    answer.expect("отзыв отклонён");

    assert_closed(&runner.mux, "отозванный runner остался на канале управления").await;
    let login = Login::Certificate {
        key: &node.key,
        certificate: &node.certificate,
    };
    let again = open(&stand.server(Side::External), &login).await;
    assert!(
        matches!(again, Err(ConnectError::Rejected)),
        "отозванный runner вошёл по ещё действующему сертификату"
    );
    let nodes: Result<Vec<NodeInfo>, Error> = ask_once(&worker, &WorkerRequest::Nodes(NodesOp::List)).await;
    let info = nodes
        .unwrap()
        .into_iter()
        .find(|n| n.runner.fingerprint == node.fingerprint)
        .expect("запись отозванного пропала");
    assert!(info.revoked, "в списке узлов отозванный не помечен");
}

#[tokio::test]
async fn revoking_a_runner_tells_its_session_to_drop_the_assignment() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let (session, secret) = stand.session();
    stand
        .control
        .hosts()
        .insert(node.fingerprint.clone(), crate::server::testing::runner_host());
    let assignment = stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap().assignment;
    let mut session_channel = session_channel(&stand, &session, &secret).await;
    let (first, incoming): (protocol::Control, _) = next_request(&mut session_channel).await;
    respond(&incoming, &Ok::<(), Error>(())).await;
    assert_eq!(
        first,
        protocol::Control::AssignRunner {
            runner: Some(protocol::AssignedRunner {
                runner: node.fingerprint.clone(),
                assignment,
                name: "r".to_owned(),
                os: "linux".to_owned(),
                host: crate::server::testing::runner_host(),
                env_names: Vec::new(),
            }),
            agent: protocol::AgentRun::Keep,
        },
        "session при подключении не получила назначение"
    );
    let worker = worker_channel(&stand, "alice").await;

    let answer: Result<(), Error> = ask_once(&worker, &WorkerRequest::Nodes(NodesOp::Revoke(node.fingerprint.clone()))).await;
    answer.expect("отзыв отклонён");

    let (next, incoming): (protocol::Control, _) = next_request(&mut session_channel).await;
    respond(&incoming, &Ok::<(), Error>(())).await;
    assert_eq!(
        next,
        protocol::Control::AssignRunner {
            runner: None,
            agent: protocol::AgentRun::Stop,
        },
        "session не узнала, что runner отозван, и не остановила агента"
    );
}

#[tokio::test]
async fn new_orbit_key_closes_other_workers_but_the_issuing_channel_gets_the_key() {
    let stand = Stand::start().await;
    let issuer = worker_channel(&stand, "alice").await;
    let other = worker_channel(&stand, "kai").await;
    let old = stand.env.orbit_key.expose().to_owned();
    assert_open(&other.mux, "второй worker закрыт сам");

    let answer: Result<String, Error> = ask_once(&issuer, &WorkerRequest::Nodes(NodesOp::NewOrbitKey)).await;
    let key = answer.expect("новый orbit_key не выдан");

    assert_closed(&other.mux, "новый orbit_key не закрыл канал другого worker").await;
    assert_ne!(key, old, "выдан прежний orbit_key");
    let runners: Result<Vec<RunnerInfo>, Error> = ask_once(&issuer, &WorkerRequest::Runners(RunnersOp::List)).await;
    assert!(runners.is_ok(), "канал выдавшего worker закрыт до ответа: {runners:?}");
    let by_old = open(&stand.server(Side::External), &Login::Enroll { orbit_key: &old }).await;
    assert!(matches!(by_old, Err(ConnectError::Rejected)), "прежний orbit_key входит");
    open(&stand.server(Side::External), &Login::Enroll { orbit_key: &key })
        .await
        .expect("новый orbit_key не входит");
}

#[tokio::test]
async fn worker_certificate_requested_over_the_channel_is_issued_for_the_current_orbit_key_only() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    let request = |orbit_key: &str| {
        WorkerRequest::WorkerCertificate(WorkerCertificateRequest {
            orbit_key: orbit_key.to_owned(),
            public_key: random_key().public_key().to_openssh().unwrap(),
            name: "alice".to_owned(),
        })
    };

    let good: WorkerCertificateResponse = ask_once(&worker, &request(stand.env.orbit_key.expose())).await;
    let bad: WorkerCertificateResponse = ask_once(&worker, &request("не тот ключ")).await;

    assert!(good.is_ok(), "сертификат по действующему orbit_key не выдан: {good:?}");
    assert!(is_error(&bad, &Error::Access), "сертификат по неверному orbit_key: {bad:?}");
}

#[tokio::test(start_paused = true)]
async fn worker_channel_closes_when_the_certificate_expires() {
    let direct = Direct::new();
    let valid_before = unix(SystemTime::now()) + 120;
    let (mux, _requests) = direct.connect(Peer::Worker {
        issued: unix(SystemTime::now()),
        valid_before,
    });
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_open(&mux, "канал worker закрыт до конца сертификата");

    tokio::time::sleep(Duration::from_secs(30)).await;

    assert_closed(&mux, "сертификат worker не продлён, а канал жив").await;
}

#[tokio::test(start_paused = true)]
async fn renewing_a_worker_certificate_over_the_channel_extends_it() {
    let direct = Direct::new();
    let valid_before = unix(SystemTime::now()) + 120;
    let (mux, _requests) = direct.connect(Peer::Worker {
        issued: unix(SystemTime::now()),
        valid_before,
    });
    let request = WorkerRequest::WorkerCertificate(WorkerCertificateRequest {
        orbit_key: direct.env.orbit_key.expose().to_owned(),
        public_key: random_key().public_key().to_openssh().unwrap(),
        name: "alice".to_owned(),
    });
    let response: WorkerCertificateResponse = ask(&mux, &request).await.unwrap();
    assert!(response.is_ok(), "продление отклонено: {response:?}");

    tokio::time::sleep(Duration::from_secs(300)).await;

    assert_open(&mux, "канал worker закрыт в момент старого сертификата, хотя продлён");
}

#[tokio::test]
async fn enroll_connection_hands_out_a_worker_certificate_for_the_current_orbit_key() {
    let stand = Stand::start().await;
    let request = WorkerCertificateRequest {
        orbit_key: stand.env.orbit_key.expose().to_owned(),
        public_key: random_key().public_key().to_openssh().unwrap(),
        name: "alice".to_owned(),
    };
    let orbit_key = stand.env.orbit_key.expose().to_owned();

    let response: WorkerCertificateResponse = enroll_request(
        &stand.server(Side::External),
        &orbit_key,
        &EnrollRequest::WorkerCertificate(request),
    )
    .await
    .expect("запрос входа не дошёл");

    assert!(response.is_ok(), "сертификат worker не выдан: {response:?}");
}

async fn session_list(worker: &Channel) -> Vec<SessionInfo> {
    let answer: Result<Vec<SessionInfo>, Error> = ask_once(worker, &WorkerRequest::Sessions(SessionsOp::List)).await;
    answer.expect("список сессий не получен")
}

#[tokio::test]
async fn session_list_shows_the_registry_state_and_what_the_session_reported() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    let (session, secret) = stand.session();

    let offline = session_list(&worker).await;
    assert_eq!(offline.len(), 1, "сессия не попала в список");
    assert_eq!(
        (offline[0].reason.clone(), offline[0].agent_running, offline[0].limits.clone()),
        (None, false, Vec::new())
    );

    let channel = session_channel(&stand, &session, &secret).await;
    let limits = vec![
        UsageWindow {
            duration: Some(18_000),
            scope: None,
            used: 0.4,
            resets_at: Some(1),
        },
        UsageWindow {
            duration: Some(604_800),
            scope: Some("sonnet".to_owned()),
            used: 0.1,
            resets_at: None,
        },
    ];
    let status = Status {
        state: SessionState::Success,
        reason: Some("агент запущен".to_owned()),
        cause: None,
        limit_reset: None,
        runner: None,
        activity: Activity::default(),
        agent_running: true,
        limits: limits.clone(),
        restart_pending: false,
    };
    let sent: Result<(), Error> = ask_once(&channel, &SessionRequest::Hello(status)).await;
    sent.expect("Hello принят");

    let online = session_list(&worker).await;
    assert_eq!(online[0].fingerprint, session);
    assert_eq!(
        online[0].reason.as_deref(),
        Some("агент запущен"),
        "причина состояния не дошла до списка"
    );
    assert!(online[0].agent_running, "агент запущен, а список говорит обратное");
    assert_eq!(online[0].limits, limits, "лимиты не дошли до списка");
}

#[tokio::test]
async fn restart_agent_request_reaches_the_session_as_resume_and_a_pending_restart_shows_in_the_list() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    let (session, secret) = stand.session();
    let mut channel = session_channel(&stand, &session, &secret).await;
    let (_announced, incoming): (protocol::Control, _) = next_request(&mut channel).await;
    respond(&incoming, &Ok::<(), Error>(())).await;

    let request = WorkerRequest::Sessions(SessionsOp::RestartAgent(session.clone()));
    let (answer, received) = tokio::join!(ask_once::<_, Result<(), Error>>(&worker, &request), async {
        let (control, incoming): (protocol::Control, _) = next_request(&mut channel).await;
        respond(&incoming, &Ok::<(), Error>(())).await;
        control
    });

    assert_eq!(answer, Ok(()), "worker не получил подтверждения");
    assert_eq!(received, protocol::Control::Resume, "session получила не Resume");

    let pending = Status {
        state: SessionState::Success,
        reason: None,
        cause: None,
        limit_reset: None,
        runner: None,
        activity: Activity::default(),
        agent_running: true,
        limits: Vec::new(),
        restart_pending: true,
    };
    let sent: Result<(), Error> = ask_once(&channel, &SessionRequest::Hello(pending)).await;
    sent.expect("Hello принят");
    assert!(
        session_list(&worker).await[0].restart_pending,
        "ожидающий перезапуск не дошёл до списка сессий"
    );
}

#[tokio::test]
async fn session_list_shows_the_agent_from_the_registry_without_docker() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    stand.env.reg().create_session(protocol::Agent::Codex).unwrap();

    let listed = session_list(&worker).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent, protocol::Agent::Codex, "агент сессии не прочитан из реестра");
}

#[tokio::test]
async fn session_list_shows_that_a_login_is_required() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    let (session, secret) = stand.session();
    let channel = session_channel(&stand, &session, &secret).await;
    let status = Status {
        state: SessionState::Waiting,
        reason: Some("нужен вход".to_owned()),
        cause: Some(Cause::LoginRequired),
        limit_reset: None,
        runner: None,
        activity: Activity::default(),
        agent_running: true,
        limits: Vec::new(),
        restart_pending: false,
    };
    let sent: Result<(), Error> = ask_once(&channel, &SessionRequest::Hello(status)).await;
    sent.expect("Hello принят");

    let listed = session_list(&worker).await;

    assert_eq!(
        (listed[0].state, listed[0].cause),
        (SessionState::Waiting, Some(Cause::LoginRequired)),
        "список не показывает, что нужен вход"
    );
}

#[tokio::test]
async fn renaming_a_runner_is_visible_in_the_list() {
    let stand = Stand::start().await;
    let worker = worker_channel(&stand, "alice").await;
    let node = stand.env.enroll("old");

    let request = WorkerRequest::Runners(RunnersOp::Rename {
        runner: node.fingerprint.clone(),
        name: "new".to_owned(),
    });
    let answer: Result<(), Error> = ask_once(&worker, &request).await;
    answer.expect("runner не переименован");
    let renamed = runner_list(&worker)
        .await
        .into_iter()
        .find(|info| info.fingerprint == node.fingerprint);
    assert_eq!(renamed.map(|info| info.name).as_deref(), Some("new"));
}
