use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::Author;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::{Control, Tokens, serve};
use crate::server::testing::Env;

/// Тело вызова инструмента в формате, который Claude Code шлёт без сессий MCP.
fn call_body(tool: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "1" },
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    })
}

struct Stand {
    env: Env,
    tokens: Arc<Tokens>,
    addr: SocketAddr,
}

/// Результат вызова: текст инструмента и признак ошибки инструмента.
struct Outcome {
    text: String,
    is_error: bool,
}

impl Stand {
    async fn start() -> Self {
        let env = Env::new();
        let tokens = Arc::new(Tokens::load_or_create(&env.dir.path().join("mcp.key")).unwrap());
        let control = Arc::new(Control::new(
            Arc::clone(&env.registry),
            Arc::clone(&env.ca),
            None,
            Author {
                name: "Iapetus".to_owned(),
                email: "iapetus@localhost".to_owned(),
            },
            env.dir.path().join("docs"),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, control, Arc::clone(&tokens)));
        Self { env, tokens, addr }
    }

    fn session(&self) -> Fingerprint {
        self.env.reg().create_session(nerve::protocol::Agent::ClaudeCode).unwrap()
    }

    async fn post(&self, token: Option<&str>, body: &Value) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/mcp", self.addr))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", body["method"].as_str().unwrap())
            .header("Mcp-Name", body["params"]["name"].as_str().unwrap_or_default())
            .body(body.to_string());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        request.send().await.unwrap()
    }

    async fn call(&self, token: &str, tool: &str, arguments: &Value) -> Outcome {
        let response = self.post(Some(token), &call_body(tool, arguments)).await;
        let status = response.status();
        let reply_bytes = response.bytes().await.unwrap();
        assert_eq!(
            status,
            200,
            "вызов {tool} с действующим токеном не дошёл до MCP: {}",
            String::from_utf8_lossy(&reply_bytes)
        );
        let reply: Value = serde_json::from_slice(&reply_bytes).unwrap();
        let result = &reply["result"];
        Outcome {
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("в ответе нет текста: {reply}"))
                .to_owned(),
            is_error: result["isError"].as_bool().unwrap_or(false),
        }
    }
}

#[tokio::test]
async fn request_without_token_is_rejected() {
    let stand = Stand::start().await;
    let response = stand.post(None, &call_body("runner_list", &json!({}))).await;
    assert_eq!(response.status(), 401, "MCP отвечает без токена");
}

#[tokio::test]
async fn token_signed_with_another_key_is_rejected() {
    let stand = Stand::start().await;
    let session = stand.session();
    let other = Tokens::load_or_create(&stand.env.dir.path().join("other.key")).unwrap();
    let response = stand
        .post(Some(&other.issue(&session)), &call_body("runner_list", &json!({})))
        .await;
    assert_eq!(response.status(), 401, "MCP принял токен, подписанный не ключом server");
}

#[tokio::test]
async fn token_cannot_be_moved_to_another_session() {
    let stand = Stand::start().await;
    let (mine, theirs) = (stand.session(), stand.session());
    let (_, tag) = stand
        .tokens
        .issue(&mine)
        .split_once('.')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap();
    let forged = format!("{theirs}.{tag}");
    let response = stand.post(Some(&forged), &call_body("session_status", &json!({}))).await;
    assert_eq!(response.status(), 401, "подпись одной сессии подошла другой");
}

#[tokio::test]
async fn malformed_tokens_are_rejected() {
    let stand = Stand::start().await;
    let session = stand.session();
    let valid = stand.tokens.issue(&session);
    for token in [
        "",
        ".",
        "x",
        &format!("{session}."),
        &format!("{session}.zz"),
        &format!("{valid}00"),
        &valid[..valid.len() - 2],
    ] {
        let response = stand.post(Some(token), &call_body("runner_list", &json!({}))).await;
        assert_eq!(response.status(), 401, "MCP принял повреждённый токен {token:?}");
    }
}

#[tokio::test]
async fn bearer_scheme_is_required() {
    let stand = Stand::start().await;
    let token = stand.tokens.issue(&stand.session());
    let response = reqwest::Client::new()
        .post(format!("http://{}/mcp", stand.addr))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", token)
        .body(call_body("runner_list", &json!({})).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401, "MCP принял токен без схемы Bearer");
}

#[tokio::test]
async fn request_with_origin_is_forbidden_even_with_a_valid_token() {
    let stand = Stand::start().await;
    let token = stand.tokens.issue(&stand.session());
    let body = call_body("session_status", &json!({}));
    let post = |origin: Option<&'static str>| {
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/mcp", stand.addr))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "session_status")
            .header("Authorization", format!("Bearer {token}"))
            .body(body.to_string());
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        async move { request.send().await.unwrap() }
    };
    for origin in ["https://evil.example", "http://127.0.0.1", "null"] {
        assert_eq!(post(Some(origin)).await.status(), 403, "MCP принял запрос с Origin {origin}");
    }
    assert_eq!(post(None).await.status(), 200, "запрос без Origin отклонён");
}

/// Инструменты действуют на сессию токена: у двух сессий разные runner, и каждая видит свой.
#[tokio::test]
async fn tools_act_only_on_the_session_of_the_token() {
    let stand = Stand::start().await;
    let (with_runner, without_runner) = (stand.session(), stand.session());
    let runner = stand.env.enroll("workstation");
    stand.env.reg().assign_runner(&with_runner, &runner.fingerprint).unwrap();

    let own = stand.call(&stand.tokens.issue(&with_runner), "session_status", &json!({})).await;
    assert!(!own.is_error, "session_status своей сессии вернул ошибку: {}", own.text);
    let own: Value = serde_json::from_str(&own.text).unwrap();
    assert_eq!(
        own["runner"],
        runner.fingerprint.as_str(),
        "session_status не показал runner своей сессии"
    );

    let other = stand.call(&stand.tokens.issue(&without_runner), "session_status", &json!({})).await;
    let other: Value = serde_json::from_str(&other.text).unwrap();
    assert_eq!(other["runner"], Value::Null, "session_status показал runner чужой сессии");

    // runner_clone берёт runner сессии токена: у сессии без runner клонировать нечего.
    let clone = stand
        .call(&stand.tokens.issue(&without_runner), "runner_clone", &json!({ "dir": "/work/a" }))
        .await;
    assert!(
        clone.is_error && clone.text.contains("runner не назначен"),
        "клон ушёл на runner чужой сессии: {}",
        clone.text
    );
    // У сессии с runner дело доходит до runner: он не в сети.
    let clone = stand
        .call(&stand.tokens.issue(&with_runner), "runner_clone", &json!({ "dir": "/work/a" }))
        .await;
    assert!(
        clone.is_error && clone.text.contains("не в сети"),
        "клон не пошёл на runner своей сессии: {}",
        clone.text
    );
}

#[tokio::test]
async fn runner_list_shows_registered_runners_with_presence() {
    let stand = Stand::start().await;
    let token = stand.tokens.issue(&stand.session());
    let runner = stand.env.enroll("workstation");
    let listed = stand.call(&token, "runner_list", &json!({})).await;
    let listed: Value = serde_json::from_str(&listed.text).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["fingerprint"], runner.fingerprint.as_str());
    assert_eq!(listed[0]["name"], "workstation");
    assert_eq!(
        listed[0]["presence"], "Offline",
        "runner без канала управления показан как доступный"
    );
    assert_eq!(listed[0]["temporary"], false);
}

#[tokio::test]
async fn runner_use_refuses_runner_that_is_not_online() {
    let stand = Stand::start().await;
    let session = stand.session();
    stand.env.enroll("workstation");
    let used = stand
        .call(&stand.tokens.issue(&session), "runner_use", &json!({ "runner": "workstation" }))
        .await;
    assert!(
        used.is_error && used.text.contains("не в сети"),
        "сессии назначен runner без канала управления: {}",
        used.text
    );
    assert!(
        stand.env.reg().session(&session).unwrap().runner.is_none(),
        "назначение записано, хотя runner не в сети"
    );
}

#[tokio::test]
async fn token_of_removed_session_is_useless() {
    let stand = Stand::start().await;
    let session = stand.session();
    let token = stand.tokens.issue(&session);
    stand.env.reg().remove_session(&session).unwrap();
    let status = stand.call(&token, "session_status", &json!({})).await;
    assert!(status.is_error, "удалённая сессия получила состояние: {}", status.text);
}

#[test]
fn key_file_is_stable_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.key");
    let session: Fingerprint = "abc".parse().unwrap();
    let first = Tokens::load_or_create(&path).unwrap().issue(&session);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "ключ MCP создан не с правами 0600"
    );
    let again = Tokens::load_or_create(&path).unwrap().issue(&session);
    assert_eq!(first, again, "после перезапуска server выданные токены перестали действовать");
}

#[test]
fn key_file_readable_by_others_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.key");
    Tokens::load_or_create(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Tokens::load_or_create(&path).is_err(), "ключ с правами шире 0600 принят");
}

/// Codex говорит по версии 2025-06-18: `initialize`, затем вызовы. Сессий MCP server не ведёт, но такой клиент
/// должен получить и инициализацию, и инструменты.
#[tokio::test]
async fn client_of_the_previous_protocol_initializes_and_calls_tools() {
    let stand = Stand::start().await;
    let token = stand.tokens.issue(&stand.session());
    let send = |body: Value| {
        let (token, url) = (token.clone(), format!("http://{}/mcp", stand.addr));
        async move {
            reqwest::Client::new()
                .post(url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .header("MCP-Protocol-Version", "2025-06-18")
                .header("Authorization", format!("Bearer {token}"))
                .body(body.to_string())
                .send()
                .await
                .unwrap()
        }
    };
    let init = send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "codex", "version": "1" } },
    }))
    .await;
    assert_eq!(init.status(), 200, "initialize отклонён");
    let init: Value = serde_json::from_slice(&init.bytes().await.unwrap()).unwrap();
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "сервер не объявил инструменты: {init}"
    );

    let listed = send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
    assert_eq!(listed.status(), 200, "tools/list отклонён");
    let listed: Value = serde_json::from_slice(&listed.bytes().await.unwrap()).unwrap();
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["runner_clone", "runner_list", "runner_use", "session_status"],
        "набор инструментов изменился"
    );

    let status =
        send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "session_status", "arguments": {} } })).await;
    assert_eq!(status.status(), 200, "tools/call отклонён");
}
