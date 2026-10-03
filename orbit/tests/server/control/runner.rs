//! Канал управления runner: набор запросов, `Start`/`End`, вытеснение, срок сертификата, окружение.

use std::time::SystemTime;

use nerve::connect::server::subscribe;
use nerve::protocol::{
    Assignment, Author, Control as ToSession, EnvChange, EnvEntry, EnvEvent, EnvOp, McpCatalog, McpServer, NodesOp, RenewResponse,
    RunnerRequest, RunnersOp, SessionMessage, SessionResponse, ToRunner, WorkerRequest,
};

use super::reassign::{assert_silent, connected_pair, next, runner_online, session_online};
use super::*;
use crate::server::testing::{signed_renewal, unix};

#[tokio::test]
async fn runner_sending_a_worker_message_closes_the_connection() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let channel = runner_channel(&stand, &node).await;

    assert_foreign_message_closes(&channel, &WorkerRequest::Nodes(NodesOp::NewOrbitKey)).await;
}

#[tokio::test]
async fn runner_without_a_session_gets_end_after_connecting() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let mut channel = runner_channel(&stand, &node).await;

    let (request, incoming): (ToRunner, _) = next_request(&mut channel).await;
    respond(&incoming, &SessionResponse::Ok(())).await;

    assert_eq!(
        request,
        ToRunner::Session(SessionMessage::End { assignment: Assignment(0) }),
        "runner без сессии получил не End"
    );
}

#[tokio::test]
async fn runner_released_while_offline_gets_end_of_its_last_pair_after_connecting() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let session = stand.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    let ended = stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap().assignment;
    stand.env.reg().release_runner(&session).unwrap();
    let mut channel = runner_channel(&stand, &node).await;

    let (request, incoming): (ToRunner, _) = next_request(&mut channel).await;
    respond(&incoming, &SessionResponse::Ok(())).await;

    assert_eq!(
        request,
        ToRunner::Session(SessionMessage::End { assignment: ended }),
        "runner, потерявший пару в отсутствие, не получил End этой пары"
    );
}

#[tokio::test]
async fn session_user_reaches_the_runner_in_start_after_every_reconnect() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let session = stand.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    let identity = protocol::SessionIdentity {
        author: Some(Author {
            name: "Alice".to_owned(),
            email: "alice@example.org".to_owned(),
        }),
        github_token: Some("ghp-session-token".to_owned()),
    };
    stand.env.reg().set_session_identity(&session, &identity).unwrap();
    let assignment = stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap().assignment;
    let expected = ToRunner::Session(SessionMessage::Start {
        assignment,
        author: identity.author.clone().unwrap(),
        github_token: identity.github_token.clone(),
    });

    for connection in ["первом", "повторном"] {
        let mut channel = runner_channel(&stand, &node).await;
        let (request, incoming): (ToRunner, _) = next_request(&mut channel).await;
        respond(&incoming, &SessionResponse::Ok(())).await;
        assert_eq!(
            request, expected,
            "Start при {connection} подключении потерял автора или токен сессии"
        );
        channel.mux.close();
    }
}

#[tokio::test]
async fn runner_with_a_session_gets_start_after_connecting() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let session = stand.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    let assignment = stand.env.reg().assign_runner(&session, &node.fingerprint).unwrap().assignment;
    let mut channel = runner_channel(&stand, &node).await;

    let (request, incoming): (ToRunner, _) = next_request(&mut channel).await;
    respond(&incoming, &SessionResponse::Ok(())).await;

    let expected = ToRunner::Session(SessionMessage::Start {
        assignment,
        author: Author {
            name: "Iapetus".to_owned(),
            email: "iapetus@localhost".to_owned(),
        },
        github_token: None,
    });
    assert_eq!(request, expected, "runner с сессией получил не Start");
}

#[tokio::test]
async fn second_connection_of_a_runner_displaces_the_first() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let first = runner_channel(&stand, &node).await;
    assert_open(&first.mux, "первое соединение закрыто само");

    let second = runner_channel(&stand, &node).await;

    assert_closed(&first.mux, "второе соединение того же fingerprint должно вытеснить первое").await;
    assert_open(&second.mux, "новое соединение закрыто вместе с прежним");
}

#[tokio::test]
async fn displaced_connection_does_not_take_the_runner_offline() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let first = runner_channel(&stand, &node).await;
    let _second = runner_channel(&stand, &node).await;
    assert_closed(&first.mux, "вытеснение").await;
    // Запись о прежнем соединении убирается позже вытеснения и не должна стереть запись нового.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        stand.control.runners.is_online(&node.fingerprint),
        "runner с живым соединением помечен как не в сети"
    );
}

#[tokio::test]
async fn runner_renewing_the_certificate_of_another_node_is_refused() {
    let stand = Stand::start().await;
    let (mine, other) = (stand.env.enroll("r1"), stand.env.enroll("r2"));
    let channel = runner_channel(&stand, &mine).await;
    let stolen = signed_renewal(&other.key, &other.fingerprint, unix(SystemTime::now()), &stand.env.orbit_key);

    let response: RenewResponse = ask_once(&channel, &RunnerRequest::Renew(stolen)).await;

    assert!(
        is_error(&response, &Error::Access),
        "runner получил сертификат другого узла: {response:?}"
    );
}

#[tokio::test]
async fn runner_renews_over_its_channel() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let channel = runner_channel(&stand, &node).await;
    let request = signed_renewal(&node.key, &node.fingerprint, unix(SystemTime::now()), &stand.env.orbit_key);

    let response: RenewResponse = ask_once(&channel, &RunnerRequest::Renew(request)).await;

    assert!(response.is_ok(), "продление по каналу управления отклонено: {response:?}");
}

#[tokio::test]
async fn new_orbit_key_closes_the_runner_channel_at_once() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let runner = runner_channel(&stand, &node).await;
    assert_open(&runner.mux, "канал закрыт до выдачи нового orbit_key");

    stand.new_orbit_key();

    assert_closed(&runner.mux, "выдан новый orbit_key, а канал runner жив до конца сертификата").await;
}

#[tokio::test(start_paused = true)]
async fn runner_channel_closes_when_the_certificate_expires() {
    let direct = Direct::new();
    let node = direct.env.enroll("r");
    let valid_before = unix(SystemTime::now()) + 120;
    let (mux, _requests) = direct.connect(Peer::Runner {
        fingerprint: node.fingerprint.clone(),
        issued: unix(SystemTime::now()),
        valid_before,
    });
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_open(&mux, "канал закрыт до конца сертификата");

    tokio::time::sleep(Duration::from_secs(30)).await;

    assert_closed(&mux, "сертификат не продлён, а канал управления жив").await;
}

#[tokio::test(start_paused = true)]
async fn renewing_over_the_channel_extends_it_to_the_new_certificate() {
    let direct = Direct::new();
    let node = direct.env.enroll("r");
    let valid_before = unix(SystemTime::now()) + 120;
    let (mux, requests) = direct.connect(Peer::Runner {
        fingerprint: node.fingerprint.clone(),
        issued: unix(SystemTime::now()),
        valid_before,
    });
    let request = signed_renewal(&node.key, &node.fingerprint, unix(SystemTime::now()), &direct.env.orbit_key);
    let response: RenewResponse = ask(&mux, &RunnerRequest::Renew(request)).await.unwrap();
    assert!(response.is_ok(), "продление отклонено: {response:?}");

    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_open(&mux, "канал закрыт в момент старого сертификата, хотя продлён");
    tokio::time::sleep(Duration::from_mins(11)).await;

    assert_closed(&mux, "канал пережил и новый сертификат").await;
    drop(requests);
}

#[tokio::test]
async fn runner_hears_environment_changes_of_its_layers_only() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let other = stand.env.enroll("r2");
    let runner = runner_channel(&stand, &node).await;
    let mut events = subscribe::<_, EnvEvent>(&runner.mux, &RunnerRequest::Env).await.unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, events.next()).await.unwrap().unwrap(),
        Some(EnvEvent::Snapshot(Vec::<EnvEntry>::new())),
        "первым пришёл не пустой снимок"
    );
    let worker = worker_channel(&stand, "alice").await;
    let own = EnvChange::Set {
        key: format!("{}.TOKEN", node.fingerprint),
        value: "свой".to_owned(),
    };
    let foreign = EnvChange::Set {
        key: format!("{}.TOKEN", other.fingerprint),
        value: "чужой".to_owned(),
    };
    for change in [&foreign, &own] {
        let answer: Result<(), Error> = ask_once(&worker, &WorkerRequest::Env(EnvOp::Change(change.clone()))).await;
        answer.expect("worker не записал окружение");
    }

    let event = tokio::time::timeout(WAIT, events.next()).await.unwrap().unwrap();

    assert_eq!(
        event,
        Some(EnvEvent::Change(own)),
        "runner получил изменение чужого слоя или не получил своё"
    );
}

#[tokio::test]
async fn runner_answers_clone_requested_by_a_worker() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let mut runner = runner_channel(&stand, &node).await;
    let (_, incoming): (ToRunner, _) = next_request(&mut runner).await;
    respond(&incoming, &SessionResponse::Ok(())).await;
    let worker = worker_channel(&stand, "alice").await;
    let asked = tokio::spawn(async move {
        let request = WorkerRequest::Runners(RunnersOp::Clone(protocol::WorkerCloneRequest {
            runner: node.fingerprint.clone(),
            request: protocol::CloneRequest { dir: "/tmp/x".to_owned() },
        }));
        let answer: protocol::CloneResponse = ask_once(&worker, &request).await;
        answer
    });

    let (request, incoming): (ToRunner, _) = next_request(&mut runner).await;
    let clone = protocol::CloneReply {
        fingerprint: crate::server::testing::random_fingerprint(),
        name: "x-clone-1".to_owned(),
    };
    respond(&incoming, &protocol::CloneResponse::Ok(clone.clone())).await;

    assert_eq!(request, ToRunner::Clone(protocol::CloneRequest { dir: "/tmp/x".to_owned() }));
    assert_eq!(asked.await.unwrap(), Ok(clone), "worker не получил ответ runner на клон");
}

/// Сводка с одним сервером; `name` отличает одну сводку от другой.
pub(super) fn catalog(name: &str) -> McpCatalog {
    McpCatalog {
        servers: vec![McpServer {
            name: name.to_owned(),
            instructions: Some("как пользоваться".to_owned()),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        }],
    }
}

async fn send(mux: &Mux, catalog: &McpCatalog) -> Result<(), Error> {
    ask(mux, &RunnerRequest::Mcp(catalog.clone())).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn catalog_of_the_assigned_runner_reaches_the_online_session_unchanged() {
    let mut p = connected_pair(Ok(())).await;

    assert_eq!(send(&p.home_mux, &catalog("db")).await, Ok(()), "сводка не принята");

    assert_eq!(
        next(&mut p.control).await,
        ToSession::McpCatalog(catalog("db")),
        "session получила не ту сводку"
    );
    assert_eq!(
        p.direct.control.catalogs().get(&p.home.fingerprint),
        Some(&catalog("db")),
        "сводка не сохранена"
    );
}

#[tokio::test(start_paused = true)]
async fn runner_without_a_session_is_refused_and_nothing_is_stored() {
    let direct = Direct::new();
    let node = direct.env.enroll("free");
    let (mux, mut seen) = runner_online(&direct, &node).await;
    next(&mut seen).await;

    assert_eq!(send(&mux, &catalog("db")).await, Err(Error::Conflict), "runner без пары принят");

    assert!(direct.control.catalogs().is_empty(), "сводка runner без пары сохранена");
}

#[tokio::test(start_paused = true)]
async fn catalog_over_the_limit_is_refused_and_nothing_is_stored() {
    let mut p = connected_pair(Ok(())).await;
    let mut huge = catalog("db");
    huge.servers[0].instructions = Some("я".repeat(protocol::MAX_MCP_CATALOG));

    let refused = send(&p.home_mux, &huge).await;

    assert!(matches!(refused, Err(Error::Io(_))), "слишком большая сводка принята: {refused:?}");
    assert!(p.direct.control.catalogs().is_empty(), "слишком большая сводка сохранена");
    assert_silent(&mut p.control, "слишком большая сводка ушла session").await;
}

#[tokio::test(start_paused = true)]
async fn empty_catalog_replaces_the_previous_one() {
    let mut p = connected_pair(Ok(())).await;
    send(&p.home_mux, &catalog("db")).await.unwrap();
    next(&mut p.control).await;

    assert_eq!(send(&p.home_mux, &McpCatalog::default()).await, Ok(()), "пустая сводка не принята");

    assert_eq!(
        next(&mut p.control).await,
        ToSession::McpCatalog(McpCatalog::default()),
        "session не получила пустую сводку"
    );
    assert_eq!(
        p.direct.control.catalogs().get(&p.home.fingerprint),
        Some(&McpCatalog::default()),
        "пустая сводка не заменила прежнюю"
    );
}

#[tokio::test(start_paused = true)]
async fn empty_catalog_sent_before_the_session_connects_reaches_it_on_connection() {
    let direct = Direct::new();
    let home = direct.env.enroll("home");
    let session = direct.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    direct.env.reg().assign_runner(&session, &home.fingerprint).unwrap();
    let (home_mux, mut home_seen) = runner_online(&direct, &home).await;
    next(&mut home_seen).await;
    assert_eq!(send(&home_mux, &McpCatalog::default()).await, Ok(()), "пустая сводка не принята");

    let (_session_mux, mut control) = session_online(&direct, &session, Ok(()));

    assert!(
        matches!(next(&mut control).await, ToSession::AssignRunner { .. }),
        "session получила не назначение"
    );
    assert_eq!(
        next(&mut control).await,
        ToSession::McpCatalog(McpCatalog::default()),
        "подключившаяся session не узнала, что серверов у проекта нет, и будет ждать первую сводку"
    );
}

#[tokio::test(start_paused = true)]
async fn offline_session_does_not_fail_the_runner_and_gets_the_catalog_later() {
    let direct = Direct::new();
    let node = direct.env.enroll("home");
    let session = direct.env.reg().create_session(protocol::Agent::ClaudeCode).unwrap();
    direct.env.reg().assign_runner(&session, &node.fingerprint).unwrap();
    let (mux, mut seen) = runner_online(&direct, &node).await;
    next(&mut seen).await;

    assert_eq!(send(&mux, &catalog("db")).await, Ok(()), "сводка при session не в сети отвергнута");

    let (_session_mux, mut control) = session_online(&direct, &session, Ok(()));
    assert!(
        matches!(next(&mut control).await, ToSession::AssignRunner { .. }),
        "сначала должно прийти назначение"
    );
    assert_eq!(
        next(&mut control).await,
        ToSession::McpCatalog(catalog("db")),
        "подключившаяся session не получила сводку"
    );
}
