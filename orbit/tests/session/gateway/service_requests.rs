//! Служебные запросы (подагент, заголовок, сжатие) идут через шлюз как
//! обычные, но их конец ответа — не конец хода агента. Каждое событие несёт вид
//! запроса, из которого получено.

use crate::session::gateway::{Api, Observation};
use axum::body::Body;
use insta::assert_snapshot;
use serde_json::json;

use super::common::*;

/// Ответ, в котором есть вызов инструмента и затем конец ответа.
fn stream_with_tool_and_end(api: Api) -> String {
    match api {
        Api::Messages => text_of(
            &[
                vec![message_start()],
                tool_block(0, "Read", &json!({"file_path": "/work/a.txt"})),
                message_end("end_turn"),
            ]
            .concat(),
        ),
        Api::Responses => {
            let call = json!({"type": "function_call", "name": "exec_command", "arguments": "{}"});
            let done = json!({"type": "response.output_item.done", "item": call});
            let end = json!({"type": "response.completed", "response": {}});
            // Вызов в одном ответе, конец — в следующем.
            let created = json!({"type": "response.created", "response": {}});
            [
                sse("response.created", created.clone()),
                sse("response.output_item.done", done),
                sse("response.completed", end.clone()),
                sse("response.created", created),
                sse("response.completed", end),
            ]
            .concat()
        }
        Api::Gemini => {
            let call = json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "view_file", "args": {}}}]}, "finishReason": "STOP"}]});
            let end = json!({"candidates": [{"content": {"parts": [{"text": "ок"}]}, "finishReason": "STOP"}]});
            format!("data: {call}\n\ndata: {end}\n\n")
        }
    }
}

async fn run(api: Api, path: &str, headers: &[(&str, &str)], body: &str) -> Vec<Observation> {
    let upstream = spawn_upstream(respond_once(sse_response(Body::from(stream_with_tool_and_end(api))))).await;
    let mut gateway = spawn_gateway_for(api, upstream).await;
    let mut request = client().post(gateway.url(path)).body(body.to_owned());
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap().bytes().await.unwrap();
    gateway.drain_observations().await
}

const CLAUDE_TITLE_REQUEST: &str = r#"{"model":"m","messages":[{"role":"user","content":"привет"}],"output_config":{"format":{"type":"json_schema","schema":{"properties":{"title":{"type":"string"}}}}}}"#;
const CLAUDE_QUOTA_REQUEST: &str = r#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#;
const CLAUDE_COMPACT_REQUEST: &str =
    r#"{"model":"m","tools":[{"name":"Read"}],"messages":[{"role":"user","content":"сводка\n\nREMINDER: Do NOT call any tools."}]}"#;

#[tokio::test(start_paused = true)]
async fn claude_code_request_kinds_are_carried_by_events() {
    let agent = [("x-claude-code-agent-id", "a1")];
    let main = run(Api::Messages, "/v1/messages", &[], MAIN_REQUEST).await;
    let sub = run(Api::Messages, "/v1/messages", &agent, MAIN_REQUEST).await;
    let title = run(Api::Messages, "/v1/messages", &[], CLAUDE_TITLE_REQUEST).await;
    let compaction = run(Api::Messages, "/v1/messages", &[], CLAUDE_COMPACT_REQUEST).await;
    let quota = run(Api::Messages, "/v1/messages", &[], CLAUDE_QUOTA_REQUEST).await;

    assert_snapshot!(dump_observed(&main), @r#"
    [Main] tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}
    [Main] end_turn
    "#);
    assert_snapshot!(dump_observed(&sub), @r#"[Subagent { id: Some("a1"), kind: None }] tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}"#);
    assert_snapshot!(dump_observed(&title), @r#"[Title] tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}"#);
    assert_snapshot!(dump_observed(&compaction), @r#"[Compaction] tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}"#);
    assert_snapshot!(dump_observed(&quota), @r#"[Other] tool_use Read path=/work/a.txt input={"file_path":"/work/a.txt"}"#);
}

#[tokio::test(start_paused = true)]
async fn interrupted_stream_of_a_subagent_carries_its_request_kind() {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(sse_response(body))).await;
    let mut gateway = spawn_gateway_for(Api::Messages, upstream).await;
    let response = client()
        .post(gateway.url("/v1/messages"))
        .header("x-claude-code-agent-id", "a1")
        .body(MAIN_REQUEST)
        .send()
        .await
        .unwrap();
    tx.send(message_start().text.into()).await.unwrap();
    settle().await;
    drop(response);
    drop(tx);

    let seen = gateway.drain_observations().await;
    assert_snapshot!(dump_observed(&seen), @r#"[Subagent { id: Some("a1"), kind: None }] Interrupted"#);
}
