//! Определяет вид запроса: основной ход или служебный (подагент, заголовок,
//! сжатие, прочее). Конец ответа служебного запроса не конец хода агента, поэтому
//! `EndTurn` рассылается только по основному. Запрос читается копией: ни тело,
//! ни заголовки не меняются.
//!
//! Неразобранный запрос считается основным: потерянный конец хода хуже лишнего.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Api, RequestKind};

/// Хвост сообщения, которым Claude Code просит сжать историю без вызова инструментов.
const COMPACT_MARKER: &str = "REMINDER: Do NOT call any tools";

pub(super) fn classify(api: Api, headers: &HeaderMap, body: &[u8]) -> RequestKind {
    match api {
        Api::Messages => classify_messages(headers, body),
        Api::Responses => classify_responses(headers),
        Api::Gemini => classify_gemini(body),
    }
}

/// Значение заголовка, если оно есть и является текстом.
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

/// Claude Code: у подагента есть `x-claude-code-agent-id`; заголовок сессии
/// идёт без `tools` с JSON-схемой `title` в `output_config.format`; проба
/// квоты и классификатор — без `tools`, поиск — только с `web_search`; сжатие
/// кончается напоминанием не вызывать инструменты и проверяется первым, потому что
/// идёт и с инструментами, и без них.
fn classify_messages(headers: &HeaderMap, body: &[u8]) -> RequestKind {
    if let Some(id) = header_text(headers, "x-claude-code-agent-id") {
        return RequestKind::Subagent { id: Some(id), kind: None };
    }
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return RequestKind::Main;
    };
    let last_message = request["messages"].as_array().and_then(|messages| messages.last());
    if last_message.is_some_and(|message| message["content"].to_string().contains(COMPACT_MARKER)) {
        return RequestKind::Compaction;
    }
    let tools = request["tools"].as_array().map_or(&[][..], Vec::as_slice);
    if tools.iter().all(|tool| tool["name"] == "web_search") {
        let is_title = request["output_config"]["format"]["schema"]["properties"]["title"].is_object();
        return if is_title { RequestKind::Title } else { RequestKind::Other };
    }
    RequestKind::Main
}

/// Codex: подагент и `/review` несут `x-openai-subagent` или
/// `x-codex-parent-thread-id`; остальное описывает `x-codex-turn-metadata`.
/// Тело запроса может быть сжато zstd, поэтому оно не читается.
fn classify_responses(headers: &HeaderMap) -> RequestKind {
    let metadata = headers
        .get("x-codex-turn-metadata")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| serde_json::from_str::<Value>(value).ok());
    let subagent_kind =
        header_text(headers, "x-openai-subagent").or_else(|| metadata.as_ref()?["subagent_kind"].as_str().map(str::to_owned));
    let subagent = RequestKind::Subagent {
        id: None,
        kind: subagent_kind,
    };
    if headers.contains_key("x-openai-subagent") || headers.contains_key("x-codex-parent-thread-id") {
        return subagent;
    }
    let Some(metadata) = metadata else {
        return RequestKind::Main;
    };
    match (
        metadata["request_kind"].as_str().unwrap_or("turn"),
        metadata["thread_source"].as_str().unwrap_or("user"),
    ) {
        (_, "subagent") => subagent,
        (_, "thread_title") => RequestKind::Title,
        ("turn", _) => RequestKind::Main,
        ("compaction", _) => RequestKind::Compaction,
        _ => RequestKind::Other,
    }
}

/// Antigravity: заголовок беседы идёт с `requestType: checkpoint`; сжатие,
/// роутер и прочее — без `functionDeclarations`; у субагента среди
/// инструментов есть `complete_task`.
fn classify_gemini(body: &[u8]) -> RequestKind {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return RequestKind::Main;
    };
    if body["requestType"] == "checkpoint" {
        return RequestKind::Title;
    }
    // Code Assist кладёт запрос в `request`, обычный Gemini API — в корень.
    let request = if body["request"].is_object() { &body["request"] } else { &body };
    let declarations: Vec<&Value> = request["tools"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .flat_map(|tool| tool["functionDeclarations"].as_array().map_or(&[][..], Vec::as_slice))
        .collect();
    if declarations.is_empty() {
        return RequestKind::Other;
    }
    if declarations.iter().any(|declaration| declaration["name"] == "complete_task") {
        return RequestKind::Subagent { id: None, kind: None };
    }
    RequestKind::Main
}

#[cfg(test)]
#[path = "../../../tests/session/gateway/classify.rs"]
mod tests;
