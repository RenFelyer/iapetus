//! Ответы, по которым сессия определяет своё состояние: принятые и отвергнутые
//! учётные данные, лимит подписки со временем сброса, ошибки, которые агент
//! повторяет или нет, — для каждого API. Формы ответов — как у настоящих Claude Code, Codex
//! и Antigravity.

use std::io;

use crate::session::gateway::{Api, GatewayEvent};
use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::{StreamExt, stream};
use insta::assert_snapshot;
use serde_json::{Value, json};

use super::common::*;

/// Ответ upstream и запрос, на который он пришёл.
struct Case {
    name: &'static str,
    path: &'static str,
    /// Запрос несёт `Authorization`, как все запросы агентов к модели.
    credentials: bool,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Case {
    fn new(name: &'static str, path: &'static str, status: u16, body: &Value) -> Self {
        Self {
            name,
            path,
            credentials: true,
            status,
            headers: vec![("content-type", "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn without_credentials(mut self) -> Self {
        self.credentials = false;
        self
    }

    fn raw_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }
}

fn request_body(api: Api) -> &'static str {
    match api {
        Api::Messages => MAIN_REQUEST,
        Api::Responses => "{}",
        Api::Gemini => GEMINI_MAIN_REQUEST,
    }
}

/// Прогоняет каждый случай через свой шлюз: ответ обязан дойти до клиента байт в
/// байт. Строка дампа — `имя: события`, `-` — событий нет.
async fn table(api: Api, cases: Vec<Case>) -> String {
    let mut lines = Vec::new();
    for case in cases {
        let mut reply = Response::builder().status(case.status);
        for (name, value) in &case.headers {
            reply = reply.header(*name, value.as_str());
        }
        let reply = reply.body(Body::from(case.body.clone())).unwrap();
        let mut gateway = spawn_gateway_for(api, spawn_upstream(respond_once(reply)).await).await;
        let mut request = client().post(gateway.url(case.path)).body(request_body(api));
        if case.credentials {
            request = request.header(header::AUTHORIZATION, "Bearer probe-token");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), case.status, "{}: статус изменён", case.name);
        let received = response.bytes().await.unwrap();
        assert_eq!(received, case.body, "{}: ответ дошёл не байт в байт", case.name);
        // Окна лимитов проверяются отдельно: здесь только события состояния.
        let mut events = gateway.drain_events().await;
        events.retain(|event| !matches!(event, GatewayEvent::Limits(_)));
        let events = if events.is_empty() {
            "-".to_owned()
        } else {
            dump(&events).replace('\n', "; ")
        };
        lines.push(format!("{}: {events}", case.name));
    }
    lines.join("\n")
}

fn anthropic_error(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

#[tokio::test(start_paused = true)]
async fn messages_responses_give_state_events() {
    const MESSAGES: &str = "/v1/messages?beta=true";
    let limit = || anthropic_error("rate_limit_error", "Usage limit reached");
    let cases = vec![
        Case::new("200 с ключом", MESSAGES, 200, &json!({})),
        Case::new("200 без ключа", MESSAGES, 200, &json!({})).without_credentials(),
        // Служебные запросы Claude Code с тем же ключом: флаги и телеметрия.
        Case::new("200 /api с ключом", "/api/claude_cli/bootstrap", 200, &json!({})),
        Case::new(
            "401 ключ API",
            MESSAGES,
            401,
            &anthropic_error("authentication_error", "invalid x-api-key"),
        ),
        Case::new(
            "401 токен подписки",
            MESSAGES,
            401,
            &anthropic_error(
                "authentication_error",
                "OAuth token has expired. Please obtain a new token or refresh your existing token.",
            ),
        ),
        Case::new(
            "403",
            MESSAGES,
            403,
            &anthropic_error(
                "permission_error",
                "OAuth authentication is currently not allowed for this organization.",
            ),
        ),
        Case::new(
            "403 /api",
            "/api/claude_code_penguin_mode",
            403,
            &anthropic_error("permission_error", "x"),
        ),
        Case::new("401 count_tokens", "/v1/messages/count_tokens", 401, &json!("не json")),
        Case::new("401 в неизвестной кодировке", MESSAGES, 401, &json!({}))
            .header("content-encoding", "x-unknown")
            .raw_body(b"\x00\x01"),
        Case::new("429 rejected со сбросом", MESSAGES, 429, &limit())
            .header("anthropic-ratelimit-unified-status", "rejected")
            .header("anthropic-ratelimit-unified-reset", "1790900000")
            .header("anthropic-ratelimit-unified-5h-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-reset", "1790800000"),
        // Без общего сброса — самый поздний среди отказавших окон; окно `allowed` не в счёт.
        Case::new("429 rejected, сброс по окнам", MESSAGES, 429, &limit())
            .header("anthropic-ratelimit-unified-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-reset", "1790800000")
            .header("anthropic-ratelimit-unified-7d-status", "rejected")
            .header("anthropic-ratelimit-unified-7d-reset", "1791400000")
            .header("anthropic-ratelimit-unified-7d_opus-status", "allowed")
            .header("anthropic-ratelimit-unified-7d_opus-reset", "1799999999"),
        Case::new("429 rejected, сброс испорчен", MESSAGES, 429, &limit())
            .header("anthropic-ratelimit-unified-status", "rejected")
            .header("anthropic-ratelimit-unified-reset", "завтра"),
        Case::new("429 без rejected", MESSAGES, 429, &limit()).header("anthropic-ratelimit-unified-reset", "1790900000"),
        Case::new(
            "400 prompt is too long",
            MESSAGES,
            400,
            &anthropic_error("invalid_request_error", "prompt is too long: 250000 tokens > 200000 maximum"),
        ),
        Case::new("400 прочее", MESSAGES, 400, &anthropic_error("invalid_request_error", "max_tokens")),
        Case::new("500", MESSAGES, 500, &anthropic_error("api_error", "Internal server error")),
        Case::new("529", MESSAGES, 529, &anthropic_error("overloaded_error", "Overloaded")),
        Case::new("503 без тела", MESSAGES, 503, &json!(null)).raw_body(b""),
        Case::new("404", MESSAGES, 404, &anthropic_error("not_found_error", "model")),
    ];
    assert_snapshot!(table(Api::Messages, cases).await, @r#"
    200 с ключом: Accepted
    200 без ключа: -
    200 /api с ключом: -
    401 ключ API: Unauthorized { status: 401, code: Some("authentication_error") }
    401 токен подписки: Unauthorized { status: 401, code: Some("authentication_error") }
    403: Unauthorized { status: 403, code: Some("permission_error") }
    403 /api: -
    401 count_tokens: Unauthorized { status: 401, code: None }
    401 в неизвестной кодировке: Unauthorized { status: 401, code: None }
    429 rejected со сбросом: SubscriptionLimitReached { reset: Some(At(1790900000)) }
    429 rejected, сброс по окнам: SubscriptionLimitReached { reset: Some(At(1791400000)) }
    429 rejected, сброс испорчен: SubscriptionLimitReached { reset: None }
    429 без rejected: RequestRateLimited
    400 prompt is too long: ContextOverflow
    400 прочее: -
    500: ServerError { status: 500, code: Some("api_error") }
    529: ServerError { status: 529, code: Some("overloaded_error") }
    503 без тела: ServerError { status: 503, code: None }
    404: -
    "#);
}

#[tokio::test(start_paused = true)]
async fn responses_responses_give_state_events() {
    const PATH: &str = "/responses";
    let usage_limit = |extra: Value| {
        let mut error = json!({"type": "usage_limit_reached", "message": "The usage limit has been reached", "plan_type": "plus"});
        error.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        json!({ "error": error })
    };
    let cases = vec![
        Case::new("200", PATH, 200, &json!({})),
        Case::new(
            "401",
            PATH,
            401,
            &json!({"error": {"message": "Invalid API key", "code": "invalid_api_key"}}),
        ),
        Case::new("403 HTML", PATH, 403, &json!(null)).raw_body(b"<html>Forbidden</html>"),
        Case::new(
            "429 usage_limit_reached",
            PATH,
            429,
            &usage_limit(json!({"resets_at": 1_790_900_000, "resets_in_seconds": 3600})),
        )
        .header("x-codex-primary-used-percent", "100")
        .header("x-codex-primary-reset-at", "1790800000"),
        Case::new(
            "429 usage_limit_reached, только через сколько",
            PATH,
            429,
            &usage_limit(json!({"resets_in_seconds": 3600})),
        ),
        Case::new("429 usage_limit_reached, сброс в заголовках", PATH, 429, &usage_limit(json!({})))
            .header("x-codex-primary-used-percent", "100")
            .header("x-codex-primary-reset-at", "1790900000")
            .header("x-codex-secondary-used-percent", "30")
            .header("x-codex-secondary-reset-at", "1791400000"),
        // Тело не разобрать, но окно израсходовано: это лимит подписки, а не частота.
        Case::new("429 без тела, окно на 100 %", PATH, 429, &json!(null))
            .raw_body(b"")
            .header("x-codex-secondary-used-percent", "100.0")
            .header("x-codex-secondary-reset-at", "1791400000"),
        Case::new(
            "429 rate_limit_exceeded",
            PATH,
            429,
            &json!({"error": {"type": "rate_limit_exceeded", "message": "slow down"}}),
        )
        .header("x-codex-primary-used-percent", "99.5")
        .header("x-codex-primary-reset-at", "1790900000"),
        Case::new("500", PATH, 500, &json!({"error": {"message": "boom"}})),
        Case::new("502", PATH, 502, &json!(null)).raw_body(b"bad gateway"),
        Case::new("400", PATH, 400, &json!({"error": {"message": "bad", "code": "invalid_value"}})),
    ];
    assert_snapshot!(table(Api::Responses, cases).await, @r#"
    200: Accepted
    401: Unauthorized { status: 401, code: Some("invalid_api_key") }
    403 HTML: Unauthorized { status: 403, code: None }
    429 usage_limit_reached: SubscriptionLimitReached { reset: Some(At(1790900000)) }
    429 usage_limit_reached, только через сколько: SubscriptionLimitReached { reset: Some(After(3600s)) }
    429 usage_limit_reached, сброс в заголовках: SubscriptionLimitReached { reset: Some(At(1790900000)) }
    429 без тела, окно на 100 %: SubscriptionLimitReached { reset: Some(At(1791400000)) }
    429 rate_limit_exceeded: UnretriedError { status: 429, code: Some("rate_limit_exceeded") }
    500: ServerError { status: 500, code: None }
    502: ServerError { status: 502, code: None }
    400: -
    "#);
}

fn google_error(code: u16, status: &str, message: &str, details: &Value) -> Value {
    json!({"error": {"code": code, "message": message, "status": status, "details": details}})
}

#[tokio::test(start_paused = true)]
async fn gemini_errors_give_state_events() {
    const PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
    let key_invalid = json!({"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID",
                             "domain": "googleapis.com"});
    let cases = vec![
        Case::new("200", PATH, 200, &json!({})),
        Case::new(
            "401",
            PATH,
            401,
            &google_error(
                401,
                "UNAUTHENTICATED",
                "API keys are not supported by this API. Expected OAuth2 access token.",
                &json!([]),
            ),
        ),
        Case::new(
            "403",
            PATH,
            403,
            &google_error(403, "PERMISSION_DENIED", "The caller does not have permission", &json!([])),
        ),
        // Неверный ключ Gemini API — 400, а не 401.
        Case::new(
            "400 API key not valid",
            PATH,
            400,
            &google_error(
                400,
                "INVALID_ARGUMENT",
                "API key not valid. Please pass a valid API key.",
                &json!([]),
            ),
        ),
        Case::new(
            "400 API_KEY_INVALID",
            PATH,
            400,
            &google_error(400, "INVALID_ARGUMENT", "x", &json!([key_invalid])),
        ),
        Case::new(
            "400 контекст",
            PATH,
            400,
            &google_error(
                400,
                "INVALID_ARGUMENT",
                "The input token count (1100000) exceeds the maximum number of tokens allowed (1048576).",
                &json!([]),
            ),
        ),
        // Слово «token» само по себе — не переполнение контекста и не отказ в ключе.
        Case::new(
            "400 прочее",
            PATH,
            400,
            &google_error(400, "INVALID_ARGUMENT", "Invalid token in the request.", &json!([])),
        ),
        Case::new(
            "500",
            PATH,
            500,
            &google_error(500, "INTERNAL", "exceeds the maximum number of tokens", &json!([])),
        ),
        Case::new(
            "503",
            PATH,
            503,
            &google_error(503, "UNAVAILABLE", "The model is overloaded. Please try again later.", &json!([])),
        ),
    ];
    assert_snapshot!(table(Api::Gemini, cases).await, @r#"
    200: Accepted
    401: Unauthorized { status: 401, code: Some("UNAUTHENTICATED") }
    403: Unauthorized { status: 403, code: Some("PERMISSION_DENIED") }
    400 API key not valid: Unauthorized { status: 400, code: Some("INVALID_ARGUMENT") }
    400 API_KEY_INVALID: Unauthorized { status: 400, code: Some("API_KEY_INVALID") }
    400 контекст: ContextOverflow
    400 прочее: UnretriedError { status: 400, code: Some("INVALID_ARGUMENT") }
    500: UnretriedError { status: 500, code: Some("INTERNAL") }
    503: ServerError { status: 503, code: Some("UNAVAILABLE") }
    "#);
}

#[tokio::test(start_paused = true)]
async fn error_body_cut_by_upstream_still_gives_the_status_event_and_the_cut() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(2);
    let chunks = stream::unfold(rx, |mut rx| async { rx.recv().await.map(|chunk| (chunk, rx)) });
    let reply = Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(chunks))
        .unwrap();
    let mut gateway = spawn_gateway(spawn_upstream(respond_once(reply)).await).await;
    let head = br#"{"type":"error","error":{"type":"authenti"#;
    tx.send(Ok(Bytes::from_static(head))).await.unwrap();

    let response = client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut body = response.bytes_stream();
    // Обрыв — только когда клиент получил начало: так заголовки точно ушли раньше.
    assert_eq!(body.next().await.unwrap().unwrap(), &head[..]);
    tx.send(Err(io::Error::new(io::ErrorKind::ConnectionReset, "обрыв"))).await.unwrap();
    assert!(body.next().await.unwrap().is_err(), "клиент принял оборванное тело за целое");

    assert_snapshot!(dump(&gateway.drain_events().await), @"
    Unauthorized { status: 401, code: None }
    UpstreamBroken
    ");
}

#[tokio::test(start_paused = true)]
async fn gemini_api_key_in_the_query_counts_as_credentials() {
    let reply = Response::builder().body(Body::from("{}")).unwrap();
    let mut gateway = spawn_gateway_for(Api::Gemini, spawn_upstream(respond_once(reply)).await).await;

    client()
        .post(gateway.url("/v1beta/models/m:generateContent?alt=sse&key=probe"))
        .body(GEMINI_MAIN_REQUEST)
        .send()
        .await
        .unwrap();

    assert_eq!(gateway.drain_events().await, [GatewayEvent::Accepted]);
}
