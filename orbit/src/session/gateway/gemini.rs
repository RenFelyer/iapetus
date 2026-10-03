//! Gemini (Antigravity CLI): события хода из SSE-потока `streamGenerateContent`,
//! из сводки квоты и из тела ошибки. Code Assist заворачивает ответ в
//! `{response, traceId}`, обычный Gemini API отдаёт `{candidates}` без обёртки;
//! разбираются оба.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::sse::SseParser;
use super::{GatewayEvent, LimitReset, error_code};

/// Дольше этого агент не ждёт `retryDelay` из 429 и останавливается.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Разбор потока `streamGenerateContent`.
#[derive(Default)]
pub(super) struct StreamWatch {
    parser: SseParser,
    /// В текущем ответе уже был `functionCall`: `finishReason: STOP` приходит и после него.
    tool_called: bool,
    /// Пришёл `finishReason`.
    finished: bool,
}

impl StreamWatch {
    pub(super) fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(GatewayEvent)) {
        let Self {
            parser,
            tool_called,
            finished,
        } = self;
        parser.feed(chunk, |_, data| handle_data(tool_called, finished, data, &mut emit));
    }

    pub(super) fn finished(&self) -> bool {
        self.finished
    }
}

fn handle_data(tool_called: &mut bool, finished: &mut bool, data: &str, emit: &mut impl FnMut(GatewayEvent)) {
    let Ok(data) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let response = if data["response"].is_object() { &data["response"] } else { &data };
    let candidate = &response["candidates"][0];
    for part in candidate["content"]["parts"].as_array().map_or(&[][..], Vec::as_slice) {
        if let Some(event) = tool_use_event(&part["functionCall"]) {
            *tool_called = true;
            emit(event);
        }
    }
    if let Some(reason) = candidate["finishReason"].as_str() {
        *finished = true;
        let called = std::mem::take(tool_called);
        if reason == "STOP" && !called {
            emit(GatewayEvent::EndTurn);
        }
    }
}

/// `functionCall` приходит целиком в одной части. Путь берётся у инструментов
/// чтения и записи файлов Antigravity CLI и Gemini CLI.
fn tool_use_event(call: &Value) -> Option<GatewayEvent> {
    let name = call["name"].as_str()?;
    let input = call["args"].clone();
    let path = match name {
        "view_file" => input["AbsolutePath"].as_str(),
        "write_to_file" | "replace_file_content" => input["TargetFile"].as_str(),
        "read_file" | "write_file" | "replace" => input["file_path"].as_str(),
        _ => None,
    };
    Some(GatewayEvent::ToolUse {
        name: name.to_owned(),
        path: path.map(str::to_owned),
        input,
    })
}

/// Событие по ответу с ошибкой.
/// 401 и 403 — отказ в учётных данных, как и 400 с `API_KEY_INVALID`: так
/// Gemini API отвечает на неверный ключ. 429 с `QuotaFailure` или с
/// `retryDelay` длиннее 30 с agy не повторяет — это лимит подписки; прочие 429
/// повторяет сам. 400 о числе токенов — переполнение контекста; прочие 400 и
/// 500 agy не повторяет; остальные 5xx повторяет.
pub(super) fn error_body_event(status: StatusCode, body: &[u8]) -> Option<GatewayEvent> {
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let error = &body["error"];
    let details = error["details"].as_array().map_or(&[][..], Vec::as_slice);
    let detail_of = |kind: &str| {
        details
            .iter()
            .find(|detail| detail["@type"].as_str().is_some_and(|name| name.ends_with(kind)))
    };
    let reason = detail_of("google.rpc.ErrorInfo").map(|detail| &detail["reason"]);
    let code = reason.and_then(error_code).or_else(|| error_code(&error["status"]));
    let message = error["message"].as_str().unwrap_or_default();
    let status_code = status.as_u16();
    let key_invalid = reason.is_some_and(|reason| reason == "API_KEY_INVALID") || message.contains("API key not valid");
    match status_code {
        401 | 403 => Some(GatewayEvent::Unauthorized { status: status_code, code }),
        400 if key_invalid => Some(GatewayEvent::Unauthorized { status: status_code, code }),
        400 if message.contains("exceeds the maximum number of tokens") => Some(GatewayEvent::ContextOverflow),
        400 | 500 => Some(GatewayEvent::UnretriedError { status: status_code, code }),
        429 => {
            let quota_failure = detail_of("google.rpc.QuotaFailure").is_some();
            let delay = detail_of("google.rpc.RetryInfo").and_then(|detail| retry_delay(&detail["retryDelay"]));
            Some(if quota_failure || delay.is_some_and(|delay| delay > MAX_RETRY_DELAY) {
                GatewayEvent::SubscriptionLimitReached {
                    reset: delay.map(LimitReset::After),
                }
            } else {
                GatewayEvent::RequestRateLimited
            })
        }
        501..=599 => Some(GatewayEvent::ServerError { status: status_code, code }),
        _ => None,
    }
}

/// `retryDelay` в форме `google.protobuf.Duration`: секунды с суффиксом `s`,
/// например `"40s"` или `"2.5s"`.
fn retry_delay(value: &Value) -> Option<Duration> {
    let seconds: f64 = value.as_str()?.strip_suffix('s')?.parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

/// Лимит подписки по сводке квоты (`retrieveUserQuotaSummary`): окно
/// `buckets[]` со временем сброса `resetTime` израсходовано, если
/// `remainingFraction` не больше нуля или его нет — JSON protobuf опускает
/// нулевые поля. Окно с `disabled: true` не считается. Сброс — самый поздний
/// среди израсходованных окон: раньше работа не продолжится.
pub(super) fn quota_summary_event(body: &[u8]) -> Option<GatewayEvent> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let summary = if body["response"].is_object() { &body["response"] } else { &body };
    let exhausted: Vec<&str> = summary["buckets"]
        .as_array()?
        .iter()
        .filter(|bucket| bucket["disabled"] != true)
        .filter(|bucket| bucket["remainingFraction"].as_f64().unwrap_or(0.0) <= 0.0)
        .filter_map(|bucket| bucket["resetTime"].as_str())
        .collect();
    if exhausted.is_empty() {
        return None;
    }
    let reset = exhausted
        .iter()
        .filter_map(|time| OffsetDateTime::parse(time, &Rfc3339).ok())
        .filter_map(|time| u64::try_from(time.unix_timestamp()).ok())
        .max()
        .map(LimitReset::At);
    Some(GatewayEvent::SubscriptionLimitReached { reset })
}

#[cfg(test)]
#[path = "../../../tests/session/gateway/gemini.rs"]
mod tests;
