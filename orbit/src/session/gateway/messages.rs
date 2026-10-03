//! Anthropic Messages API: события хода из потока `/v1/messages`, из ответа
//! без потока, из заголовков лимита и из тела ошибки.

use std::collections::HashMap;

use axum::http::{HeaderMap, StatusCode};
use nerve::protocol::UsageWindow;
use serde_json::Value;

use super::sse::SseParser;
use super::{GatewayEvent, LimitReset, error_code};

/// Префикс заголовков лимита подписки.
const UNIFIED: &str = "anthropic-ratelimit-unified-";

/// Говорит ли ответ на этот путь о годности ключа. Claude Code шлёт с тем же ключом служебные запросы `/api/*`
/// (флаги, телеметрия, `bootstrap`): их ответ зависит от доступа к функции, а не от ключа, и 403 там не значит,
/// что ключ отозван. О ключе говорят запросы модели — `/v1/*`.
pub(super) fn speaks_for_credentials(path: &str) -> bool {
    path.starts_with("/v1/")
}

/// Разбор потока `/v1/messages`: события по мере прихода и состояние блоков.
#[derive(Default)]
pub(super) struct StreamWatch {
    parser: SseParser,
    tools: HashMap<u64, ToolBlock>,
    message_stopped: bool,
}

/// Блок `tool_use`, вход которого ещё собирается из `input_json_delta`.
struct ToolBlock {
    name: String,
    initial_input: Value,
    partial_json: String,
}

impl StreamWatch {
    pub(super) fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(GatewayEvent)) {
        let Self {
            parser,
            tools,
            message_stopped,
        } = self;
        parser.feed(chunk, |event, data| {
            if let Some(event) = handle_event(tools, message_stopped, event, data) {
                emit(event);
            }
        });
    }

    /// Сообщение закончилось: пришёл `message_stop`.
    pub(super) fn finished(&self) -> bool {
        self.message_stopped
    }
}

fn handle_event(tools: &mut HashMap<u64, ToolBlock>, message_stopped: &mut bool, event: &str, data: &str) -> Option<GatewayEvent> {
    let data: Value = serde_json::from_str(data).ok()?;
    match event {
        "content_block_start" => {
            let block = &data["content_block"];
            if block["type"] == "tool_use" {
                tools.insert(
                    data["index"].as_u64()?,
                    ToolBlock {
                        name: block["name"].as_str()?.to_owned(),
                        initial_input: block["input"].clone(),
                        partial_json: String::new(),
                    },
                );
            }
            None
        }
        "content_block_delta" => {
            let delta = &data["delta"];
            if delta["type"] == "input_json_delta"
                && let Some(tool) = tools.get_mut(&data["index"].as_u64()?)
            {
                tool.partial_json.push_str(delta["partial_json"].as_str()?);
            }
            None
        }
        "content_block_stop" => {
            let tool = tools.remove(&data["index"].as_u64()?)?;
            Some(tool_use_event(tool))
        }
        "message_delta" => (data["delta"]["stop_reason"] == "end_turn").then_some(GatewayEvent::EndTurn),
        "message_stop" => {
            *message_stopped = true;
            None
        }
        // Событие `error` заканчивает поток: `message_stop` после него не будет.
        "error" => {
            *message_stopped = true;
            error_event(&data["error"])
        }
        _ => None,
    }
}

fn tool_use_event(tool: ToolBlock) -> GatewayEvent {
    let input = if tool.partial_json.is_empty() {
        tool.initial_input
    } else {
        serde_json::from_str(&tool.partial_json).unwrap_or(Value::Null)
    };
    let path = match tool.name.as_str() {
        "Read" | "Edit" | "Write" => input["file_path"].as_str().map(str::to_owned),
        _ => None,
    };
    GatewayEvent::ToolUse {
        name: tool.name,
        input,
        path,
    }
}

/// События по JSON-сообщению без потока: каждый `tool_use`, затем конец хода.
pub(super) fn message_events(body: &[u8]) -> Vec<GatewayEvent> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    if message["type"] == "error" {
        return error_event(&message["error"]).into_iter().collect();
    }
    let blocks = message["content"].as_array().map_or(&[][..], Vec::as_slice);
    let mut events: Vec<GatewayEvent> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .filter_map(|block| {
            Some(tool_use_event(ToolBlock {
                name: block["name"].as_str()?.to_owned(),
                initial_input: block["input"].clone(),
                partial_json: String::new(),
            }))
        })
        .collect();
    if message["stop_reason"] == "end_turn" {
        events.push(GatewayEvent::EndTurn);
    }
    events
}

/// Событие по ответу 429: заголовок `anthropic-ratelimit-unified-status:
/// rejected` — исчерпан лимит подписки, и Claude Code такой запрос не
/// повторяет; иначе это ограничение частоты, которое он повторяет сам.
pub(super) fn limit_event(headers: &HeaderMap) -> GatewayEvent {
    if unified(headers, "status") == Some("rejected") {
        GatewayEvent::SubscriptionLimitReached {
            reset: unified_reset(headers),
        }
    } else {
        GatewayEvent::RequestRateLimited
    }
}

/// Окна подписки из заголовков `anthropic-ratelimit-unified-<окно>-utilization` и `-reset`. Окно названо длиной
/// (`5h`, `7d`), после `_` может идти модель (`7d_opus`). Служебные группы (`overage`, `grace-5h`, `7d_oi`) окнами
/// не считаются; значение, которое не разобралось, пропускает окно целиком. Порядок — по длине, затем по модели.
pub(super) fn usage_windows(headers: &HeaderMap) -> Vec<UsageWindow> {
    let mut windows: Vec<UsageWindow> = headers
        .keys()
        .filter_map(|name| name.as_str().strip_prefix(UNIFIED)?.strip_suffix("-utilization"))
        .filter_map(|window| {
            let (length, scope) = match window.split_once('_') {
                Some((length, scope)) => (length, Some(scope)),
                None => (window, None),
            };
            if scope.is_some_and(|scope| scope.is_empty() || scope == "oi") {
                return None;
            }
            let duration = window_seconds(length)?;
            let used: f32 = unified(headers, &format!("{window}-utilization"))?.trim().parse().ok()?;
            if !used.is_finite() {
                return None;
            }
            Some(UsageWindow {
                duration: Some(duration),
                scope: scope.map(str::to_owned),
                used: used.clamp(0.0, 1.0),
                resets_at: unified(headers, &format!("{window}-reset")).and_then(unix_seconds),
            })
        })
        .collect();
    windows.sort_by(|a, b| (a.duration, &a.scope).cmp(&(b.duration, &b.scope)));
    windows
}

/// Длина окна в секундах из названия вида `5h` или `7d`.
fn window_seconds(length: &str) -> Option<u64> {
    let unit = match length.chars().last()? {
        'h' => 3_600,
        'd' => 86_400,
        _ => return None,
    };
    let count: u64 = length[..length.len() - 1].parse().ok()?;
    (count > 0).then(|| count.checked_mul(unit)).flatten()
}

/// Значение заголовка `anthropic-ratelimit-unified-<suffix>`.
fn unified<'a>(headers: &'a HeaderMap, suffix: &str) -> Option<&'a str> {
    headers.get(format!("{UNIFIED}{suffix}"))?.to_str().ok()
}

/// Когда сбросится лимит: `anthropic-ratelimit-unified-reset` — сброс того
/// окна, что отказало (по нему Claude Code пишет «resets …»); без него —
/// самый поздний сброс среди окон со статусом `rejected`, раньше работа не
/// продолжится.
fn unified_reset(headers: &HeaderMap) -> Option<LimitReset> {
    if let Some(at) = unified(headers, "reset").and_then(unix_seconds) {
        return Some(LimitReset::At(at));
    }
    headers
        .keys()
        .filter_map(|name| name.as_str().strip_prefix(UNIFIED)?.strip_suffix("-reset"))
        .filter(|window| unified(headers, &format!("{window}-status")) == Some("rejected"))
        .filter_map(|window| unified(headers, &format!("{window}-reset")).and_then(unix_seconds))
        .max()
        .map(LimitReset::At)
}

/// Секунды Unix из заголовка: целое число.
fn unix_seconds(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

/// Событие по ответу с ошибкой, кроме 429: его решают заголовки.
pub(super) fn error_body_event(status: StatusCode, body: &[u8]) -> Option<GatewayEvent> {
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let error = &body["error"];
    let code = error_code(&error["type"]);
    let status_code = status.as_u16();
    if matches!(status_code, 401 | 403) {
        return Some(GatewayEvent::Unauthorized { status: status_code, code });
    }
    if let Some(event) = error_event(error) {
        return Some(event);
    }
    status
        .is_server_error()
        .then_some(GatewayEvent::ServerError { status: status_code, code })
}

/// Событие по объекту `error` из тела ответа или из SSE-события `error`.
/// `rate_limit_error` в теле одинаков у лимита подписки и у ограничения
/// частоты, а Claude Code после такой ошибки в потоке сразу повторяет запрос
/// без `stream`: исчерпанный лимит подписки различает только 429 с заголовком.
fn error_event(error: &Value) -> Option<GatewayEvent> {
    if error["message"]
        .as_str()
        .is_some_and(|message| message.contains("prompt is too long"))
    {
        return Some(GatewayEvent::ContextOverflow);
    }
    (error["type"] == "rate_limit_error").then_some(GatewayEvent::RequestRateLimited)
}

#[cfg(test)]
#[path = "../../../tests/session/gateway/messages.rs"]
mod tests;
