//! Streamable HTTP: подставной сервер на `127.0.0.1:0`, настоящий клиент runner.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::timeout;

use crate::runner::mcp::tests::*;
use crate::runner::mcp::{CALL_TIMEOUT, McpCall};

#[tokio::test]
async fn streamable_http_server_gets_catalog_calls_headers_and_the_session_on_every_request() {
    let fake = Fake::start(Behavior {
        mode: Mode::Session,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    check_catalog_and_calls(&mut harness).await;
    harness.mcp.stop().await;

    let seen = fake.seen();
    let posts: Vec<&Seen> = seen.iter().filter(|seen| seen.method == "POST").collect();
    assert_eq!(posts[0].rpc(), "initialize");
    assert_eq!(posts[0].header("mcp-session-id"), "", "initialize уже с сессией");
    for request in &seen[1..] {
        assert_eq!(
            request.header("mcp-session-id"),
            SESSION,
            "запрос {} {} ушёл без Mcp-Session-Id",
            request.method,
            request.rpc()
        );
    }
    for post in &posts[1..] {
        assert_eq!(
            post.header("mcp-protocol-version"),
            "2025-06-18",
            "запрос {} без версии протокола",
            post.rpc()
        );
    }
    for request in &seen {
        assert_eq!(
            request.header("authorization"),
            format!("Bearer {HEADER_SECRET}"),
            "заголовок не ушёл серверу"
        );
        assert!(
            request.target.ends_with(&format!("key={URL_SECRET}")),
            "адрес не подставлен: {}",
            request.target
        );
    }
    let last = seen.last().unwrap();
    assert_eq!(last.method, "DELETE", "сессия не удалена при остановке");
    let call = posts.iter().find(|post| post.rpc() == "tools/call").unwrap();
    assert_eq!(call.header("mcp-session-id"), SESSION);
}

#[tokio::test]
async fn streamable_http_answer_as_event_stream_is_parsed() {
    let fake = Fake::start(Behavior {
        mode: Mode::Events,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    check_catalog_and_calls(&mut harness).await;
    harness.mcp.stop().await;
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let trap = Fake::start(Behavior::default()).await;
    let fake = Fake::start(Behavior {
        redirect: Some(trap.addr),
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    let catalog = harness.start().await;
    assert!(catalog.servers.is_empty(), "сервер с перенаправлением подключён: {catalog:?}");
    assert_eq!(fake.seen().len(), 1);
    assert!(trap.seen().is_empty(), "клиент пошёл за перенаправлением: {:?}", trap.seen().len());
    harness.mcp.stop().await;
}

#[tokio::test]
async fn http_call_without_an_answer_fails_after_the_timeout_and_the_server_stays_usable() {
    let fake = Fake::start(Behavior {
        hang: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    harness.start().await;

    // Время замораживается после запуска: ответы настоящего сервера не должны гоняться с таймером.
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

    let read = McpCall::ReadResource {
        server: "fake".to_owned(),
        uri: "file:///notes.txt".to_owned(),
    };
    let result = within("resources/read", harness.mcp.call(read)).await;
    assert!(
        result.is_ok_and(|text| text.contains("note body")),
        "сервер не отвечает после таймаута вызова"
    );
    harness.mcp.stop().await;
}

#[tokio::test]
async fn end_of_assignment_closes_http_connections_and_cancels_calls_in_flight() {
    let fake = Fake::start(Behavior {
        hang: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
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
    let after = harness.mcp.call(tool_call("fake")).await.unwrap_err();
    assert!(after.0.contains("не запущен"), "вызов после конца назначения: {after:?}");
}
#[tokio::test]
async fn unauthorized_server_is_reported_with_its_name_and_challenge_without_the_token() {
    let (log, _guard) = capture_log();
    let fake = Fake::start(Behavior {
        deny: Deny::Everything,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    let catalog = harness.start().await;
    assert!(catalog.servers.is_empty(), "сервер без авторизации попал в сводку: {catalog:?}");

    let text = log.wait_for("требует авторизации").await;
    assert!(
        text.contains("сервер fake требует авторизации") && text.contains("HTTP 401"),
        "журнал не называет сервер и статус: {text}"
    );
    assert!(
        text.contains("`headers` через `${VAR}`"),
        "журнал не подсказывает, как задать токен: {text}"
    );
    assert!(
        text.contains("WWW-Authenticate: Bearer error=") && text.contains("Bearer ***"),
        "журнал без WWW-Authenticate или с немаскированным токеном: {text}"
    );
    assert!(!text.contains(HEADER_SECRET), "токен попал в журнал: {text}");

    let error = harness.mcp.call(tool_call("fake")).await.unwrap_err();
    assert!(
        error.0.contains("сервер fake требует авторизации") && error.0.contains("WWW-Authenticate"),
        "агент не узнал причину: {error:?}"
    );
    assert!(!error.0.contains(HEADER_SECRET), "токен попал в ответ агенту: {error:?}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn call_rejected_with_403_names_the_server_and_the_server_stays() {
    let fake = Fake::start(Behavior {
        deny: Deny::Calls,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    assert_eq!(names(&harness.start().await), ["fake"]);

    let error = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap_err();
    assert!(
        error.0.contains("сервер fake требует авторизации") && error.0.contains("HTTP 403"),
        "ответ на 403 не объясняет причину: {error:?}"
    );
    assert!(!error.0.contains(HEADER_SECRET), "токен попал в ответ агенту: {error:?}");
    let read = McpCall::ReadResource {
        server: "fake".to_owned(),
        uri: "file:///notes.txt".to_owned(),
    };
    assert!(
        within("resources/read", harness.mcp.call(read)).await.is_ok(),
        "403 на вызове сломал сервер"
    );
    harness.mcp.stop().await;
}

#[tokio::test]
async fn expired_session_starts_a_new_one_and_repeats_the_request_once() {
    let fake = Fake::start(Behavior {
        mode: Mode::Session,
        expire_sessions: 1,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    harness.start().await;

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(result.contains("session=sess-2"), "повтор ушёл не с новой сессией: {result}");
    // Вызов после обновления сессии идёт сразу в новую.
    let again = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(again.contains("session=sess-2"), "следующий вызов ушёл не с новой сессией: {again}");
    harness.mcp.stop().await;

    assert_eq!(
        fake.count("initialize"),
        2,
        "новый initialize не отправлен или отправлен не один раз"
    );
    assert_eq!(
        fake.count("notifications/initialized"),
        2,
        "initialized не отправлен для новой сессии"
    );
    assert_eq!(fake.count("tools/call"), 3, "вызов не повторён ровно один раз");
}

#[tokio::test]
async fn session_expired_again_right_after_renewal_is_an_error_without_more_retries() {
    let fake = Fake::start(Behavior {
        mode: Mode::Session,
        expire_sessions: 2,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    harness.start().await;

    let error = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap_err();
    assert!(error.0.contains("истекла"), "ошибка не говорит об истёкшей сессии: {error:?}");
    harness.mcp.stop().await;
    assert_eq!(fake.count("tools/call"), 2, "повторов больше одного");
    assert_eq!(fake.count("initialize"), 2, "initialize больше одного раза");
}

#[tokio::test]
async fn http_server_asking_for_roots_in_the_response_stream_gets_the_project_dir() {
    let fake = Fake::start(Behavior {
        mode: Mode::Events,
        ask_roots: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    harness.start().await;
    within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();

    let seen = fake.seen();
    let initialize = seen.iter().find(|seen| seen.rpc() == "initialize").unwrap();
    assert!(
        initialize.body.contains(r#""capabilities":{"roots":{}}"#),
        "initialize не объявил roots без listChanged: {}",
        initialize.body
    );
    let reply = seen
        .iter()
        .find(|seen| seen.body.contains(r#""id":"r1""#))
        .expect("ответ на roots/list не пришёл");
    let reply: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(reply["result"]["roots"], json!([{"uri": project_uri(&harness)}]));
    harness.mcp.stop().await;
}
