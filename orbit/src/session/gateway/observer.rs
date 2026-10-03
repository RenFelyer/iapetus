use std::time::Instant;

use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use tokio::sync::broadcast;

use super::activity::Guard;
use super::decode::{Decoder, Encoding};
use super::{Api, GatewayEvent, LimitReset, Observation, RequestKind, gemini, messages, publish, responses, speaks_for_credentials};

/// Сколько байт тела ошибки читается для разбора; ошибки API короткие.
const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Сколько байт ответа без потока читается для разбора; сообщение длиннее
/// отдаётся клиенту без событий.
const MESSAGE_BODY_LIMIT: usize = 4 * 1024 * 1024;
/// Окончание пути сводки квоты Code Assist: агент спрашивает её при старте.
const QUOTA_SUMMARY_PATH: &str = ":retrieveUserQuotaSummary";

/// Одна строка журнала на запрос: что за запрос и сколько он шёл.
pub(super) struct RequestLog {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) started: Instant,
}

/// Наблюдает за телом ответа, пока оно идёт клиенту, и рассылает события шлюза.
///
/// Тело не задерживается: наблюдатель получает те же куски, что уходят
/// клиенту. Копия копится только для ошибки и ответа без потока, с пределом.
/// Когда ответ закончился или клиент его бросил, в `Drop` рассылаются итоговые
/// события и пишется строка журнала.
pub(super) struct Observer {
    api: Api,
    /// Вид запроса: конец ответа служебного запроса не `EndTurn`.
    request_kind: RequestKind,
    events: broadcast::Sender<Observation>,
    request: RequestLog,
    status: StatusCode,
    watch: Watch,
    /// Распаковщик копии, если ответ сжат; клиенту уходят исходные куски.
    decoder: Option<Box<Decoder>>,
    outcome: Outcome,
    /// Сколько байт тела ещё не пришло, если известен `content-length`:
    /// hyper не опрашивает поток после последнего байта, конца не будет.
    remaining: Option<u64>,
    /// Время сброса исчерпанного лимита по заголовкам ответа: Responses сообщает
    /// его и в заголовках, и в теле ошибки.
    header_reset: Option<LimitReset>,
    /// Учёт запроса в занятости агента: снимается вместе с наблюдателем.
    busy: Option<Guard>,
}

/// Разбор потока своего API.
enum StreamWatch {
    Messages(messages::StreamWatch),
    Responses(responses::StreamWatch),
    Gemini(gemini::StreamWatch),
}

impl StreamWatch {
    fn new(api: Api) -> Self {
        match api {
            Api::Messages => Self::Messages(messages::StreamWatch::default()),
            Api::Responses => Self::Responses(responses::StreamWatch::default()),
            Api::Gemini => Self::Gemini(gemini::StreamWatch::default()),
        }
    }

    fn feed(&mut self, chunk: &[u8], emit: impl FnMut(GatewayEvent)) {
        match self {
            Self::Messages(watch) => watch.feed(chunk, emit),
            Self::Responses(watch) => watch.feed(chunk, emit),
            Self::Gemini(watch) => watch.feed(chunk, emit),
        }
    }

    /// Ответ дошёл до своего конца: оборвавшийся раньше считается прерванным.
    fn finished(&self) -> bool {
        match self {
            Self::Messages(watch) => watch.finished(),
            Self::Responses(watch) => watch.finished(),
            Self::Gemini(watch) => watch.finished(),
        }
    }
}

enum Watch {
    Nothing,
    Stream(StreamWatch),
    /// Тело ответа с ошибкой. `intact: false` — копию не распаковать: событие
    /// решает один статус.
    ErrorBody {
        body: Vec<u8>,
        intact: bool,
    },
    /// JSON без потока; `truncated` — тело длиннее предела.
    Json {
        kind: JsonKind,
        body: Vec<u8>,
        truncated: bool,
    },
}

/// Какой JSON без потока разбирается.
enum JsonKind {
    /// Сообщение Messages API: агент повторяет без `stream` после двух обрывов.
    Message,
    /// Сводка квоты Code Assist с окнами лимита.
    QuotaSummary,
}

impl Watch {
    fn json(kind: JsonKind) -> Self {
        Self::Json {
            kind,
            body: Vec::new(),
            truncated: false,
        }
    }
}

enum Outcome {
    Streaming,
    Finished,
    UpstreamFailed,
}

impl Observer {
    /// Выбирает, что наблюдать, по статусу и типу содержимого. Ответ 429 от
    /// Messages API сразу даёт событие лимита по заголовкам: тело не нужно.
    /// Остальные ошибки различаются по телу. Ответ без потока разбирается у
    /// Messages и у сводки квоты Gemini.
    pub(super) fn new(
        api: Api,
        request_kind: RequestKind,
        events: broadcast::Sender<Observation>,
        request: RequestLog,
        status: StatusCode,
        headers: &HeaderMap,
        content_length: Option<u64>,
    ) -> Self {
        let windows = match api {
            Api::Messages => messages::usage_windows(headers),
            Api::Responses => responses::usage_windows(headers),
            Api::Gemini => Vec::new(),
        };
        if !windows.is_empty() {
            publish(&events, &request_kind, GatewayEvent::Limits(windows));
        }
        let content_type = headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok());
        let is_sse = content_type.is_some_and(|value| value.starts_with("text/event-stream"));
        let is_json = content_type.is_some_and(|value| value.starts_with("application/json"));
        let watch = if api == Api::Messages && status == StatusCode::TOO_MANY_REQUESTS {
            publish(&events, &request_kind, messages::limit_event(headers));
            Watch::Nothing
        } else if status.is_success() && is_sse {
            Watch::Stream(StreamWatch::new(api))
        } else if api == Api::Messages && status.is_success() && is_json {
            Watch::json(JsonKind::Message)
        } else if api == Api::Gemini && status.is_success() && is_json && request.path.ends_with(QUOTA_SUMMARY_PATH) {
            Watch::json(JsonKind::QuotaSummary)
        } else if status.as_u16() >= 400 {
            Watch::ErrorBody {
                body: Vec::new(),
                intact: true,
            }
        } else {
            Watch::Nothing
        };
        let encoding = Encoding::from_header(headers.get(CONTENT_ENCODING).and_then(|value| value.to_str().ok()));
        let (watch, decoder) = match (watch, encoding) {
            (watch, Encoding::Identity) => (watch, None),
            (watch, Encoding::Compressed(decoder)) => (watch, Some(decoder)),
            (Watch::Nothing, Encoding::Unsupported) => (Watch::Nothing, None),
            (Watch::ErrorBody { body, .. }, Encoding::Unsupported) => (Watch::ErrorBody { body, intact: false }, None),
            (_, Encoding::Unsupported) => {
                tracing::warn!(method = %request.method, path = %request.path, "кодировка ответа не поддержана, событий нет");
                (Watch::Nothing, None)
            }
        };
        let header_reset = match api {
            Api::Responses => responses::header_reset(headers),
            Api::Messages | Api::Gemini => None,
        };
        Self {
            api,
            request_kind,
            events,
            request,
            status,
            watch,
            decoder,
            outcome: if content_length == Some(0) {
                Outcome::Finished
            } else {
                Outcome::Streaming
            },
            remaining: content_length,
            header_reset,
            busy: None,
        }
    }

    /// Привязывает учёт запроса в занятости агента к жизни наблюдателя.
    pub(super) fn holding(mut self, busy: Option<Guard>) -> Self {
        self.busy = busy;
        self
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) {
        if let Some(decoder) = &mut self.decoder {
            match decoder.push(chunk) {
                Ok(plain) => self.observe(&plain),
                Err(error) => {
                    // Сбой разбора копии не касается потока клиенту: он идёт дальше как есть.
                    tracing::warn!(method = %self.request.method, path = %self.request.path, %error, "ответ не распакован, событий из тела больше нет");
                    self.decoder = None;
                    self.watch = match std::mem::replace(&mut self.watch, Watch::Nothing) {
                        Watch::ErrorBody { .. } => Watch::ErrorBody {
                            body: Vec::new(),
                            intact: false,
                        },
                        _ => Watch::Nothing,
                    };
                }
            }
        } else {
            self.observe(chunk);
        }
        if let Some(remaining) = &mut self.remaining {
            *remaining = remaining.saturating_sub(chunk.len() as u64);
            if *remaining == 0 {
                self.finish();
            }
        }
    }

    /// Показывает наблюдению распакованные байты тела.
    fn observe(&mut self, chunk: &[u8]) {
        match &mut self.watch {
            Watch::Nothing | Watch::ErrorBody { intact: false, .. } => {}
            Watch::Stream(stream) => {
                let events = &self.events;
                let request_kind = &self.request_kind;
                stream.feed(chunk, |event| publish_turn_event(events, request_kind, event));
            }
            Watch::ErrorBody { body, intact: true } => {
                let room = ERROR_BODY_LIMIT.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Watch::Json { body, truncated, .. } => {
                let room = MESSAGE_BODY_LIMIT.saturating_sub(body.len());
                *truncated |= chunk.len() > room;
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
        }
    }

    /// Upstream закончил тело.
    pub(super) fn finish(&mut self) {
        self.outcome = Outcome::Finished;
    }

    /// Upstream оборвал тело посреди ответа.
    pub(super) fn upstream_failed(&mut self) {
        self.outcome = Outcome::UpstreamFailed;
    }

    /// Итоговые события ответа, когда он закончился или брошен.
    fn publish_final_events(&self) {
        let finished = matches!(self.outcome, Outcome::Finished);
        let mut broken = matches!(self.outcome, Outcome::UpstreamFailed);
        match &self.watch {
            // Ошибка решается и по неполному телу: статус известен, а без
            // события сессия не узнала бы об отказе.
            Watch::ErrorBody { body, intact } => {
                let body = if *intact { body.as_slice() } else { &[] };
                match error_body_event(self.api, self.status, body, self.header_reset) {
                    Some(GatewayEvent::Unauthorized { .. }) if !speaks_for_credentials(self.api, &self.request.path) => {
                        tracing::info!(method = %self.request.method, path = %self.request.path, status = self.status.as_u16(), "отказ служебного запроса не касается ключа");
                    }
                    Some(event) => publish(&self.events, &self.request_kind, event),
                    None => {}
                }
            }
            Watch::Json { truncated: true, .. } if finished => {
                tracing::warn!(method = %self.request.method, path = %self.request.path, limit = MESSAGE_BODY_LIMIT, "ответ без потока больше предела разбора, событий нет");
            }
            Watch::Json { kind, body, .. } if finished => {
                let events = match kind {
                    JsonKind::Message => messages::message_events(body),
                    JsonKind::QuotaSummary => gemini::quota_summary_event(body).into_iter().collect(),
                };
                for event in events {
                    publish_turn_event(&self.events, &self.request_kind, event);
                }
            }
            Watch::Stream(stream) => {
                if stream.finished() {
                    // Поток дошёл до своего конца: сбой после него ничего не отнял.
                    broken = false;
                } else if matches!(self.outcome, Outcome::Streaming) {
                    publish(&self.events, &self.request_kind, GatewayEvent::Interrupted);
                }
            }
            Watch::Json { .. } | Watch::Nothing => {}
        }
        if broken {
            publish(&self.events, &self.request_kind, GatewayEvent::UpstreamBroken);
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.publish_final_events();
        let method = &self.request.method;
        let path = &self.request.path;
        let status = self.status.as_u16();
        let elapsed_ms = self.request.started.elapsed().as_millis();
        match self.outcome {
            Outcome::Finished => tracing::info!(%method, %path, status, elapsed_ms, "запрос завершён"),
            Outcome::UpstreamFailed => tracing::warn!(%method, %path, status, elapsed_ms, "upstream оборвал ответ"),
            Outcome::Streaming => tracing::info!(%method, %path, status, elapsed_ms, "клиент оборвал ответ"),
        }
        // Занятость снимается после итоговых событий запроса.
        drop(self.busy.take());
    }
}

/// Событие по телу ответа с ошибкой.
fn error_body_event(api: Api, status: StatusCode, body: &[u8], header_reset: Option<LimitReset>) -> Option<GatewayEvent> {
    match api {
        Api::Messages => messages::error_body_event(status, body),
        Api::Responses => responses::error_body_event(status, body, header_reset),
        Api::Gemini => gemini::error_body_event(status, body),
    }
}

/// Рассылает событие ответа; `EndTurn` служебного запроса отбрасывается.
fn publish_turn_event(events: &broadcast::Sender<Observation>, request_kind: &RequestKind, event: GatewayEvent) {
    if event == GatewayEvent::EndTurn && *request_kind != RequestKind::Main {
        return;
    }
    publish(events, request_kind, event);
}
