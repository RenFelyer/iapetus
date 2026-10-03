//! Устаревший SSE: подставной сервер на `127.0.0.1:0`, настоящий клиент runner.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;

use crate::runner::mcp::CALL_TIMEOUT;
use crate::runner::mcp::tests::*;

#[tokio::test]
async fn sse_server_gets_catalog_and_calls_through_endpoint_and_message_events() {
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "sse", &fake);
    check_catalog_and_calls(&mut harness).await;
    harness.mcp.stop().await;

    let seen = fake.seen();
    assert_eq!(seen[0].method, "GET");
    for request in &seen {
        assert_eq!(
            request.header("authorization"),
            format!("Bearer {HEADER_SECRET}"),
            "заголовок не ушёл серверу"
        );
    }
    assert!(
        seen[1..].iter().all(|request| request.target.starts_with("/messages?sid=1")),
        "запросы пошли не на адрес из endpoint"
    );
}

#[tokio::test]
async fn sse_endpoint_on_another_host_is_refused() {
    let other = Fake::start(Behavior::default()).await;
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        endpoint: Some(format!("http://{}/steal", other.addr)),
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "sse", &fake);
    let catalog = harness.start().await;
    assert!(catalog.servers.is_empty(), "сервер с чужим endpoint подключён: {catalog:?}");
    assert!(other.seen().is_empty(), "клиент слал запросы на чужой адрес");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn sse_call_without_an_answer_fails_after_the_timeout() {
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        hang: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "sse", &fake);
    harness.start().await;

    tokio::time::pause();
    let mcp = Arc::clone(&harness.mcp);
    let call = tokio::spawn(async move { mcp.call(tool_call("fake")).await });
    tokio::time::sleep(CALL_TIMEOUT + Duration::from_secs(1)).await;
    let error = call.await.unwrap().unwrap_err();
    assert!(
        error.0.contains("не ответил"),
        "вызов без ответа завершился не по таймауту: {error:?}"
    );
    tokio::time::resume();
    harness.mcp.stop().await;
}

#[tokio::test]
async fn end_of_assignment_closes_the_sse_stream_and_cancels_calls_in_flight() {
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        hang: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "sse", &fake);
    harness.start().await;

    let mcp = Arc::clone(&harness.mcp);
    let call = tokio::spawn(async move { mcp.call(tool_call("fake")).await });
    fake.wait_seen("tools/call").await;
    assert!(!call.is_finished(), "вызов к молчащему инструменту уже закончился");

    within("stop", harness.mcp.stop()).await;
    // Предел короче `CALL_TIMEOUT`: вызов должен прервать конец назначения, а не таймаут.
    let result = timeout(Duration::from_secs(10), call)
        .await
        .expect("вызов в полёте не отменён концом назначения")
        .unwrap();
    assert!(result.is_err(), "вызов в полёте не отменён концом назначения: {result:?}");
    fake.wait_dropped().await;
}
#[tokio::test]
async fn sse_server_asking_for_roots_gets_the_project_dir_by_post() {
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        ask_roots: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "sse", &fake);
    harness.start().await;
    within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();

    let reply = within("ответ на roots/list", async {
        loop {
            if let Some(seen) = fake.seen().into_iter().find(|seen| seen.body.contains(r#""id":"r1""#)) {
                break seen;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reply.target.starts_with("/messages?sid=1"),
        "ответ ушёл не на адрес из endpoint: {}",
        reply.target
    );
    let reply: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(reply["result"]["roots"], json!([{"uri": project_uri(&harness)}]));
    harness.mcp.stop().await;
}
