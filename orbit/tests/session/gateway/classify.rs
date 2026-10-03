use axum::http::HeaderValue;
use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::session::gateway::tests::slow_tests_enabled;

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(*name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

fn messages(tools: &Value, last_user: &str) -> Vec<u8> {
    json!({"tools": tools, "messages": [{"role": "user", "content": last_user}]})
        .to_string()
        .into_bytes()
}

fn gemini(wrapped: bool, tool_names: &[&str], request_type: &str) -> Vec<u8> {
    let declarations: Vec<Value> = tool_names.iter().map(|name| json!({"name": name})).collect();
    let tools = if declarations.is_empty() {
        json!([])
    } else {
        json!([{"functionDeclarations": declarations}])
    };
    let request = json!({"contents": [], "tools": tools});
    let body = if wrapped {
        json!({"requestType": request_type, "request": request})
    } else {
        request
    };
    body.to_string().into_bytes()
}

fn turn_metadata(kind: &str, source: &str) -> String {
    json!({"request_kind": kind, "thread_source": source}).to_string()
}

fn subagent(id: Option<&str>, kind: Option<&str>) -> RequestKind {
    RequestKind::Subagent {
        id: id.map(str::to_owned),
        kind: kind.map(str::to_owned),
    }
}

fn messages_title(tools: &Value) -> Vec<u8> {
    let schema = json!({"properties": {"title": {"type": "string"}}});
    json!({"tools": tools, "output_config": {"format": {"type": "json_schema", "schema": schema}}, "messages": []})
        .to_string()
        .into_bytes()
}

#[test]
fn messages_requests_are_classified_by_the_conditions_one_by_one() {
    let none = headers(&[]);
    let tools = json!([{"name": "Read"}]);
    let cases: Vec<(&str, HeaderMap, Vec<u8>, RequestKind)> = vec![
        ("основной ход", none.clone(), messages(&tools, "привет"), RequestKind::Main),
        (
            "подагент",
            headers(&[("x-claude-code-agent-id", "a1")]),
            messages(&tools, "привет"),
            subagent(Some("a1"), None),
        ),
        ("без tools", none.clone(), messages(&json!([]), "привет"), RequestKind::Other),
        (
            "поле tools отсутствует",
            none.clone(),
            br#"{"messages":[]}"#.to_vec(),
            RequestKind::Other,
        ),
        (
            "только web_search",
            none.clone(),
            messages(&json!([{"name": "web_search"}]), "привет"),
            RequestKind::Other,
        ),
        (
            "web_search и другой инструмент",
            none.clone(),
            messages(&json!([{"name": "web_search"}, {"name": "Read"}]), "привет"),
            RequestKind::Main,
        ),
        ("заголовок сессии", none.clone(), messages_title(&json!([])), RequestKind::Title),
        (
            "схема title при инструментах — не заголовок",
            none.clone(),
            messages_title(&tools),
            RequestKind::Main,
        ),
        (
            "сжатие",
            none.clone(),
            messages(&tools, "сводка\n\nREMINDER: Do NOT call any tools."),
            RequestKind::Compaction,
        ),
        (
            "сжатие без tools",
            none.clone(),
            messages(&json!([]), "сводка\n\nREMINDER: Do NOT call any tools."),
            RequestKind::Compaction,
        ),
        ("пустое тело", none.clone(), Vec::new(), RequestKind::Main),
        ("не JSON", none, "{не json".as_bytes().to_vec(), RequestKind::Main),
    ];
    for (name, headers, body, expected) in cases {
        assert_eq!(classify(Api::Messages, &headers, &body), expected, "{name}");
    }
}

#[test]
fn compact_marker_in_an_earlier_message_does_not_make_the_request_a_compaction() {
    let body = json!({
        "tools": [{"name": "Read"}],
        "messages": [
            {"role": "user", "content": "REMINDER: Do NOT call any tools"},
            {"role": "user", "content": "дальше"},
        ],
    })
    .to_string();

    assert_eq!(classify(Api::Messages, &HeaderMap::new(), body.as_bytes()), RequestKind::Main);
}

#[test]
fn responses_requests_are_classified_by_the_conditions_one_by_one() {
    let metadata = |kind: &str, source: &str| headers(&[("x-codex-turn-metadata", &turn_metadata(kind, source))]);
    let cases: Vec<(&str, HeaderMap, RequestKind)> = vec![
        ("основной ход", metadata("turn", "user"), RequestKind::Main),
        ("без метаданных", headers(&[]), RequestKind::Main),
        (
            "метаданные не JSON",
            headers(&[("x-codex-turn-metadata", "{не json")]),
            RequestKind::Main,
        ),
        ("сжатие", metadata("compaction", "user"), RequestKind::Compaction),
        ("prewarm", metadata("prewarm", "user"), RequestKind::Other),
        ("thread_source subagent", metadata("turn", "subagent"), subagent(None, None)),
        ("заголовок треда", metadata("turn", "thread_title"), RequestKind::Title),
        (
            "x-openai-subagent",
            headers(&[("x-openai-subagent", "review")]),
            subagent(None, Some("review")),
        ),
        (
            "x-codex-parent-thread-id",
            headers(&[("x-codex-parent-thread-id", "t")]),
            subagent(None, None),
        ),
        (
            "subagent_kind в метаданных",
            headers(&[(
                "x-codex-turn-metadata",
                &json!({"request_kind": "turn", "thread_source": "subagent", "subagent_kind": "collab_spawn"}).to_string(),
            )]),
            subagent(None, Some("collab_spawn")),
        ),
    ];
    for (name, headers, expected) in cases {
        assert_eq!(classify(Api::Responses, &headers, b""), expected, "{name}");
    }
}

#[test]
fn gemini_requests_are_classified_by_the_conditions_one_by_one() {
    let none = HeaderMap::new();
    let cases: Vec<(&str, Vec<u8>, RequestKind)> = vec![
        (
            "основной ход в обёртке",
            gemini(true, &["view_file", "run_command"], "agent"),
            RequestKind::Main,
        ),
        ("основной ход без обёртки", gemini(false, &["read_file"], ""), RequestKind::Main),
        (
            "заголовок беседы checkpoint",
            gemini(true, &["view_file"], "checkpoint"),
            RequestKind::Title,
        ),
        ("без инструментов в обёртке", gemini(true, &[], "agent"), RequestKind::Other),
        ("без инструментов", gemini(false, &[], ""), RequestKind::Other),
        (
            "субагент в обёртке",
            gemini(true, &["view_file", "complete_task"], "agent"),
            subagent(None, None),
        ),
        (
            "субагент без обёртки",
            gemini(false, &["read_file", "complete_task"], ""),
            subagent(None, None),
        ),
        (
            "только поиск Google",
            br#"{"request":{"tools":[{"googleSearch":{}}]}}"#.to_vec(),
            RequestKind::Other,
        ),
        ("пустое тело", Vec::new(), RequestKind::Main),
        ("не JSON", "{не json".as_bytes().to_vec(), RequestKind::Main),
    ];
    for (name, body, expected) in cases {
        assert_eq!(classify(Api::Gemini, &none, &body), expected, "{name}");
    }
}

#[test]
fn garbage_requests_do_not_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(body in prop::collection::vec(any::<u8>(), 0..128), name in "[a-z-]{1,20}", value in "[ -~]{0,40}")| {
        let mut garbage = HeaderMap::new();
        if let (Ok(name), Ok(value)) = (name.parse::<axum::http::HeaderName>(), HeaderValue::from_str(&value)) {
            garbage.insert(name, value);
        }
        garbage.insert("x-codex-turn-metadata", HeaderValue::from_str(&value).unwrap());
        for api in [Api::Messages, Api::Responses, Api::Gemini] {
            classify(api, &garbage, &body);
        }
    });
}
