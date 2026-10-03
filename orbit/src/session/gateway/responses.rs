//! `OpenAI` Responses API (Codex CLI): события хода из SSE-потока `/responses`
//! и из тела ошибки. Ответ может прийти сжатым: к этому разбору он доходит
//! уже распакованным.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use nerve::protocol::UsageWindow;
use serde_json::Value;

use super::sse::SseParser;
use super::{GatewayEvent, LimitReset, error_code};

/// Окна лимита подписки в заголовках `x-codex-<окно>-*`.
const WINDOWS: [&str; 2] = ["primary", "secondary"];

/// Разбор потока `/responses`: событие хода по каждому SSE-событию.
#[derive(Default)]
pub(super) struct StreamWatch {
    parser: SseParser,
    /// В текущем ответе уже был вызов инструмента: ход ещё не закончен.
    tool_called: bool,
    /// Пришло одно из `response.completed`, `response.incomplete`, `response.failed`.
    finished: bool,
}

impl StreamWatch {
    pub(super) fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(GatewayEvent)) {
        let Self {
            parser,
            tool_called,
            finished,
        } = self;
        parser.feed(chunk, |_, data| {
            if let Some(event) = handle_event(tool_called, finished, data) {
                emit(event);
            }
        });
    }

    pub(super) fn finished(&self) -> bool {
        self.finished
    }
}

/// Тип события берётся из `type` в данных: имя в строке `event:` повторяет его.
fn handle_event(tool_called: &mut bool, finished: &mut bool, data: &str) -> Option<GatewayEvent> {
    let data: Value = serde_json::from_str(data).ok()?;
    let response = &data["response"];
    match data["type"].as_str()? {
        "response.output_item.done" => {
            let event = tool_use_event(&data["item"])?;
            *tool_called = true;
            Some(event)
        }
        "response.completed" => {
            *finished = true;
            let called = std::mem::take(tool_called);
            (!called && response["end_turn"] != false).then_some(GatewayEvent::EndTurn)
        }
        "response.incomplete" => {
            *finished = true;
            (response["incomplete_details"]["reason"] == "interrupted").then_some(GatewayEvent::Interrupted)
        }
        "response.failed" => {
            *finished = true;
            match response["error"]["code"].as_str()? {
                "context_length_exceeded" => Some(GatewayEvent::ContextOverflow),
                "rate_limit_exceeded" => Some(GatewayEvent::RequestRateLimited),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Вызов инструмента приходит целиком в `output_item.done`: `function_call`
/// с JSON-строкой `arguments` или `custom_tool_call` с сырым текстом `input`.
/// Путь есть только у `view_image`: файлы Codex читает и правит через оболочку и `apply_patch`.
fn tool_use_event(item: &Value) -> Option<GatewayEvent> {
    let input = match item["type"].as_str()? {
        "function_call" => item["arguments"]
            .as_str()
            .and_then(|arguments| serde_json::from_str(arguments).ok())
            .unwrap_or(Value::Null),
        "custom_tool_call" => item["input"].clone(),
        _ => return None,
    };
    let name = item["name"].as_str()?;
    let path = match name {
        "view_image" => input["path"].as_str().map(str::to_owned),
        _ => None,
    };
    Some(GatewayEvent::ToolUse {
        name: name.to_owned(),
        input,
        path,
    })
}

/// Событие по ответу с ошибкой. Любой 429 Codex не повторяет: исчерпанный
/// лимит подписки — `usage_limit_reached` в теле или израсходованное на 100 %
/// окно со сбросом в заголовках (`header_reset`); остальные 429 обрывают ход. 401 и 5xx Codex повторяет
/// сам, событие приходит на каждую попытку.
pub(super) fn error_body_event(status: StatusCode, body: &[u8], header_reset: Option<LimitReset>) -> Option<GatewayEvent> {
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let error = &body["error"];
    let code = error_code(&error["code"]).or_else(|| error_code(&error["type"]));
    let status_code = status.as_u16();
    match status_code {
        401 | 403 => Some(GatewayEvent::Unauthorized { status: status_code, code }),
        429 if error["type"] == "usage_limit_reached" || error["code"] == "usage_limit_reached" || header_reset.is_some() => {
            Some(GatewayEvent::SubscriptionLimitReached {
                reset: body_reset(error).or(header_reset),
            })
        }
        429 => Some(GatewayEvent::UnretriedError { status: status_code, code }),
        500..=599 => Some(GatewayEvent::ServerError { status: status_code, code }),
        _ => None,
    }
}

/// Сброс из тела `usage_limit_reached`: момент `resets_at` (секунды Unix) или
/// `resets_in_seconds`.
fn body_reset(error: &Value) -> Option<LimitReset> {
    if let Some(at) = error["resets_at"].as_u64() {
        return Some(LimitReset::At(at));
    }
    error["resets_in_seconds"]
        .as_u64()
        .map(|seconds| LimitReset::After(Duration::from_secs(seconds)))
}

/// Сброс по заголовкам `x-codex-<окно>-used-percent` и `-reset-at`: самый
/// поздний среди окон, израсходованных на 100 %; раньше работа не продолжится.
pub(super) fn header_reset(headers: &HeaderMap) -> Option<LimitReset> {
    let header = |window: &str, field: &str| headers.get(format!("x-codex-{window}-{field}"))?.to_str().ok();
    WINDOWS
        .iter()
        .filter(|window| {
            header(window, "used-percent")
                .and_then(|value| value.trim().parse::<f64>().ok())
                .is_some_and(|percent| percent >= 100.0)
        })
        .filter_map(|window| header(window, "reset-at")?.trim().parse::<u64>().ok())
        .max()
        .map(LimitReset::At)
}

/// Окна подписки из `x-codex-<окно>-used-percent`, `-window-minutes` и `-reset-at`; без процента окно
/// пропускается, остальные поля необязательны.
pub(super) fn usage_windows(headers: &HeaderMap) -> Vec<UsageWindow> {
    let header = |window: &str, field: &str| headers.get(format!("x-codex-{window}-{field}"))?.to_str().ok().map(str::trim);
    WINDOWS
        .iter()
        .filter_map(|window| {
            let percent: f32 = header(window, "used-percent")?.parse().ok()?;
            if !percent.is_finite() {
                return None;
            }
            Some(UsageWindow {
                duration: header(window, "window-minutes")
                    .and_then(|minutes| minutes.parse::<u64>().ok())
                    .and_then(|minutes| minutes.checked_mul(60)),
                scope: None,
                used: (percent / 100.0).clamp(0.0, 1.0),
                resets_at: header(window, "reset-at").and_then(|at| at.parse().ok()),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "../../../tests/session/gateway/responses.rs"]
mod tests;
