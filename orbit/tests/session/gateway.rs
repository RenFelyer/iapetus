//! Шлюз против поддельного upstream, поднятого в этом же процессе: HTTP на
//! входе, события шлюза и запросы, дошедшие до upstream, на выходе.

#[path = "gateway/activity.rs"]
mod activity;
#[path = "gateway/common.rs"]
mod common;
#[path = "gateway/compression.rs"]
mod compression;
#[path = "gateway/errors.rs"]
mod errors;
#[path = "gateway/gemini_upstream.rs"]
mod gemini_upstream;
#[path = "gateway/proxy_env.rs"]
mod proxy_env;
#[path = "gateway/responses_upstream.rs"]
mod responses_upstream;
#[path = "gateway/service_requests.rs"]
mod service_requests;

use std::fmt::Write as _;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::session::gateway::GatewayEvent;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::*;
use futures_util::StreamExt;
use insta::assert_snapshot;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, advance};

/// Прогоняет запрос через шлюз и дочитывает ответ до конца.
async fn post_and_read(gateway: &Gateway, path: &str, body: impl Into<reqwest::Body>) -> Bytes {
    client()
        .post(gateway.url(path))
        .body(body)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
}

/// Пишет в тело ответа upstream пинги, пока шлюз его читает.
async fn keep_writing(tx: mpsc::Sender<Bytes>) {
    while tx.send(Bytes::from_static(b": ping\n\n")).await.is_ok() {
        tokio::task::yield_now().await;
    }
}

// --- Проксирование -----------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn sse_arrives_at_the_client_as_it_leaves_upstream() {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_response(body))).await;
    let gateway = spawn_gateway(upstream).await;

    let first = message_start().text;
    let rest = [sse("ping", json!({"type": "ping"})), text_of(&message_end("end_turn"))].concat();
    // Второй кусок режет событие посреди строки.
    let (rest_head, rest_tail) = rest.split_at(rest.len() / 2);

    tx.send(Bytes::from(first.clone())).await.unwrap();
    let response = client()
        .post(gateway.url("/v1/messages?beta=true"))
        .body(MAIN_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
    let mut chunks = response.bytes_stream();

    // Если шлюз копит ответ до конца, тест зависает здесь: upstream молчит.
    let got = chunks.next().await.unwrap().unwrap();
    assert_eq!(got, first.as_bytes(), "первое событие искажено");

    tx.send(Bytes::from(rest_head.to_owned())).await.unwrap();
    tx.send(Bytes::from(rest_tail.to_owned())).await.unwrap();
    drop(tx);

    let mut all = got.to_vec();
    while let Some(chunk) = chunks.next().await {
        all.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(all, format!("{first}{rest}").as_bytes(), "поток дошёл не байт в байт");
}

/// Заголовки, с которыми запрос дошёл до upstream.
async fn forwarded_request_headers(send: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> HeaderMap {
    let (upstream, recorder) = recording_upstream().await;
    let gateway = spawn_gateway(upstream).await;
    send(client().get(gateway.url("/v1/models"))).send().await.unwrap();
    recorder.only_request().headers
}

#[tokio::test(start_paused = true)]
async fn credentials_reach_upstream_unchanged() {
    let headers = forwarded_request_headers(|request| {
        request
            .header("x-api-key", "sk-ant-test")
            .header(header::AUTHORIZATION, "Bearer sk-ant-oat-test")
            .header("anthropic-version", "2023-06-01")
    })
    .await;
    assert_eq!(headers["x-api-key"], "sk-ant-test");
    assert_eq!(headers[header::AUTHORIZATION], "Bearer sk-ant-oat-test");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
}

#[tokio::test(start_paused = true)]
async fn headers_named_in_connection_are_not_forwarded() {
    let headers = forwarded_request_headers(|request| {
        request
            .header(header::CONNECTION, "X-Hop, x-other-hop")
            .header("x-hop", "1")
            .header("x-other-hop", "1")
            .header("x-end-to-end", "1")
    })
    .await;
    assert!(!headers.contains_key("x-hop"), "заголовок из Connection ушёл на upstream");
    assert!(!headers.contains_key("x-other-hop"), "второе имя из Connection ушло на upstream");
    assert!(headers.contains_key("x-end-to-end"), "обычный заголовок потерян");
}

#[tokio::test(start_paused = true)]
async fn hop_by_hop_headers_are_not_forwarded() {
    let headers = forwarded_request_headers(|request| {
        request
            .header("keep-alive", "timeout=5")
            .header("proxy-authorization", "Basic eA==")
            .header("te", "trailers")
            .header("upgrade", "x-proto")
    })
    .await;
    for name in ["keep-alive", "proxy-authorization", "te", "upgrade"] {
        assert!(!headers.contains_key(name), "{name} ушёл на upstream");
    }
}

#[tokio::test(start_paused = true)]
async fn redirect_is_returned_to_the_client_and_not_followed() {
    // Редирект на другой адрес унёс бы заголовки с токеном Claude Code.
    let (elsewhere, elsewhere_recorder) = recording_upstream().await;
    let target = format!("{elsewhere}/stolen");
    let app = axum::Router::new().fallback(move || {
        let location = target.clone();
        async move {
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header(header::LOCATION, location)
                .body(Body::empty())
                .unwrap()
        }
    });
    let gateway = spawn_gateway(spawn_upstream(app).await).await;

    let response = client()
        .post(gateway.url("/v1/messages"))
        .header("x-api-key", "sk-ant-test")
        .body(MAIN_REQUEST)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(response.headers()[header::LOCATION], format!("{elsewhere}/stolen").as_str());
    assert_eq!(elsewhere_recorder.count(), 0, "шлюз пошёл по редиректу");
}

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Подписчик действует на поток теста: шлюз работает в том же однопоточном рантайме. Глобальный нельзя —
// другие тесты бинарника уже могли его занять.
#[tokio::test(start_paused = true)]
async fn request_headers_never_reach_the_log() {
    let logs = SharedBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (upstream, _) = recording_upstream().await;
    let mut gateway = spawn_gateway(upstream).await;
    let response = client()
        .post(gateway.url("/v1/messages"))
        .header("x-api-key", "sk-ant-secret-key")
        .header(header::AUTHORIZATION, "Bearer sk-ant-oat-secret-token")
        .body(MAIN_REQUEST)
        .send()
        .await
        .unwrap();
    response.bytes().await.unwrap();
    gateway.drain_events().await;

    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("/v1/messages"), "журнал пуст: {output}");
    assert!(!output.contains("secret"), "заголовок попал в журнал: {output}");
}

#[tokio::test(start_paused = true)]
async fn api_key_from_the_query_never_reaches_the_log_on_connection_error() {
    let logs = SharedBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut gateway = spawn_gateway_for(crate::session::Api::Gemini, dead_address().await).await;
    let request = tokio::spawn(
        client()
            .post(gateway.url("/v1beta/models/m:generateContent?key=SECRET"))
            .body(GEMINI_MAIN_REQUEST)
            .send(),
    );
    for attempt in 1..=5 {
        gateway.next_event().await;
        if attempt < 5 {
            advance(Duration::from_secs(31)).await;
        }
    }
    request.await.unwrap().unwrap();

    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("соединение с upstream не удалось"), "журнал пуст: {output}");
    assert!(!output.contains("SECRET"), "ключ из query попал в журнал: {output}");
}

// --- Запрос уходит без изменений ---------------------------------------------

/// Тело, как его формирует агент: нетипичные пробелы, порядок ключей, `cache_control`,
/// не-ASCII и экранированные символы. Любая нормализация JSON его испортит.
const AGENT_REQUEST: &str = concat!(
    "{ \"model\":\"m\",\"tools\" : [{\"name\":\"Read\"}],\n",
    " \"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"\\u0041 привет\",",
    "\"cache_control\":{\"type\":\"ephemeral\"}}]}], \"stream\":true }"
);

#[tokio::test(start_paused = true)]
async fn messages_request_body_reaches_upstream_byte_for_byte() {
    let (upstream, recorder) = recording_upstream().await;
    let gateway = spawn_gateway(upstream).await;
    post_and_read(&gateway, "/v1/messages?beta=true", AGENT_REQUEST).await;
    assert_eq!(recorder.only_request().body, AGENT_REQUEST.as_bytes(), "тело запроса изменено");
}

#[tokio::test(start_paused = true)]
async fn content_encoding_of_a_request_reaches_upstream_with_its_body() {
    let (upstream, recorder) = recording_upstream().await;
    let gateway = spawn_gateway(upstream).await;
    let body = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01];
    client()
        .post(gateway.url("/v1/messages"))
        .header(header::CONTENT_ENCODING, "zstd")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    let request = recorder.only_request();
    assert_eq!(request.headers[header::CONTENT_ENCODING], "zstd");
    assert_eq!(request.body, body);
}

// --- События -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn tool_use_and_end_turn_events_follow_the_stream() {
    let pieces = [
        vec![message_start()],
        tool_block(0, "Read", &json!({"file_path": "/work/a.txt"})),
        tool_block(
            1,
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "a", "new_string": "б"}),
        ),
        tool_block(2, "Write", &json!({"file_path": "/work/b.txt", "content": "новый текст\n"})),
        tool_block(3, "Bash", &json!({"command": "ls", "file_path": "/не/путь"})),
        tool_block_without_deltas(4, "TodoRead"),
        message_end("end_turn"),
    ]
    .concat();
    let stream = text_of(&pieces);
    let upstream = spawn_upstream(respond_once(sse_response(Body::from(stream.clone())))).await;
    let mut gateway = spawn_gateway(upstream).await;

    let body = post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;
    assert_eq!(body, stream.as_bytes());

    // Путь берётся только у Read, Edit и Write; Bash с `file_path` его не получает.
    let events = gateway.drain_events().await;
    assert_snapshot!(dump(&events), @r#"
    tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}
    tool_use Edit path=/work/a.txt input={"file_path":"/work/a.txt","old_string":"a","new_string":"б"}
    tool_use Write path=/work/b.txt input={"file_path":"/work/b.txt","content":"новый текст\n"}
    tool_use Bash path=- input={"command":"ls","file_path":"/не/путь"}
    tool_use TodoRead path=- input={}
    end_turn
    "#);
}

fn rate_limit_reply_with(unified_status: Option<&str>) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header(header::CONTENT_TYPE, "application/json")
        .header("retry-after", "30");
    if let Some(status) = unified_status {
        response = response.header("anthropic-ratelimit-unified-status", status);
    }
    response
        .body(Body::from(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#,
        ))
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn rejected_429_is_passed_through_reported_as_subscription_limit_and_not_retried() {
    let (upstream, recorder) = replying_upstream(|| rate_limit_reply_with(Some("rejected"))).await;
    let mut gateway = spawn_gateway(upstream).await;

    let response = client().post(gateway.url("/v1/messages")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "30");
    response.bytes().await.unwrap();

    let events = gateway.drain_events().await;
    assert_eq!(
        events,
        [GatewayEvent::SubscriptionLimitReached { reset: None }],
        "rejected должен дать ровно одно событие"
    );
    assert_eq!(recorder.count(), 1, "429 повторён");
}

#[tokio::test(start_paused = true)]
async fn status_429_without_rejected_is_request_rate_limiting_and_not_retried_by_the_gateway() {
    // Тело ошибки тоже `rate_limit_error`, но событие решает заголовок: тело 429 не разбирается.
    let (upstream, recorder) = replying_upstream(|| rate_limit_reply_with(None)).await;
    let mut gateway = spawn_gateway(upstream).await;

    post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    assert_eq!(gateway.drain_events().await, [GatewayEvent::RequestRateLimited]);
    assert_eq!(recorder.count(), 1, "429 повторён шлюзом");
}

#[tokio::test(start_paused = true)]
async fn unified_ratelimit_headers_reach_the_client_unchanged() {
    fn reply() -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-ratelimit-unified-status", "allowed")
            .header("anthropic-ratelimit-unified-5h-utilization", "0.42")
            .header("anthropic-ratelimit-unified-7d-reset", "1767225600")
            .body(Body::from("{}"))
            .unwrap()
    }
    let (upstream, _) = replying_upstream(reply).await;
    let gateway = spawn_gateway(upstream).await;

    let response = client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();

    assert_eq!(response.headers()["anthropic-ratelimit-unified-status"], "allowed");
    assert_eq!(response.headers()["anthropic-ratelimit-unified-5h-utilization"], "0.42");
    assert_eq!(response.headers()["anthropic-ratelimit-unified-7d-reset"], "1767225600");
}

#[tokio::test(start_paused = true)]
async fn rate_limit_error_inside_sse_is_request_rate_limiting() {
    // Claude Code 2.1.285 после такой ошибки в потоке сразу повторяет
    // запрос без `stream`; исчерпанную подписку отличает только 429 с `rejected`.
    let stream = sse(
        "error",
        json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Usage limit reached"}}),
    );
    let upstream = spawn_upstream(respond_once(sse_response(Body::from(stream)))).await;
    let mut gateway = spawn_gateway(upstream).await;

    post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    assert_eq!(gateway.drain_events().await, [GatewayEvent::RequestRateLimited]);
}

// --- Ответ без потока --------------------------------------------------------

fn json_reply(body: impl Into<Body>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

/// Ответ без потока, как его отдаёт API на запрос без `stream`.
fn message_json(blocks: &Value, stop_reason: &str) -> String {
    json!({"id": "m", "type": "message", "role": "assistant", "content": blocks, "stop_reason": stop_reason}).to_string()
}

#[tokio::test(start_paused = true)]
async fn non_streaming_message_gives_tool_use_events_and_end_turn() {
    let body = message_json(
        &json!([
            {"type": "text", "text": "смотрю"},
            {"type": "server_tool_use", "id": "s", "name": "web_search", "input": {"query": "q"}},
            {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/work/a.txt"}},
            {"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "ls"}},
            {"type": "tool_use", "id": "t3", "name": "Write", "input": {"file_path": "/work/b.txt", "content": "x"}},
            {"type": "text", "text": "готово"}
        ]),
        "end_turn",
    );
    let upstream = spawn_upstream(respond_once(json_reply(body.clone()))).await;
    let mut gateway = spawn_gateway(upstream).await;

    let received = post_and_read(&gateway, "/v1/messages", r#"{"tools":[{"name":"Read"}]}"#).await;

    assert_eq!(received, body.as_bytes(), "ответ изменён");
    assert_snapshot!(dump(&gateway.drain_events().await), @r#"
    tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}
    tool_use Bash path=- input={"command":"ls"}
    tool_use Write path=/work/b.txt input={"file_path":"/work/b.txt","content":"x"}
    end_turn
    "#);
}

#[tokio::test(start_paused = true)]
async fn non_streaming_message_beyond_the_read_limit_is_passed_without_events() {
    let padding = "x".repeat(5 * 1024 * 1024);
    let body = json!({
        "type": "message", "padding": padding, "stop_reason": "end_turn",
        "content": [{"type": "tool_use", "id": "t", "name": "Read", "input": {"file_path": "/a"}}],
    })
    .to_string();
    let upstream = spawn_upstream(respond_once(json_reply(body.clone()))).await;
    let mut gateway = spawn_gateway(upstream).await;

    let received = post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    assert_eq!(received.len(), body.len(), "клиент должен получить тело целиком");
    assert_none(&gateway.drain_events().await);
}

const PROMPT_TOO_LONG: &str =
    r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;

fn bad_request(body: impl Into<Body>) -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn prompt_too_long_is_passed_through_reported_and_not_retried() {
    let (upstream, recorder) = replying_upstream(|| bad_request(PROMPT_TOO_LONG)).await;
    let mut gateway = spawn_gateway(upstream).await;

    let response = client().post(gateway.url("/v1/messages")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.text().await.unwrap(), PROMPT_TOO_LONG);

    assert_eq!(gateway.drain_events().await, [GatewayEvent::ContextOverflow]);
    assert_eq!(recorder.count(), 1, "ответ 400 повторён");
}

/// Тело ошибки «prompt is too long», раздутое полем `padding` после `error`.
fn prompt_too_long_padded_with(padding: usize) -> String {
    let padding = "x".repeat(padding);
    format!(r#"{{"type":"error","error":{{"message":"prompt is too long: 250000 tokens"}},"padding":"{padding}"}}"#)
}

#[tokio::test(start_paused = true)]
async fn error_body_beyond_the_read_limit_is_not_buffered_for_parsing() {
    // Шлюз читает для разбора не больше 64 КиБ: обрезанный JSON не разбирается,
    // память под чужое тело не растёт.
    let (upstream, _) = replying_upstream(|| bad_request(prompt_too_long_padded_with(100 * 1024))).await;
    let mut gateway = spawn_gateway(upstream).await;

    let body = post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    assert!(body.len() > 100 * 1024, "клиент должен получить тело целиком");
    assert_none(&gateway.drain_events().await);
}

#[tokio::test(start_paused = true)]
async fn client_leaving_before_message_stop_is_reported_as_interrupted() {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_response(body))).await;
    let mut gateway = spawn_gateway(upstream).await;

    tx.send(Bytes::from(message_start().text)).await.unwrap();
    let response = client().post(gateway.url("/v1/messages")).send().await.unwrap();
    let mut chunks = response.bytes_stream();
    chunks.next().await.unwrap().unwrap();
    drop(chunks);

    // Пока upstream молчит, обрыв клиента виден шлюзу только при следующей записи.
    tokio::spawn(keep_writing(tx));
    assert_eq!(gateway.next_event().await, GatewayEvent::Interrupted);
}

// --- Повторы и обрывы --------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn unreachable_upstream_is_tried_five_times_thirty_seconds_apart_then_bad_gateway() {
    let mut gateway = spawn_gateway(dead_address().await).await;
    let started = Instant::now();
    let request = tokio::spawn(client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send());

    // Часы стоят, пока тест их не сдвинет: за 30 с до срока повтора нет, к сроку есть.
    let mut log = String::new();
    for attempt in 1..=5 {
        let event = gateway.next_event().await;
        writeln!(log, "{:>3} с  {event:?}", started.elapsed().as_secs()).unwrap();
        if attempt < 5 {
            advance(Duration::from_millis(29_999)).await;
            assert_none(&gateway.drain_events().await);
            advance(Duration::from_millis(2)).await;
        }
    }
    let response = request.await.unwrap().unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_none(&gateway.drain_events().await);
    assert_snapshot!(log, @"
      0 с  ConnectionError { exhausted: false }
     30 с  ConnectionError { exhausted: false }
     60 с  ConnectionError { exhausted: false }
     90 с  ConnectionError { exhausted: false }
    120 с  ConnectionError { exhausted: true }
    ");
}

/// Что ждёт шлюз от потока, оборванного на `cut`-м байте: события блоков,
/// которые успели прийти целиком. Событие SSE закончено пустой строкой, так что
/// блок засчитывается, только когда весь его `content_block_stop` в префиксе.
fn events_in_prefix(pieces: &[Piece], cut: usize) -> Vec<GatewayEvent> {
    let mut end = 0;
    let mut events = Vec::new();
    for piece in pieces {
        end += piece.text.len();
        if end <= cut {
            events.extend(piece.emits.clone());
        }
    }
    events
}

/// Каждый байт потока при `RUN_SLOW_TESTS`; иначе концы событий и соседние
/// байты, где решается, считать ли событие пришедшим, и середины событий.
fn cuts_to_check(pieces: &[Piece]) -> Vec<usize> {
    let total = text_of(pieces).len();
    if std::env::var_os("RUN_SLOW_TESTS").is_some() {
        return (1..=total).collect();
    }
    let mut cuts = vec![1];
    let mut end = 0;
    for piece in pieces {
        let start = end;
        end += piece.text.len();
        cuts.extend([start + piece.text.len() / 2, end - 1, end, end + 1]);
    }
    cuts.retain(|cut| (1..=total).contains(cut));
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

/// Upstream обрывов: тело каждого ответа приходит из заготовленного канала.
#[derive(Default)]
struct CutUpstream {
    next_body: Mutex<Option<mpsc::Receiver<Result<Bytes, io::Error>>>>,
    requests: AtomicUsize,
}

async fn cut_reply(State(upstream): State<Arc<CutUpstream>>) -> Response {
    upstream.requests.fetch_add(1, Ordering::SeqCst);
    let Some(rx) = upstream.next_body.lock().unwrap().take() else {
        // Тело отдано первому запросу: второй — это повтор, которого быть не должно.
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let chunks = futures_util::stream::unfold(rx, |mut rx| async { rx.recv().await.map(|chunk| (chunk, rx)) });
    sse_response(Body::from_stream(chunks))
}

#[tokio::test(start_paused = true)]
async fn upstream_cut_at_any_byte_gives_the_client_the_exact_prefix_and_matching_events() {
    let pieces = [
        vec![message_start()],
        tool_block(
            0,
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "а", "new_string": "б"}),
        ),
        message_end("end_turn"),
    ]
    .concat();
    let stream = text_of(&pieces).into_bytes();

    // Один upstream на все обрывы: каждому запросу достаётся свой канал.
    let upstream = Arc::new(CutUpstream::default());
    let app = axum::Router::new().fallback(cut_reply).with_state(Arc::clone(&upstream));
    let mut gateway = spawn_gateway(spawn_upstream(app).await).await;
    let client = client();

    // Обрыв до первого байта — это обрыв до начала ответа, он повторяется
    // (см. тест повтора); здесь поток уже начался.
    for (done, cut) in cuts_to_check(&pieces).into_iter().enumerate() {
        let (tx, rx) = mpsc::channel(2);
        *upstream.next_body.lock().unwrap() = Some(rx);
        tx.send(Ok(Bytes::copy_from_slice(&stream[..cut]))).await.unwrap();

        let response = client.post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "обрыв на {cut}: запрос повторён после начала ответа"
        );
        let mut chunks = response.bytes_stream();
        let mut received = Vec::new();
        // Обрыв — только после того, как клиент получил весь префикс: так
        // заголовки и данные гарантированно ушли раньше обрыва.
        while received.len() < cut {
            match chunks.next().await {
                Some(Ok(chunk)) => received.extend_from_slice(&chunk),
                other => panic!("обрыв на {cut}: клиент не получил префикс: {other:?}"),
            }
        }
        tx.send(Err(io::Error::new(io::ErrorKind::ConnectionReset, "обрыв"))).await.unwrap();
        let mut failed = false;
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(chunk) => received.extend_from_slice(&chunk),
                Err(_) => failed = true,
            }
        }

        assert_eq!(received, stream[..cut], "обрыв на {cut}: клиент получил не отданный префикс");
        assert!(failed, "обрыв на {cut}: клиент принял оборванный поток за целый");
        assert_eq!(
            upstream.requests.load(Ordering::SeqCst),
            done + 1,
            "обрыв на {cut}: запрос повторён после начала ответа"
        );
        // Обрыв после последнего события ничего не отнял; до него — это обрыв upstream.
        let mut expected = events_in_prefix(&pieces, cut);
        if cut < stream.len() {
            expected.push(GatewayEvent::UpstreamBroken);
        }
        let events = gateway.drain_events().await;
        assert_eq!(
            dump(&events),
            dump(&expected),
            "обрыв на {cut}: события не согласуются с тем, что пришло"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn garbage_instead_of_sse_passes_through_without_events() {
    let junk: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let response = Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(junk.clone()))
        .unwrap();
    let upstream = spawn_upstream(respond_once(response)).await;
    let mut gateway = spawn_gateway(upstream).await;

    let body = post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    assert_eq!(body, junk);
    assert_none(&gateway.drain_events().await);
}

/// Статус, который несёт событие ошибки ответа.
pub(crate) fn status_of(event: Option<&GatewayEvent>) -> Option<u16> {
    match event? {
        GatewayEvent::Unauthorized { status, .. }
        | GatewayEvent::UnretriedError { status, .. }
        | GatewayEvent::ServerError { status, .. } => Some(*status),
        _ => None,
    }
}

/// Медленные и случайные прогоны выключены, пока не задан `RUN_SLOW_TESTS`.
pub(crate) fn slow_tests_enabled() -> bool {
    std::env::var_os("RUN_SLOW_TESTS").is_some()
}
#[tokio::test(start_paused = true)]
async fn subscription_windows_from_a_messages_response_are_reported_as_limits() {
    fn reply() -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-ratelimit-unified-status", "allowed")
            .header("anthropic-ratelimit-unified-5h-utilization", "0.42")
            .header("anthropic-ratelimit-unified-5h-reset", "1767225600")
            .body(Body::from("{}"))
            .unwrap()
    }
    let (upstream, _) = replying_upstream(reply).await;
    let mut gateway = spawn_gateway(upstream).await;

    post_and_read(&gateway, "/v1/messages", MAIN_REQUEST).await;

    let limits = gateway
        .drain_events()
        .await
        .into_iter()
        .find_map(|event| match event {
            GatewayEvent::Limits(windows) => Some(windows),
            _ => None,
        })
        .expect("событие Limits не пришло");
    assert_eq!(
        limits,
        [nerve::protocol::UsageWindow {
            duration: Some(18_000),
            scope: None,
            used: 0.42,
            resets_at: Some(1_767_225_600),
        }]
    );
}
