//! Шлюз для Codex CLI: поток `OpenAI` Responses. Форма событий и ошибок взята
//! из ответов, которые принимает Codex.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

const PATH: &str = "/responses";

fn event(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_owned();
    sse(&kind, data)
}

fn created() -> String {
    event(json!({"type": "response.created", "response": {"id": "resp_00"}}))
}

fn function_call(name: &str, arguments: &Value) -> Value {
    json!({"type": "function_call", "name": name, "arguments": arguments.to_string(), "call_id": "call_00"})
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "id": "msg_00", "content": [{"type": "output_text", "text": text}]})
}

/// Элемент вывода так, как его шлёт Responses API: `added`, затем `done`.
fn output_item(item: &Value) -> String {
    [
        event(json!({"type": "response.output_item.added", "item": item})),
        event(json!({"type": "response.output_text.delta", "delta": "…"})),
        event(json!({"type": "response.output_item.done", "item": item})),
    ]
    .concat()
}

fn completed(extra: &Value) -> String {
    let mut response = json!({"id": "resp_00", "usage": {"input_tokens": 10, "output_tokens": 1, "total_tokens": 11}});
    response.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    event(json!({"type": "response.completed", "response": response}))
}

fn sse_reply(body: impl Into<Body>) -> Response {
    sse_response(body.into())
}

/// Прогоняет поток через шлюз: клиент обязан получить его байт в байт.
async fn events_of_stream(stream: &str) -> Vec<GatewayEvent> {
    let upstream = spawn_upstream(respond_once(sse_reply(stream.to_owned()))).await;
    let mut gateway = spawn_gateway_for(Api::Responses, upstream).await;
    let received = post_and_read(&gateway, PATH, "{}").await;
    assert_eq!(received, stream.as_bytes(), "поток дошёл не байт в байт");
    gateway.drain_events().await
}

#[tokio::test(start_paused = true)]
async fn function_call_is_a_tool_use_and_not_the_end_of_the_turn() {
    let stream = [
        created(),
        output_item(&function_call("exec_command", &json!({"cmd": "echo one; pwd"}))),
        completed(&json!({})),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"tool_use exec_command path=- input={"cmd":"echo one; pwd"}"#);
}

#[tokio::test(start_paused = true)]
async fn view_image_gives_the_path_of_the_file_it_reads() {
    let stream = [
        created(),
        output_item(&function_call("view_image", &json!({"path": "/w/shot.png"}))),
        completed(&json!({})),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"tool_use view_image path=/w/shot.png input={"path":"/w/shot.png"}"#);
}

#[tokio::test(start_paused = true)]
async fn custom_tool_call_gives_the_raw_input() {
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-beta\n+BETA\n*** End Patch";
    let item = json!({"type": "custom_tool_call", "name": "apply_patch", "input": patch, "call_id": "call_01"});
    let stream = [created(), output_item(&item), completed(&json!({}))].concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"tool_use apply_patch path=- input="*** Begin Patch\n*** Update File: a.txt\n@@\n-beta\n+BETA\n*** End Patch""#);
}

#[tokio::test(start_paused = true)]
async fn completed_without_tool_calls_is_the_end_of_the_turn() {
    let stream = [created(), output_item(&message("Готово.")), completed(&json!({}))].concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @"end_turn");
}

#[tokio::test(start_paused = true)]
async fn tool_call_of_an_earlier_response_does_not_hide_the_end_of_a_later_one() {
    // Одно соединение — два ответа подряд: вызов инструмента относится только к своему ответу.
    let stream = [
        created(),
        output_item(&function_call("exec_command", &json!({"cmd": "ls"}))),
        completed(&json!({})),
        created(),
        output_item(&message("Готово.")),
        completed(&json!({})),
    ]
    .concat();

    let events = events_of_stream(&stream).await;

    assert_snapshot!(dump(&events), @r#"
    tool_use exec_command path=- input={"cmd":"ls"}
    end_turn
    "#);
}

#[tokio::test(start_paused = true)]
async fn failed_response_is_classified_by_its_error_code() {
    let failed = |code: &str| {
        [
            created(),
            event(json!({"type": "response.failed", "response": {"id": "resp_00", "error": {"code": code, "message": "m"}}})),
        ]
        .concat()
    };

    assert_eq!(
        events_of_stream(&failed("context_length_exceeded")).await,
        [GatewayEvent::ContextOverflow]
    );
    assert_eq!(
        events_of_stream(&failed("rate_limit_exceeded")).await,
        [GatewayEvent::RequestRateLimited]
    );
    assert_none(&events_of_stream(&failed("server_error")).await);
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
    let mut gateway = spawn_gateway_for(Api::Responses, upstream).await;
    let received = post_and_read(&gateway, PATH, "{}").await;
    assert_eq!(received, text.as_bytes(), "ошибка дошла не байт в байт");
    (gateway.drain_events().await, requests.load(Ordering::SeqCst))
}

#[tokio::test(start_paused = true)]
async fn usage_limit_reached_is_a_subscription_limit_and_is_not_retried() {
    let body = json!({"error": {"type": "usage_limit_reached", "message": "The usage limit has been reached",
                                "plan_type": "plus", "resets_at": 1_790_900_000, "resets_in_seconds": 3600}});

    let (events, requests) = events_of_error(StatusCode::TOO_MANY_REQUESTS, &body).await;

    assert_eq!(
        events,
        [GatewayEvent::SubscriptionLimitReached {
            reset: Some(LimitReset::At(1_790_900_000))
        }]
    );
    assert_eq!(requests, 1, "429 повторён шлюзом");
}

#[tokio::test(start_paused = true)]
async fn other_429_breaks_the_turn() {
    // Codex прочие 429 не повторяет: «exceeded retry limit», ход оборван.
    let body = json!({"error": {"type": "rate_limit_exceeded", "message": "slow down"}});

    let (events, _) = events_of_error(StatusCode::TOO_MANY_REQUESTS, &body).await;

    assert_eq!(
        events,
        [GatewayEvent::UnretriedError {
            status: 429,
            code: Some("rate_limit_exceeded".to_owned())
        }]
    );
}

#[tokio::test(start_paused = true)]
async fn unparsable_429_still_breaks_the_turn() {
    let (events, _) = events_of_error(StatusCode::TOO_MANY_REQUESTS, &json!("slow down")).await;

    assert_eq!(events, [GatewayEvent::UnretriedError { status: 429, code: None }]);
}

#[tokio::test(start_paused = true)]
async fn json_success_body_is_not_read_as_a_messages_message() {
    // Тело в форме сообщения Anthropic с концом хода: для этих API оно ничего не значит.
    let body = json!({"type": "message", "stop_reason": "end_turn", "content": []}).to_string();
    for api in [Api::Responses, Api::Gemini] {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.clone()))
            .unwrap();
        let upstream = spawn_upstream(respond_once(response)).await;
        let mut gateway = spawn_gateway_for(api, upstream).await;

        let received = post_and_read(&gateway, PATH, "{}").await;

        assert_eq!(received, body.as_bytes(), "{api:?}");
        assert_none(&gateway.drain_events().await);
    }
}

#[tokio::test(start_paused = true)]
async fn client_leaving_before_the_response_ends_is_an_interruption() {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_reply(body))).await;
    let mut gateway = spawn_gateway_for(Api::Responses, upstream).await;

    tx.send(Bytes::from(created())).await.unwrap();
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
