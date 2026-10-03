//! Прокси целиком: клиент говорит HTTP на сокет прокси, поддельный Docker — настоящий HTTP-сервер
//! на unix-сокете во временном каталоге и записывает всё, что до него дошло. Отказ проверяется
//! по двум признакам сразу: ответ 403 и то, что в Docker не ушла нужная операция.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::super::fixtures::{template, valid_create};
use super::{bind, serve};

const SESSION_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    uri: String,
    body: String,
    headers: Vec<String>,
}

impl Seen {
    fn call(&self) -> String {
        format!("{} {}", self.method, self.uri)
    }
}

fn inspect(id: &str, labels: &Value) -> String {
    json!({ "Id": id, "Config": { "Labels": labels } }).to_string()
}

/// Ответы поддельного Docker. Контейнеры: `session-1` с меткой сессии, `plain` без меток,
/// остальные — с чужими или искажёнными метками, `gone` не существует.
fn docker_reply(path: &str) -> (u16, String) {
    let path = path.strip_prefix("/v1.45").unwrap_or(path);
    let labeled = json!({ "iapetus.role": "session", "other": "x" });
    match path {
        "/containers/session-1/json" => (200, inspect(SESSION_ID, &labeled)),
        "/containers/plain/json" => (200, inspect(&"2".repeat(64), &json!({}))),
        "/containers/nolabels/json" => (200, inspect(&"2".repeat(64), &Value::Null)),
        "/containers/worker/json" => (200, inspect(&"2".repeat(64), &json!({ "iapetus.role": "worker" }))),
        "/containers/upper/json" => (200, inspect(&"2".repeat(64), &json!({ "IAPETUS.ROLE": "session" }))),
        "/containers/shortid/json" => (200, inspect("abc", &labeled)),
        "/containers/gone/json" => (404, r#"{"message":"No such container"}"#.to_owned()),
        "/volumes/claude-vol" => (
            200,
            json!({ "Name": "claude-vol", "Labels": { "iapetus.role": "session" } }).to_string(),
        ),
        "/volumes/plain-vol" => (200, json!({ "Name": "plain-vol", "Labels": null }).to_string()),
        "/volumes/missing" => (404, r#"{"message":"no such volume"}"#.to_owned()),
        "/containers/create" => (201, r#"{"Id":"3333"}"#.to_owned()),
        "/volumes/create" => (201, r#"{"Name":"claude-vol"}"#.to_owned()),
        _ => (200, "docker-answer".to_owned()),
    }
}

struct Harness {
    dir: TempDir,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Harness {
    /// Прокси и поддельный Docker.
    fn start() -> Self {
        let harness = Self::start_without_docker();
        let listener = UnixListener::bind(harness.dir.path().join("docker.sock")).unwrap();
        let seen = Arc::clone(&harness.seen);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            seen.lock().unwrap().push(Seen {
                                method: parts.method.to_string(),
                                uri: parts.uri.to_string(),
                                body: String::from_utf8(body.to_vec()).unwrap(),
                                headers: parts.headers.keys().map(|name| name.as_str().to_owned()).collect(),
                            });
                            let (status, answer) = docker_reply(parts.uri.path());
                            let response = Response::builder().status(status).body(Full::new(Bytes::from(answer)));
                            Ok::<_, std::convert::Infallible>(response.unwrap())
                        }
                    });
                    http1::Builder::new().serve_connection(TokioIo::new(stream), service).await.unwrap();
                });
            }
        });
        harness
    }

    /// Прокси, у которого `docker.sock` нет.
    fn start_without_docker() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = bind(&dir.path().join("proxy.sock")).unwrap();
        tokio::spawn(serve(listener, dir.path().join("docker.sock"), template()));
        Self { dir, seen: Arc::default() }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<String> {
        self.seen().iter().map(Seen::call).collect()
    }

    fn proxy(&self) -> PathBuf {
        self.dir.path().join("proxy.sock")
    }

    /// Шлёт байты запроса на сокет прокси и читает ответ до закрытия соединения.
    async fn raw(&self, request: &str) -> (u16, String) {
        let mut stream = UnixStream::connect(self.proxy()).await.unwrap();
        // Прокси может ответить отказом, не дочитав тело, поэтому ошибка записи не провал.
        drop(stream.write_all(request.as_bytes()).await);
        let mut response = Vec::new();
        drop(stream.read_to_end(&mut response).await);
        let response = String::from_utf8(response).unwrap();
        let status = response.split(' ').nth(1).unwrap().parse().unwrap();
        let body = response.split_once("\r\n\r\n").map_or(String::new(), |(_, body)| body.to_owned());
        (status, body)
    }

    async fn call(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let length = body.map_or(String::new(), |body| format!("Content-Length: {}\r\n", body.len()));
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n{length}\r\n{}",
            body.unwrap_or("")
        );
        self.raw(&request).await
    }

    async fn assert_denied_untouched(&self, method: &str, path: &str, body: Option<&str>) {
        let (status, answer) = self.call(method, path, body).await;
        assert_eq!(status, 403, "{method} {path} не отвергнут: {answer}");
        assert_eq!(self.calls(), Vec::<String>::new(), "{method} {path} дошёл до Docker");
    }
}

#[tokio::test]
async fn valid_create_is_forwarded_as_the_rebuilt_body() {
    let harness = Harness::start();
    let (status, answer) = harness
        .call("POST", "/containers/create?name=session-1", Some(&valid_create().to_string()))
        .await;
    assert_eq!((status, answer.as_str()), (201, r#"{"Id":"3333"}"#));
    let seen = harness.seen();
    assert_eq!(
        harness.calls(),
        ["GET /volumes/claude-vol", "POST /containers/create?name=session-1"],
        "перед create проверяется метка тома"
    );
    let expected = template().check_create(valid_create().to_string().as_bytes()).unwrap().body;
    assert_eq!(seen[1].body.as_bytes(), expected.as_slice());
}

#[tokio::test]
async fn harmless_extras_do_not_reach_docker() {
    let mut body = valid_create();
    let host = body["HostConfig"].as_object_mut().unwrap();
    host.insert("Privileged".into(), json!(false));
    host.insert("Binds".into(), json!([]));
    host.insert("PidMode".into(), json!(""));
    let harness = Harness::start();
    let (status, _) = harness.call("POST", "/containers/create?name=a", Some(&body.to_string())).await;
    assert_eq!(status, 201);
    let forwarded = &harness.seen()[1].body;
    for field in ["Privileged", "Binds", "PidMode"] {
        assert!(!forwarded.contains(field), "{field} дошёл до Docker: {forwarded}");
    }
}

#[tokio::test]
async fn create_violating_the_template_never_reaches_docker() {
    let mut cases: Vec<String> = Vec::new();
    let mut with = |pointer: &str, value: Value| {
        let mut body = valid_create();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        body.pointer_mut(parent).unwrap()[key] = value;
        cases.push(body.to_string());
    };
    with("/HostConfig/Privileged", json!(true));
    with("/HostConfig/CapAdd", json!(["SYS_ADMIN", "NET_ADMIN"]));
    with("/HostConfig/Binds", json!(["/:/host"]));
    with("/HostConfig/PidMode", json!("host"));
    with("/HostConfig/NetworkMode", json!("host"));
    with(
        "/HostConfig/Mounts",
        json!([{ "Type": "bind", "Source": "/", "Target": "/home/agent/.claude" }]),
    );
    with("/HostConfig/SecurityOpt", json!(["no-new-privileges", "seccomp=unconfined"]));
    with("/HostConfig/CapDrop", json!([]));
    with(
        "/HostConfig/Tmpfs",
        json!({ "/work": "rw,noexec,nosuid,nodev,mode=0700,size=1073741824", "/run/secrets": "rw,exec,mode=0700" }),
    );
    with("/Image", json!("registry.example/iapetus/session:latest"));
    with("/Labels", json!({}));
    with("/HostConfig/Memory", json!(0));
    cases.push(
        valid_create()
            .to_string()
            .replace("\"HostConfig\":{", "\"HostConfig\":{\"Privileged\":false,\"Privileged\":true,"),
    );
    cases.push("not json".to_owned());
    let harness = Harness::start();
    for body in cases {
        harness
            .assert_denied_untouched("POST", "/containers/create?name=a", Some(&body))
            .await;
    }
}

#[tokio::test]
async fn create_mounting_the_volume_into_another_agents_directory_never_reaches_docker() {
    let mut body = valid_create();
    body["Labels"]["iapetus.agent"] = json!("codex");
    let harness = Harness::start();
    harness
        .assert_denied_untouched("POST", "/containers/create?name=a", Some(&body.to_string()))
        .await;
    body["HostConfig"]["Mounts"][0]["Target"] = json!("/home/agent/.codex");
    let (status, _) = harness.call("POST", "/containers/create?name=a", Some(&body.to_string())).await;
    assert_eq!(status, 201);
}

#[tokio::test]
async fn create_on_a_volume_without_the_session_label_is_denied() {
    let mut body = valid_create();
    body["HostConfig"]["Mounts"][0]["Source"] = json!("plain-vol");
    let harness = Harness::start();
    let (status, _) = harness.call("POST", "/containers/create?name=a", Some(&body.to_string())).await;
    assert_eq!(status, 403);
    assert_eq!(harness.calls(), ["GET /volumes/plain-vol"]);
}

#[tokio::test]
async fn create_needs_a_name_and_nothing_else_in_the_query() {
    let harness = Harness::start();
    let body = valid_create().to_string();
    for path in [
        "/containers/create",
        "/containers/create?name=a&platform=linux",
        "/containers/create?name=a&name=b",
        "/containers/create?name=a%2Fb",
    ] {
        harness.assert_denied_untouched("POST", path, Some(&body)).await;
    }
}

#[tokio::test]
async fn operations_on_containers_without_the_session_label_stop_at_the_inspect() {
    let harness = Harness::start();
    for id in ["plain", "nolabels", "worker", "upper", "shortid"] {
        for (method, suffix) in [
            ("GET", "/json"),
            ("POST", "/start"),
            ("POST", "/stop"),
            ("DELETE", ""),
            ("GET", "/logs"),
            ("POST", "/wait"),
        ] {
            let path = format!("/containers/{id}{suffix}");
            let (status, _) = harness.call(method, &path, None).await;
            assert_eq!(status, 403, "{method} {path}");
            assert_eq!(harness.calls(), [format!("GET /containers/{id}/json")], "{method} {path}");
            harness.seen.lock().unwrap().clear();
        }
    }
}

#[tokio::test]
async fn operations_on_session_containers_use_the_full_id_and_canonical_query() {
    let harness = Harness::start();
    let cases = [
        (
            "POST",
            "/containers/session-1/start",
            None,
            format!("POST /containers/{SESSION_ID}/start"),
        ),
        (
            "POST",
            "/containers/session-1/stop?t=5",
            None,
            format!("POST /containers/{SESSION_ID}/stop?t=5"),
        ),
        (
            "DELETE",
            "/containers/session-1?v=true&force=true",
            None,
            format!("DELETE /containers/{SESSION_ID}?force=true&v=true"),
        ),
        (
            "GET",
            "/containers/session-1/logs?tail=10&stdout=true",
            None,
            format!("GET /containers/{SESSION_ID}/logs?stdout=true&tail=10"),
        ),
        (
            "POST",
            "/containers/session-1/wait?condition=next-exit",
            None,
            format!("POST /containers/{SESSION_ID}/wait?condition=next-exit"),
        ),
    ];
    for (method, path, body, expected) in cases {
        let (status, answer) = harness.call(method, path, body).await;
        assert_eq!((status, answer.as_str()), (200, "docker-answer"), "{method} {path}");
        let calls = harness.calls();
        assert_eq!(calls[0], "GET /containers/session-1/json");
        assert_eq!(calls[1], expected);
        harness.seen.lock().unwrap().clear();
    }
}

#[tokio::test]
async fn inspect_of_a_session_container_is_answered_from_the_checked_reply() {
    let harness = Harness::start();
    let (status, answer) = harness.call("GET", "/containers/session-1/json", None).await;
    assert_eq!(status, 200);
    assert!(answer.contains(SESSION_ID), "{answer}");
    assert_eq!(harness.calls(), ["GET /containers/session-1/json"]);
}

#[tokio::test]
async fn list_is_always_filtered_by_the_session_label() {
    let harness = Harness::start();
    harness.call("GET", "/containers/json?all=true", None).await;
    harness.call("GET", "/v1.45/containers/json", None).await;
    assert_eq!(
        harness.calls(),
        [
            "GET /containers/json?all=true&filters=%7B%22label%22%3A%5B%22iapetus.role%3Dsession%22%5D%7D",
            "GET /v1.45/containers/json?all=false&filters=%7B%22label%22%3A%5B%22iapetus.role%3Dsession%22%5D%7D",
        ]
    );
    harness.assert_denied_after("GET", "/containers/json?filters=%7B%7D").await;
}

impl Harness {
    /// Отказ без нового обращения к Docker: список вызовов не вырос.
    async fn assert_denied_after(&self, method: &str, path: &str) {
        let before = self.calls();
        let (status, _) = self.call(method, path, None).await;
        assert_eq!(status, 403, "{method} {path}");
        assert_eq!(self.calls(), before, "{method} {path} дошёл до Docker");
    }
}

#[tokio::test]
async fn forbidden_endpoints_never_reach_docker() {
    let harness = Harness::start();
    for (method, path) in [
        ("POST", "/containers/session-1/exec"),
        ("POST", "/exec/abc/start"),
        ("POST", "/containers/session-1/attach"),
        ("GET", "/containers/session-1/attach/ws"),
        ("GET", "/containers/session-1/archive"),
        ("PUT", "/containers/session-1/archive"),
        ("HEAD", "/containers/session-1/archive"),
        ("POST", "/build"),
        ("POST", "/commit"),
        ("POST", "/containers/session-1/kill"),
        ("POST", "/containers/session-1/restart"),
        ("POST", "/containers/session-1/update"),
        ("POST", "/containers/session-1/rename"),
        ("GET", "/containers/session-1/export"),
        ("POST", "/containers/prune"),
        ("POST", "/volumes/prune"),
        ("GET", "/volumes"),
        ("POST", "/images/create"),
        ("POST", "/networks/create"),
        ("GET", "/info"),
        ("GET", "/version"),
        ("GET", "/events"),
        ("POST", "/auth"),
    ] {
        harness.assert_denied_untouched(method, path, None).await;
    }
}

#[tokio::test]
async fn path_tricks_never_reach_docker() {
    let harness = Harness::start();
    for (method, path) in [
        ("POST", "/containers/%63reate?name=a"),
        ("POST", "/Containers/create?name=a"),
        ("POST", "//containers/create?name=a"),
        ("POST", "/containers/create/?name=a"),
        ("POST", "/containers/./create?name=a"),
        ("POST", "/containers/session-1/../create?name=a"),
        ("post", "/containers/create?name=a"),
        ("POST", "/v1.45/../containers/session-1/exec"),
        ("POST", "/containers/session-1%2Fexec"),
        ("POST", "/containers/session-1/exec%00"),
        ("POST", "/containers/%2e%2e/start"),
        ("GET", "/containers/session-1/archive?path=%2F"),
    ] {
        harness.assert_denied_untouched(method, path, Some("{}")).await;
    }
}

#[tokio::test]
async fn body_is_only_accepted_where_it_is_expected() {
    let harness = Harness::start();
    harness.assert_denied_untouched("GET", "/containers/json", Some("{}")).await;
    harness
        .assert_denied_untouched("POST", "/containers/session-1/start", Some("{}"))
        .await;
    let chunked = "POST /containers/session-1/start HTTP/1.1\r\nHost: p\r\nConnection: close\r\n\
                   Transfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
    assert_eq!(harness.raw(chunked).await.0, 403);
    assert_eq!(harness.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn oversized_create_body_is_denied() {
    let harness = Harness::start();
    let padding = "a".repeat(70_000);
    let mut body = valid_create();
    body["Env"] = json!([format!("IAPETUS_X={padding}")]);
    harness
        .assert_denied_untouched("POST", "/containers/create?name=a", Some(&body.to_string()))
        .await;
}

#[tokio::test]
async fn client_headers_are_not_forwarded() {
    let harness = Harness::start();
    let request = "POST /containers/session-1/start HTTP/1.1\r\nHost: p\r\nConnection: close\r\n\
                   X-Evil: 1\r\nAuthorization: Bearer x\r\nUpgrade: tcp\r\n\r\n";
    assert_eq!(harness.raw(request).await.0, 200);
    for seen in harness.seen() {
        let mut headers = seen.headers.clone();
        headers.sort();
        assert_eq!(headers, ["host"], "{} получил заголовки клиента", seen.call());
    }
}

#[tokio::test]
async fn volume_create_forwards_only_name_and_session_label() {
    let harness = Harness::start();
    let sent = r#"{ "Labels": {"iapetus.role": "session"}, "Name": "claude-vol" }"#;
    let (status, _) = harness.call("POST", "/volumes/create", Some(sent)).await;
    assert_eq!(status, 201);
    assert_eq!(
        harness.seen()[0].body,
        r#"{"Name":"claude-vol","Labels":{"iapetus.role":"session"}}"#
    );
}

#[tokio::test]
async fn volume_with_driver_options_never_reaches_docker() {
    let harness = Harness::start();
    for body in [
        r#"{"Name":"v","Labels":{"iapetus.role":"session"},"DriverOpts":{"type":"none","o":"bind","device":"/"}}"#,
        r#"{"Name":"v","Labels":{"iapetus.role":"session"},"Driver":"local","DriverOpts":{}}"#,
        r#"{"Name":"v","Labels":{"iapetus.role":"session"},"Driver":"other"}"#,
        r#"{"Name":"v"}"#,
        r#"{"Name":"v","Labels":{"iapetus.role":"session"},"Name":"w"}"#,
    ] {
        harness.assert_denied_untouched("POST", "/volumes/create", Some(body)).await;
    }
}

#[tokio::test]
async fn volumes_without_the_session_label_cannot_be_read_or_removed() {
    let harness = Harness::start();
    for method in ["GET", "DELETE"] {
        let (status, _) = harness.call(method, "/volumes/plain-vol", None).await;
        assert_eq!(status, 403, "{method}");
        assert_eq!(harness.calls(), ["GET /volumes/plain-vol"], "{method}");
        harness.seen.lock().unwrap().clear();
    }
    let (status, _) = harness.call("DELETE", "/volumes/claude-vol", None).await;
    assert_eq!(status, 200);
    assert_eq!(harness.calls(), ["GET /volumes/claude-vol", "DELETE /volumes/claude-vol"]);
}

#[tokio::test]
async fn proxy_socket_is_private_and_replaces_a_stale_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.sock");
    std::fs::write(&path, "stale").unwrap();
    let _listener = bind(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
