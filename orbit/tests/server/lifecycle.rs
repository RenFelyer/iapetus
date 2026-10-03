//! Жизненный цикл на настоящем прокси: за ним поддельный Docker с состоянием на unix-сокете. Тесты смотрят на то,
//! что дошло до Docker — тела `create`, тома, имена, — и на реестр.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use nerve::connect::server::ask;
use nerve::fingerprint::Fingerprint;
use nerve::mux::{Mux, Requests};
use nerve::protocol::{
    Agent, AgentRun, Author, Control as ToSession, Error, SessionEvent, SessionIdentity, SessionMessage, SessionResponse, SessionsOp,
    ToRunner, WorkerRequest,
};
use nerve::session_state::SessionState;
use nerve::ssh::{DATA_HOST, Trust};
use russh::keys::Certificate;
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use super::super::control::{Control, Peer};
use super::super::docker::{self, Docker, Limits, Template};
use super::super::mcp::Tokens;
use super::super::registry::SessionSecret;
use super::super::testing::Env;
use super::{Lifecycle, Settings, container_name, store_log, volume_name};

const IMAGE: &str = "registry.example/iapetus/session@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WORK_MAX: i64 = 1 << 30;

#[derive(Debug, Default)]
struct DockerState {
    volumes: BTreeMap<String, Value>,
    /// Имя контейнера, его id и тело `create`.
    containers: Vec<(String, String, Value)>,
    volumes_created: usize,
    /// Все запросы к Docker, в том числе отказанные.
    requests: usize,
    /// Docker отказывает в создании контейнера.
    fail_container_create: bool,
    /// Журналы контейнеров по имени: пары (поток, текст), как их хранит Docker.
    logs: BTreeMap<String, Vec<(u8, String)>>,
    next_id: u64,
}

impl DockerState {
    fn container(&self, key: &str) -> Option<&(String, String, Value)> {
        self.containers.iter().find(|(name, id, _)| name == key || id == key)
    }
}

type Shared = Arc<Mutex<DockerState>>;

fn reply(status: u16, body: impl std::fmt::Display) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn serve_docker(state: &Shared, method: &Method, path: &str, query: &str, body: &[u8]) -> Response<Full<Bytes>> {
    let mut state = state.lock().unwrap();
    state.requests += 1;
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("POST", ["volumes", "create"]) => {
            let body: Value = serde_json::from_slice(body).unwrap();
            let name = body["Name"].as_str().unwrap().to_owned();
            state.volumes.insert(name.clone(), body["Labels"].clone());
            state.volumes_created += 1;
            reply(201, json!({ "Name": name }))
        }
        ("GET", ["volumes", name]) => match state.volumes.get(*name) {
            Some(labels) => reply(200, json!({ "Name": name, "Labels": labels })),
            None => reply(404, r#"{"message":"no such volume"}"#),
        },
        ("DELETE", ["volumes", name]) => {
            state.volumes.remove(*name);
            reply(204, "")
        }
        ("POST", ["containers", "create"]) => {
            if state.fail_container_create {
                return reply(500, r#"{"message":"boom"}"#);
            }
            let name = query.strip_prefix("name=").unwrap().to_owned();
            if state.container(&name).is_some() {
                return reply(409, r#"{"message":"name in use"}"#);
            }
            state.next_id += 1;
            let id = format!("{:064x}", state.next_id);
            let body: Value = serde_json::from_slice(body).unwrap();
            state.containers.push((name, id.clone(), body));
            reply(201, json!({ "Id": id }))
        }
        ("GET", ["containers", "json"]) => {
            let list: Vec<Value> = state
                .containers
                .iter()
                .map(|(name, ..)| json!({ "Names": [format!("/{name}")] }))
                .collect();
            reply(200, Value::Array(list))
        }
        ("GET", ["containers", key, "json"]) => match state.container(key) {
            Some((_, id, create)) => reply(200, json!({ "Id": id, "Config": { "Labels": create["Labels"] } })),
            None => reply(404, r#"{"message":"no such container"}"#),
        },
        ("GET", ["containers", key, "logs"]) => match state.container(key) {
            Some((name, ..)) => {
                let mut framed = Vec::new();
                for (stream, text) in state.logs.get(name).into_iter().flatten() {
                    framed.extend([*stream, 0, 0, 0]);
                    framed.extend(u32::try_from(text.len()).unwrap().to_be_bytes());
                    framed.extend(text.as_bytes());
                }
                Response::builder().status(200).body(Full::new(Bytes::from(framed))).unwrap()
            }
            None => reply(404, r#"{"message":"no such container"}"#),
        },
        ("POST", ["containers", _, "start" | "stop"]) => reply(204, ""),
        ("DELETE", ["containers", key]) => {
            state.containers.retain(|(name, id, _)| name != key && id != key);
            reply(204, "")
        }
        other => reply(500, format!("поддельный Docker не знает {other:?}")),
    }
}

struct Harness {
    env: Env,
    docker: Shared,
    lifecycle: Arc<Lifecycle>,
    control: Arc<Control>,
    tokens: Arc<Tokens>,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let docker: Shared = Shared::default();
        let upstream = UnixListener::bind(dir.path().join("docker.sock")).unwrap();
        let state = Arc::clone(&docker);
        tokio::spawn(async move {
            loop {
                let (stream, _) = upstream.accept().await.unwrap();
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let state = Arc::clone(&state);
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            let query = parts.uri.query().unwrap_or_default();
                            Ok::<_, Infallible>(serve_docker(&state, &parts.method, parts.uri.path(), query, &body))
                        }
                    });
                    http1::Builder::new().serve_connection(TokioIo::new(stream), service).await.unwrap();
                });
            }
        });
        let limits = Limits {
            memory: 2 << 30,
            nano_cpus: 2_000_000_000,
            pids: 512,
            work: WORK_MAX,
        };
        let template = Template::new(vec![IMAGE.to_owned()], "iapetus-sessions".to_owned(), limits, None).unwrap();
        let proxy_socket = dir.path().join("proxy.sock");
        tokio::spawn(docker::serve(
            docker::bind(&proxy_socket).unwrap(),
            dir.path().join("docker.sock"),
            template.clone(),
        ));
        let env = Env::new();
        let tokens = Arc::new(Tokens::load_or_create(&dir.path().join("mcp.key")).unwrap());
        let settings = Settings {
            image: IMAGE.to_owned(),
            orbit_url: "orbit-server:2200".to_owned(),
            mcp_url: "http://orbit-server:2202/mcp".to_owned(),
            mcp_tokens: Arc::clone(&tokens),
            work_max: WORK_MAX,
            logs_dir: dir.path().join("session-logs"),
        };
        let lifecycle = Arc::new(Lifecycle::new(
            Docker::new(proxy_socket),
            template,
            Arc::clone(&env.registry),
            Arc::clone(&env.ca),
            settings,
        ));
        let control = Arc::new(
            Control::new(
                Arc::clone(&env.registry),
                Arc::clone(&env.ca),
                None,
                Author {
                    name: "Iapetus".to_owned(),
                    email: "iapetus@localhost".to_owned(),
                },
                dir.path().join("docs"),
            )
            .with_lifecycle(Some(Arc::clone(&lifecycle))),
        );
        Self {
            env,
            docker,
            lifecycle,
            control,
            tokens,
            dir,
        }
    }

    /// Сессия в реестре с назначенным runner.
    fn new_session(&self) -> Fingerprint {
        let runner = self.env.enroll("runner").fingerprint;
        let mut registry = self.env.reg();
        let session = registry.create_session(Agent::ClaudeCode).unwrap();
        registry.assign_runner(&session, &runner).unwrap();
        session
    }

    fn runner_of(&self, session: &Fingerprint) -> Fingerprint {
        self.env.reg().session(session).unwrap().runner.unwrap()
    }

    /// Тело `create`, с которым контейнер сессии сейчас существует.
    fn created(&self, session: &Fingerprint) -> Value {
        let state = self.docker.lock().unwrap();
        state.container(&container_name(session)).expect("контейнера сессии нет").2.clone()
    }

    fn env_of(&self, session: &Fingerprint) -> BTreeMap<String, String> {
        self.created(session)["Env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .unwrap()
                    .split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .unwrap()
            })
            .collect()
    }

    fn say_in_container(&self, session: &Fingerprint, stream: u8, text: &str) {
        self.docker
            .lock()
            .unwrap()
            .logs
            .entry(container_name(session))
            .or_default()
            .push((stream, text.to_owned()));
    }

    /// Сохранённые журналы: имя файла и содержимое.
    fn saved_logs(&self) -> Vec<(String, String)> {
        let dir = self.dir.path().join("session-logs");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut logs: Vec<_> = entries
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(path).unwrap(),
                )
            })
            .collect();
        logs.sort();
        logs
    }

    fn lose_container(&self, session: &Fingerprint) {
        self.docker
            .lock()
            .unwrap()
            .containers
            .retain(|(name, ..)| *name != container_name(session));
    }

    fn secret_matches(&self, session: &Fingerprint, secret: &str) -> bool {
        self.env.reg().session_secret_matches(session, &SessionSecret::new(secret)).unwrap()
    }
}

#[tokio::test]
async fn each_container_gets_only_its_own_secret_and_identity() {
    let harness = Harness::new();
    let (first, second) = (harness.new_session(), harness.new_session());
    harness
        .lifecycle
        .start(&first, Agent::ClaudeCode, &harness.runner_of(&first), None)
        .await
        .unwrap();
    harness
        .lifecycle
        .start(&second, Agent::Codex, &harness.runner_of(&second), None)
        .await
        .unwrap();

    let (first_env, second_env) = (harness.env_of(&first), harness.env_of(&second));
    let (first_secret, second_secret) = (
        &first_env["IAPETUS_SESSION_SECRET_CONTENT"],
        &second_env["IAPETUS_SESSION_SECRET_CONTENT"],
    );
    assert_ne!(first_secret, second_secret, "две сессии получили один секрет");
    assert!(
        harness.secret_matches(&first, first_secret),
        "секрет контейнера не принят реестром для его сессии"
    );
    assert!(
        !harness.secret_matches(&first, second_secret),
        "секрет второй сессии подошёл первой"
    );

    let first_body = harness.created(&first).to_string();
    for foreign in [
        second_secret.as_str(),
        second.as_str(),
        second_env["IAPETUS_DATA_HOST_KEY_CONTENT"].as_str(),
    ] {
        assert!(!first_body.contains(foreign), "в контейнере первой сессии есть чужое значение");
    }

    // Хост-сертификат подписан CA server для имени, которое проверяет runner, и привязан к сессии.
    let cert = Certificate::from_openssh(&first_env["IAPETUS_DATA_HOST_CERT_CONTENT"]).unwrap();
    let trust = Trust::new(&[harness.env.ca.public_key()]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    trust
        .check_host_cert(&cert, DATA_HOST, now)
        .expect("хост-сертификат session не принят по CA server");
    assert_eq!(cert.key_id(), first.as_str());
    assert_eq!(first_env["IAPETUS_AGENT"], "claude");
    assert_eq!(first_env["IAPETUS_RUNNER"], harness.runner_of(&first).as_str());
    assert_eq!(second_env["IAPETUS_AGENT"], "codex");
    assert!(
        !first_env.contains_key("IAPETUS_RESUME"),
        "новая сессия не должна продолжать разговор"
    );
}

#[tokio::test]
async fn lost_container_is_recreated_on_the_same_volume_with_resume_and_a_new_secret() {
    let harness = Harness::new();
    let session = harness.new_session();
    harness
        .lifecycle
        .start(&session, Agent::Antigravity, &harness.runner_of(&session), Some(4096))
        .await
        .unwrap();
    let old_secret = harness.env_of(&session)["IAPETUS_SESSION_SECRET_CONTENT"].clone();

    harness.lose_container(&session);
    harness.lifecycle.restore_missing().await.unwrap();

    let body = harness.created(&session);
    assert_eq!(
        body["HostConfig"]["Mounts"][0]["Source"],
        json!(volume_name(&session)),
        "контейнер получил другой том"
    );
    assert_eq!(
        harness.docker.lock().unwrap().volumes_created,
        1,
        "том создан заново, разговор потерян"
    );
    assert_eq!(
        body["HostConfig"]["Tmpfs"]["/work"],
        json!("rw,noexec,nosuid,nodev,mode=0700,size=4096")
    );
    let env = harness.env_of(&session);
    assert_eq!(env["IAPETUS_RESUME"], "1");
    assert_eq!(env["IAPETUS_AGENT"], "antigravity");
    assert!(
        !harness.secret_matches(&session, &old_secret),
        "секрет потерянного контейнера продолжает действовать"
    );
    assert!(harness.secret_matches(&session, &env["IAPETUS_SESSION_SECRET_CONTENT"]));
}

#[tokio::test]
async fn session_in_failure_is_restored_only_by_request() {
    let harness = Harness::new();
    let session = harness.new_session();
    assert!(
        harness.lifecycle.restore_failed(&session).await.is_err(),
        "здоровую сессию восстановили по запросу"
    );
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
    {
        let mut registry = harness.env.reg();
        registry.set_session_state(&session, SessionState::Failure).unwrap();
    }
    harness.lose_container(&session);

    harness.lifecycle.restore_missing().await.unwrap();
    assert!(
        harness.docker.lock().unwrap().container(&container_name(&session)).is_none(),
        "сессию из failure подняли сами"
    );

    harness.lifecycle.restore_failed(&session).await.unwrap();
    assert_eq!(harness.env_of(&session)["IAPETUS_RESUME"], "1");
    assert_eq!(harness.env.reg().session(&session).unwrap().state, SessionState::Waiting);
}

#[tokio::test]
async fn ending_a_session_saves_the_container_log_before_it_is_removed() {
    let harness = Harness::new();
    let session = harness.new_session();
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
    harness.say_in_container(&session, 1, "orbit session запущен\n");
    harness.say_in_container(&session, 2, "агент завершился\n");

    harness.lifecycle.remove(&session).await.unwrap();

    let logs = harness.saved_logs();
    assert_eq!(logs.len(), 1, "журнал контейнера не сохранён при завершении сессии: {logs:?}");
    assert!(
        logs[0].0.starts_with(&format!("{session}-")) && std::path::Path::new(&logs[0].0).extension().is_some_and(|ext| ext == "log"),
        "имя файла: {}",
        logs[0].0
    );
    assert_eq!(
        logs[0].1, "orbit session запущен\nагент завершился\n",
        "в файле не stdout и stderr контейнера"
    );
}

#[tokio::test]
async fn restarting_a_failed_session_saves_the_log_of_the_container_it_replaces() {
    let harness = Harness::new();
    let session = harness.new_session();
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
    harness.say_in_container(&session, 1, "агент завершается снова и снова\n");
    harness.env.reg().set_session_state(&session, SessionState::Failure).unwrap();

    harness.lifecycle.restore_failed(&session).await.unwrap();

    let logs = harness.saved_logs();
    assert_eq!(logs.len(), 1, "журнал упавшего контейнера пропал при перезапуске: {logs:?}");
    assert_eq!(logs[0].1, "агент завершается снова и снова\n");
}

fn fingerprint(name: &str) -> Fingerprint {
    name.parse().unwrap()
}

#[test]
fn only_the_newest_logs_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let session = fingerprint("session-a");
    for at in [100, 300, 200, 400] {
        store_log(dir.path(), &session, format!("журнал {at}").as_bytes(), at, 3, 1 << 20).unwrap();
    }
    let mut kept: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    kept.sort();
    assert_eq!(
        kept,
        ["session-a-200.log", "session-a-300.log", "session-a-400.log"],
        "ротация оставила не три самых новых журнала"
    );
}

#[test]
fn logs_of_different_sessions_share_the_limit_and_foreign_files_stay() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "чужой файл").unwrap();
    for (name, at) in [("one", 10), ("two", 20), ("three", 30)] {
        store_log(dir.path(), &fingerprint(name), b"x", at, 2, 1 << 20).unwrap();
    }
    assert!(
        !dir.path().join("one-10.log").exists(),
        "самый старый журнал другой сессии не удалён"
    );
    assert!(dir.path().join("two-20.log").exists() && dir.path().join("three-30.log").exists());
    assert!(dir.path().join("notes.txt").exists(), "ротация удалила чужой файл");
}

#[test]
fn oversized_log_keeps_its_tail_cut_at_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let log = "первая строка\nвторая строка\nтретья строка\n";
    let path = store_log(dir.path(), &fingerprint("big"), log.as_bytes(), 1, 20, 30).unwrap();
    let saved = std::fs::read_to_string(path).unwrap();
    let (mark, tail) = saved.split_once('\n').unwrap();
    assert!(mark.starts_with("[журнал усечён"), "нет пометки об усечении: {saved:?}");
    assert_eq!(tail, "третья строка\n", "после усечения остался не целый хвост");
    let under = store_log(dir.path(), &fingerprint("small"), b"abc", 2, 20, 25).unwrap();
    assert_eq!(std::fs::read_to_string(under).unwrap(), "abc", "короткий журнал изменён");
}

#[test]
fn saved_log_is_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("session-logs");
    let path = store_log(&logs, &fingerprint("priv"), b"x", 1, 20, 100).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "журнал доступен другим"
    );
    assert_eq!(
        std::fs::metadata(&logs).unwrap().permissions().mode() & 0o777,
        0o700,
        "каталог журналов доступен другим"
    );
}

#[tokio::test]
async fn session_without_a_volume_is_not_restored_by_the_server() {
    let harness = Harness::new();
    harness.new_session();
    harness.lifecycle.restore_missing().await.unwrap();
    assert!(
        harness.docker.lock().unwrap().containers.is_empty(),
        "контейнер поднят без запроса worker"
    );
}

#[tokio::test]
async fn remove_deletes_container_volume_and_registry_entry() {
    let harness = Harness::new();
    let session = harness.new_session();
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
    harness.lifecycle.remove(&session).await.unwrap();
    let state = harness.docker.lock().unwrap();
    assert!(
        state.containers.is_empty() && state.volumes.is_empty(),
        "после завершения остались контейнер или том"
    );
    drop(state);
    assert!(harness.env.reg().session(&session).is_err(), "сессия осталась в реестре");
}

#[tokio::test]
async fn operation_on_a_busy_session_is_refused() {
    let harness = Harness::new();
    let session = harness.new_session();
    let claim = harness.lifecycle.claim(&session).unwrap();
    assert!(
        harness
            .lifecycle
            .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
            .await
            .is_err()
    );
    drop(claim);
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn work_size_above_the_limit_never_reaches_docker() {
    let harness = Harness::new();
    let session = harness.new_session();
    assert!(
        harness
            .lifecycle
            .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), Some(WORK_MAX + 1))
            .await
            .is_err()
    );
    assert!(harness.docker.lock().unwrap().volumes.is_empty());
}

#[tokio::test]
async fn container_gets_the_mcp_token_of_its_own_session() {
    let harness = Harness::new();
    let (first, second) = (harness.new_session(), harness.new_session());
    for session in [&first, &second] {
        harness
            .lifecycle
            .start(session, Agent::ClaudeCode, &harness.runner_of(session), None)
            .await
            .unwrap();
    }
    let env = harness.env_of(&first);
    assert_eq!(env["IAPETUS_MCP_URL"], "http://orbit-server:2202/mcp");
    assert_eq!(
        harness.tokens.session(&env["IAPETUS_MCP_TOKEN"]),
        Some(first.clone()),
        "токен MCP контейнера принадлежит не его сессии"
    );
    assert_ne!(
        env["IAPETUS_MCP_TOKEN"],
        harness.env_of(&second)["IAPETUS_MCP_TOKEN"],
        "две сессии получили один токен MCP"
    );
}

#[tokio::test]
async fn container_gets_no_agent_credentials_and_no_ticket() {
    let harness = Harness::new();
    let session = harness.new_session();
    harness
        .lifecycle
        .start(&session, Agent::ClaudeCode, &harness.runner_of(&session), None)
        .await
        .unwrap();
    let env = harness.env_of(&session);

    let names: Vec<&String> = env
        .keys()
        .filter(|name| ["TICKET", "OAUTH", "CREDENTIALS", "API_KEY"].iter().any(|word| name.contains(word)))
        .collect();
    assert!(names.is_empty(), "в окружение контейнера попали учётные данные агента: {names:?}");

    harness.lose_container(&session);
    harness.lifecycle.restore_missing().await.unwrap();
    let restored = harness.env_of(&session);
    assert!(
        restored.keys().all(|name| !name.contains("TICKET") && !name.contains("OAUTH")),
        "пересозданный контейнер получил учётные данные агента"
    );
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn unix_in(seconds: u64) -> u64 {
    super::super::testing::unix(std::time::SystemTime::now()) + seconds
}

/// Runner входит на канал управления, отвечает на сообщение о состоянии сессии.
async fn runner_online(harness: &Harness, runner: &Fingerprint) -> (Mux, Requests) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let control = Arc::clone(&harness.control);
    let peer = Peer::Runner {
        fingerprint: runner.clone(),
        issued: super::super::testing::unix(std::time::SystemTime::now()),
        valid_before: unix_in(3600),
    };
    tokio::spawn(async move { control.serve(peer, server).await });
    let (mux, mut requests) = Mux::new(client);
    super::super::testing::report_host(&mux).await;
    let incoming = tokio::time::timeout(WAIT, requests.recv())
        .await
        .expect("server не сообщил runner состояние сессии")
        .expect("канал runner закрыт");
    let body = nerve::protocol::encode(&SessionResponse::Ok(())).unwrap();
    incoming.respond(body.into(), true).await.unwrap();
    (mux, requests)
}

fn worker_online(harness: &Harness) -> (Mux, Requests) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let control = Arc::clone(&harness.control);
    let peer = Peer::Worker {
        issued: unix_in(0),
        valid_before: unix_in(3600),
    };
    tokio::spawn(async move { control.serve(peer, server).await });
    Mux::new(client)
}

/// Runner, которых назначить нельзя: не в сети, занят другой сессией, отозван. Ни один не должен дойти до Docker.
async fn unusable_runners(harness: &Harness) -> (Vec<(&'static str, Fingerprint, Error)>, [(Mux, Requests); 2]) {
    let offline = harness.env.enroll("offline").fingerprint;

    let busy = harness.env.enroll("busy").fingerprint;
    {
        let mut registry = harness.env.reg();
        let other = registry.create_session(Agent::Codex).unwrap();
        registry.assign_runner(&other, &busy).unwrap();
    }
    let busy_channel = runner_online(harness, &busy).await;

    let revoked = harness.env.enroll("revoked").fingerprint;
    let revoked_channel = runner_online(harness, &revoked).await;
    harness.control.revoke_runner(&revoked).unwrap();

    // Каналы нужны открытыми до конца проверки: иначе `busy` стал бы «не в сети».
    let cases = vec![
        ("не в сети", offline, Error::Io("runner не в сети".to_owned())),
        ("занят", busy, Error::Conflict),
        ("отозван", revoked, Error::Access),
    ];
    (cases, [busy_channel, revoked_channel])
}

#[tokio::test]
async fn reassigning_to_an_unusable_runner_is_refused_before_docker() {
    let harness = Harness::new();
    let session = harness.new_session();
    let previous = harness.env.reg().session(&session).unwrap();

    let (cases, _channels) = unusable_runners(&harness).await;
    for (what, runner, expected) in cases {
        let refused = harness.control.reassign(&session, &runner, true).await;
        assert_eq!(refused, Err(expected), "runner {what}: назначение не отказано как положено");
    }

    assert_eq!(
        harness.docker.lock().unwrap().requests,
        0,
        "отказ дошёл до Docker: контейнер тронут до проверок"
    );
    assert_eq!(
        harness.env.reg().session(&session).unwrap(),
        previous,
        "после отказа у сессии изменилась пара"
    );
}

/// Session входит на канал управления; каждое указание server уходит в очередь и получает ответ `Ok`.
fn session_online(harness: &Harness, session: &Fingerprint) -> (Mux, mpsc::UnboundedReceiver<ToSession>) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let control = Arc::clone(&harness.control);
    let peer = Peer::Session {
        fingerprint: session.clone(),
        ip: [127, 0, 0, 1].into(),
    };
    tokio::spawn(async move { control.serve(peer, server).await });
    let (mux, mut requests) = Mux::new(client);
    let (sent, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = requests.recv().await {
            sent.send(nerve::protocol::decode_body(incoming.body()).unwrap()).unwrap();
            let body = nerve::protocol::encode(&Ok::<(), Error>(())).unwrap();
            incoming.respond(body.into(), true).await.unwrap();
        }
    });
    (mux, received)
}

#[tokio::test]
async fn changing_the_runner_never_reaches_docker_and_the_session_restarts_its_agent() {
    let harness = Harness::new();
    let session = harness.new_session();
    // Прежний runner сообщал окружение до разрыва: session получает назначение и без него в сети.
    let old = harness.env.reg().session(&session).unwrap().runner.unwrap();
    harness.control.hosts().insert(old, super::super::testing::runner_host());
    let new = harness.env.enroll("new").fingerprint;
    let (_new_mux, mut new_requests) = runner_online(&harness, &new).await;
    let (starts, mut started) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = new_requests.recv().await {
            starts
                .send(nerve::protocol::decode_body::<ToRunner>(incoming.body()).unwrap())
                .unwrap();
            let body = nerve::protocol::encode(&SessionResponse::Ok(())).unwrap();
            incoming.respond(body.into(), true).await.unwrap();
        }
    });
    let (_session_mux, mut control) = session_online(&harness, &session);
    let announced = tokio::time::timeout(WAIT, control.recv()).await.unwrap().unwrap();
    assert!(
        matches!(announced, ToSession::AssignRunner { agent: AgentRun::Keep, .. }),
        "при подключении session получила не «не трогать агента»: {announced:?}"
    );

    harness.control.reassign(&session, &new, true).await.unwrap();

    let sent = tokio::time::timeout(WAIT, control.recv()).await.unwrap().unwrap();
    let ToSession::AssignRunner {
        runner: Some(assigned),
        agent,
    } = sent
    else {
        panic!("session получила не назначение: {sent:?}");
    };
    assert_eq!(assigned.runner, new, "session получила не нового runner");
    assert_eq!(agent, AgentRun::Restart { resume: true }, "флаг resume не дошёл до session");
    assert_eq!(harness.runner_of(&session), new, "пара не записана в реестр");
    assert_eq!(harness.docker.lock().unwrap().requests, 0, "смена runner дошла до Docker");
    let start = tokio::time::timeout(WAIT, started.recv()).await.unwrap().unwrap();
    assert!(
        matches!(start, ToRunner::Session(SessionMessage::Start { .. })),
        "новый runner не получил Start"
    );
}

#[tokio::test]
async fn starting_a_session_on_an_unusable_runner_ends_it_before_docker() {
    let harness = Harness::new();
    let (mux, _requests) = worker_online(&harness);

    let (cases, _channels) = unusable_runners(&harness).await;
    for (what, runner, expected) in cases {
        let request = WorkerRequest::Sessions(SessionsOp::Start {
            agent: Agent::ClaudeCode,
            runner,
            identity: SessionIdentity::default(),
        });
        let event: SessionEvent = tokio::time::timeout(WAIT, ask(&mux, &request))
            .await
            .expect("server не ответил")
            .expect("ответ не получен");
        let reason = match expected {
            Error::Io(_) => "runner не в сети",
            Error::Conflict => "runner занят",
            _ => "runner отозван",
        };
        assert_eq!(
            event,
            SessionEvent::Refused(reason.to_owned()),
            "сессия на runner {what} не отказана с причиной"
        );
    }

    assert_eq!(harness.docker.lock().unwrap().requests, 0, "отказ дошёл до Docker");
    let created = harness.env.reg().sessions().unwrap();
    assert!(
        created.iter().all(|session| session.agent != Agent::ClaudeCode),
        "отказанная сессия осталась в реестре"
    );
}

#[tokio::test]
async fn failed_container_start_tells_the_worker_why_and_drops_the_session() {
    let harness = Harness::new();
    let (mux, _requests) = worker_online(&harness);
    let runner = harness.env.enroll("online").fingerprint;
    let _channel = runner_online(&harness, &runner).await;
    harness.docker.lock().unwrap().fail_container_create = true;

    let request = WorkerRequest::Sessions(SessionsOp::Start {
        agent: Agent::ClaudeCode,
        runner,
        identity: SessionIdentity::default(),
    });
    let event: SessionEvent = tokio::time::timeout(WAIT, ask(&mux, &request))
        .await
        .expect("server не ответил")
        .expect("ответ не получен");
    assert_eq!(
        event,
        SessionEvent::Refused("контейнер не поднялся".to_owned()),
        "причина сбоя не дошла до worker"
    );
    assert!(
        harness.env.reg().sessions().unwrap().is_empty(),
        "сессия сбойного старта осталась в реестре"
    );
}

#[tokio::test]
async fn starting_a_session_without_a_key_creates_a_container_without_a_ticket() {
    let harness = Harness::new();
    let (mux, _requests) = worker_online(&harness);
    let runner = harness.env.enroll("online").fingerprint;
    let _channel = runner_online(&harness, &runner).await;
    let request = WorkerRequest::Sessions(SessionsOp::Start {
        agent: Agent::Codex,
        runner,
        identity: SessionIdentity::default(),
    });
    // Ответ `Ready` придёт, когда в session запустится агент; контейнер в Docker появляется раньше.
    let pending = tokio::spawn(async move { ask::<_, SessionEvent>(&mux, &request).await });

    let session = tokio::time::timeout(WAIT, async {
        loop {
            let created = harness.env.reg().sessions().unwrap().into_iter().next();
            if let Some(session) = created
                && harness
                    .docker
                    .lock()
                    .unwrap()
                    .container(&container_name(&session.fingerprint))
                    .is_some()
            {
                return session.fingerprint;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("контейнер не создан");
    pending.abort();

    let env = harness.env_of(&session);
    assert!(
        env.keys().all(|name| !name.contains("TICKET") && !name.contains("OAUTH")),
        "контейнер сессии без ключа получил билет или токен"
    );
    assert_eq!(env["IAPETUS_AGENT"], "codex", "агент не дошёл до контейнера");
}
