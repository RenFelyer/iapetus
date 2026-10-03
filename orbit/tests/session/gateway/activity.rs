//! Занятость агента: подагенты и сжатие истории видны, пока идёт их запрос, и снимаются
//! при любом конце запроса.

use std::io;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::response::Response;
use futures_util::stream;
use nerve::protocol::Activity;
use tokio::sync::mpsc::{self, Sender};

use super::common::*;

const SUBAGENT: [(&str, &str); 1] = [("x-claude-code-agent-id", "a1")];
const OTHER_SUBAGENT: [(&str, &str); 1] = [("x-claude-code-agent-id", "a2")];

fn busy(subagents: u32, compactions: u32) -> Activity {
    Activity { subagents, compactions }
}

/// Отправляет запрос и возвращает ответ, когда его заголовки дошли: тело ещё идёт.
async fn start(gateway: &Gateway, headers: &[(&str, &str)], body: &str) -> reqwest::Response {
    let mut request = client().post(gateway.url("/v1/messages")).body(body.to_owned());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

/// Upstream, чей ответ идёт, пока жив отправитель.
async fn slow_upstream() -> (String, Sender<Bytes>) {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_response(body))).await;
    (upstream, tx)
}

/// Upstream, который отвечает на первый запрос первым ответом, на второй вторым.
async fn upstream_in_order(first: Response, second: Response) -> String {
    let queue = Arc::new(Mutex::new(vec![second, first]));
    spawn_upstream(Router::new().fallback(move || async move { queue.lock().unwrap().pop().unwrap() })).await
}

#[tokio::test(start_paused = true)]
async fn subagent_request_is_busy_until_its_response_ends() {
    let (upstream, tx) = slow_upstream().await;
    let gateway = spawn_gateway(upstream).await;
    assert_eq!(gateway.activity().await, busy(0, 0));

    tx.send(Bytes::from(message_start().text)).await.unwrap();
    let response = start(&gateway, &SUBAGENT, MAIN_REQUEST).await;
    assert_eq!(gateway.activity().await, busy(1, 0), "подагент не виден, пока идёт его ответ");

    tx.send(Bytes::from(text_of(&message_end("end_turn")))).await.unwrap();
    drop(tx);
    response.bytes().await.unwrap();
    assert_eq!(gateway.activity().await, busy(0, 0), "занятость осталась после конца ответа");
}

#[tokio::test(start_paused = true)]
async fn parallel_subagents_are_counted_and_released_one_by_one() {
    let (first_tx, first_body) = channel_body();
    let (second_tx, second_body) = channel_body();
    let upstream = upstream_in_order(sse_response(first_body), sse_response(second_body)).await;
    let gateway = spawn_gateway(upstream).await;

    first_tx.send(Bytes::from(message_start().text)).await.unwrap();
    let first = start(&gateway, &SUBAGENT, MAIN_REQUEST).await;
    second_tx.send(Bytes::from(message_start().text)).await.unwrap();
    let second = start(&gateway, &OTHER_SUBAGENT, MAIN_REQUEST).await;
    assert_eq!(gateway.activity().await, busy(2, 0), "параллельные подагенты не сосчитаны");

    drop(first_tx);
    first.bytes().await.unwrap();
    assert_eq!(gateway.activity().await, busy(1, 0), "конец одного подагента снял занятость обоих");

    drop(second_tx);
    second.bytes().await.unwrap();
    assert_eq!(gateway.activity().await, busy(0, 0));
}

#[tokio::test(start_paused = true)]
async fn busy_clears_when_the_client_drops_the_subagent_stream() {
    let (upstream, tx) = slow_upstream().await;
    let gateway = spawn_gateway(upstream).await;
    tx.send(Bytes::from(message_start().text)).await.unwrap();
    let response = start(&gateway, &SUBAGENT, MAIN_REQUEST).await;
    assert_eq!(gateway.activity().await, busy(1, 0));

    drop(response);

    assert_eq!(gateway.activity().await, busy(0, 0), "занятость залипла после обрыва клиентом");
}

#[tokio::test(start_paused = true)]
async fn busy_clears_when_upstream_breaks_the_subagent_stream() {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(2);
    let chunks = stream::unfold(rx, |mut rx| async { rx.recv().await.map(|chunk| (chunk, rx)) });
    let upstream = spawn_upstream(respond_once(sse_response(Body::from_stream(chunks)))).await;
    let gateway = spawn_gateway(upstream).await;
    tx.send(Ok(Bytes::from(message_start().text))).await.unwrap();
    let response = start(&gateway, &SUBAGENT, MAIN_REQUEST).await;
    assert_eq!(gateway.activity().await, busy(1, 0));

    tx.send(Err(io::Error::new(io::ErrorKind::ConnectionReset, "обрыв"))).await.unwrap();

    assert!(response.bytes().await.is_err(), "оборванный поток дочитался без ошибки");
    assert_eq!(gateway.activity().await, busy(0, 0), "занятость залипла после обрыва upstream");
}
