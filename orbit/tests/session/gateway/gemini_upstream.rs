//! Шлюз для Antigravity CLI: поток `streamGenerateContent` в обёртке Code
//! Assist (`{response, traceId}`) и без неё. Форма событий и ошибок взята из
//! ответов, которые принимает agy.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::session::gateway::{Api, GatewayEvent, LimitReset};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use insta::assert_snapshot;
use serde_json::{Value, json};

use super::common::*;
use super::post_and_read;

const PATH: &str = "/v1internal:streamGenerateContent?alt=sse";

fn data(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// Часть ответа в обёртке Code Assist.
fn wrapped(parts: &Value, finish_reason: Option<&str>) -> String {
    let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
    if let Some(reason) = finish_reason {
        candidate["finishReason"] = json!(reason);
    }
    data(&json!({
        "response": {"candidates": [candidate], "modelVersion": "probe-model",
                     "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 1, "totalTokenCount": 11}},
        "traceId": "trace-1",
    }))
}

fn call(name: &str, args: &Value) -> Value {
    json!({"functionCall": {"name": name, "args": args}})
}

/// Прогоняет поток через шлюз: клиент обязан получить его байт в байт.
async fn events_of_stream(stream: &str) -> Vec<GatewayEvent> {
    let upstream = spawn_upstream(respond_once(sse_response(Body::from(stream.to_owned())))).await;
    let mut gateway = spawn_gateway_for(Api::Gemini, upstream).await;
    let received = post_and_read(&gateway, PATH, GEMINI_MAIN_REQUEST).await;
    assert_eq!(received, stream.as_bytes(), "поток дошёл не байт в байт");
    gateway.drain_events().await
}

#[tokio::test(start_paused = true)]
async fn stop_without_function_calls_is_the_end_of_the_turn() {
    let stream = [
        wrapped(&json!([{"text": "часть "}]), None),
        wrapped(&json!([{"text": "ответа"}]), Some("STOP")),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @"end_turn");
}

#[tokio::test(start_paused = true)]
async fn function_call_before_stop_is_a_tool_use_and_not_the_end_of_the_turn() {
    // `STOP` приходит и в ответе с вызовом инструмента, в другом куске потока.
    let stream = [
        wrapped(
            &json!([{"text": "смотрю"}, call("view_file", &json!({"AbsolutePath": "/work/a.txt"}))]),
            None,
        ),
        wrapped(&json!([]), Some("STOP")),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"tool_use view_file path=/work/a.txt input={"AbsolutePath":"/work/a.txt"}"#);
}

#[tokio::test(start_paused = true)]
async fn parallel_function_calls_give_one_event_each_and_paths_come_from_file_tools_only() {
    let stream = wrapped(
        &json!([
            call("replace_file_content", &json!({"TargetFile": "/work/c.txt", "TargetContent": "a"})),
            call("read_file", &json!({"file_path": "/work/d.txt"})),
            call("write_file", &json!({"file_path": "/work/e.txt", "content": "e"})),
            call("replace", &json!({"file_path": "/work/f.txt", "old_string": "a"})),
            call("run_command", &json!({"CommandLine": "cat", "TargetFile": "/не/путь"})),
        ]),
        Some("STOP"),
    );

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"
    tool_use replace_file_content path=/work/c.txt input={"TargetFile":"/work/c.txt","TargetContent":"a"}
    tool_use read_file path=/work/d.txt input={"file_path":"/work/d.txt"}
    tool_use write_file path=/work/e.txt input={"file_path":"/work/e.txt","content":"e"}
    tool_use replace path=/work/f.txt input={"file_path":"/work/f.txt","old_string":"a"}
    tool_use run_command path=- input={"CommandLine":"cat","TargetFile":"/не/путь"}
    "#);
}

#[tokio::test(start_paused = true)]
async fn function_call_of_an_earlier_response_does_not_hide_the_end_of_a_later_one() {
    let stream = [
        wrapped(&json!([call("run_command", &json!({"CommandLine": "ls"}))]), Some("STOP")),
        wrapped(&json!([{"text": "готово"}]), Some("STOP")),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"
    tool_use run_command path=- input={"CommandLine":"ls"}
    end_turn
    "#);
}

/// Upstream, который на каждый запрос отвечает ошибкой; возвращает события шлюза и число запросов.
async fn events_of_error(status: StatusCode, body: &Value) -> (Vec<GatewayEvent>, usize) {
    let requests = Arc::new(AtomicUsize::new(0));
    let text = body.to_string();
    let app = {
        let requests = Arc::clone(&requests);
        let text = text.clone();
        Router::new().fallback(move || {
            requests.fetch_add(1, Ordering::SeqCst);
            let reply = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(text.clone()))
                .unwrap();
            async move { reply }
        })
    };
    let upstream = spawn_upstream(app).await;
    let mut gateway = spawn_gateway_for(Api::Gemini, upstream).await;
    let received = post_and_read(&gateway, PATH, GEMINI_MAIN_REQUEST).await;
    assert_eq!(received, text.as_bytes(), "ошибка дошла не байт в байт");
    (gateway.drain_events().await, requests.load(Ordering::SeqCst))
}

fn google_error(code: u16, status: &str, message: &str, details: &[Value]) -> Value {
    json!({"error": {"code": code, "message": message, "status": status, "details": details}})
}

fn quota_failure() -> Value {
    json!({"@type": "type.googleapis.com/google.rpc.QuotaFailure",
           "violations": [{"quotaMetric": "generativelanguage.googleapis.com/generate_content_free_tier_requests"}]})
}

fn retry_info(delay: &str) -> Value {
    json!({"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay})
}

#[tokio::test(start_paused = true)]
async fn quota_failure_is_a_subscription_limit_and_is_not_retried() {
    let body = google_error(
        429,
        "RESOURCE_EXHAUSTED",
        "You exceeded your current quota",
        &[quota_failure(), retry_info("40s")],
    );

    let (events, requests) = events_of_error(StatusCode::TOO_MANY_REQUESTS, &body).await;

    assert_eq!(
        events,
        [GatewayEvent::SubscriptionLimitReached {
            reset: Some(LimitReset::After(Duration::from_secs(40)))
        }]
    );
    assert_eq!(requests, 1, "429 повторён шлюзом");
}

#[tokio::test(start_paused = true)]
async fn retry_delay_up_to_thirty_seconds_is_request_rate_limiting() {
    for delay in ["2s", "30s", "2.5s"] {
        let body = google_error(429, "RESOURCE_EXHAUSTED", "Quota exceeded", &[retry_info(delay)]);

        let (events, _) = events_of_error(StatusCode::TOO_MANY_REQUESTS, &body).await;

        assert_eq!(events, [GatewayEvent::RequestRateLimited], "retryDelay {delay}");
    }
}

#[tokio::test(start_paused = true)]
async fn token_count_over_the_maximum_is_context_overflow_and_is_not_retried() {
    let body = google_error(
        400,
        "INVALID_ARGUMENT",
        "The input token count (1100000) exceeds the maximum number of tokens allowed (1048576).",
        &[],
    );

    let (events, requests) = events_of_error(StatusCode::BAD_REQUEST, &body).await;

    assert_eq!(events, [GatewayEvent::ContextOverflow]);
    assert_eq!(requests, 1, "400 повторён шлюзом");
}

#[tokio::test(start_paused = true)]
async fn client_leaving_before_finish_reason_is_an_interruption() {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_response(body))).await;
    let mut gateway = spawn_gateway_for(Api::Gemini, upstream).await;

    tx.send(Bytes::from(wrapped(&json!([{"text": "часть "}]), None))).await.unwrap();
    let response = client().post(gateway.url(PATH)).send().await.unwrap();
    let mut chunks = response.bytes_stream();
    chunks.next().await.unwrap().unwrap();
    drop(chunks);

    // Пока upstream молчит, обрыв клиента виден шлюзу только при следующей записи.
    tokio::spawn(async move {
        while tx.send(Bytes::from_static(b": ping\n\n")).await.is_ok() {
            tokio::task::yield_now().await;
        }
    });
    assert_eq!(gateway.next_event().await, GatewayEvent::Interrupted);
}
