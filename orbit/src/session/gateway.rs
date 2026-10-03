//! Шлюз на базовом адресе API агента: пассивный прокси к Messages API, Responses API или Gemini в обёртке
//! Code Assist. Запрос уходит к upstream, а ответ к клиенту без изменений; шлюз лишь наблюдает за копией ответа,
//! распаковывая её, если ответ сжат.
//!
//! Заголовки запросов в журнал не попадают: в них токен агента.

mod activity;
mod classify;
mod decode;
mod gemini;
mod messages;
mod observer;
mod responses;
mod sse;

use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) use activity::Tracker;
use anyhow::Context;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use futures_util::stream;
use nerve::protocol::UsageWindow;
use observer::{Observer, RequestLog};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// API, на котором говорит агент сессии: от него зависит разбор ответов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum Api {
    /// Anthropic Messages (`/v1/messages`)
    Messages,
    /// `OpenAI` Responses (`/responses`, SSE)
    Responses,
    /// Gemini `streamGenerateContent`, в обёртке Code Assist или без неё
    Gemini,
}

/// Вид запроса агента к API: от него зависит, что значит событие.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RequestKind {
    /// Ход агента: только его конец ответа есть `EndTurn`.
    Main,
    /// Запрос подагента. `id` — идентификатор агента (у Messages — заголовок
    /// подагента); `kind` — род подагента (Responses:
    /// `x-openai-subagent` или `subagent_kind`, например `review`).
    Subagent { id: Option<String>, kind: Option<String> },
    /// Название сессии, треда или беседы.
    Title,
    /// Сжатие истории.
    Compaction,
    /// Остальные служебные запросы: проба квоты, классификатор, поиск,
    /// роутер, prewarm.
    Other,
}

/// Событие шлюза вместе с видом запроса, из которого оно получено.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Observation {
    pub request: RequestKind,
    pub event: GatewayEvent,
}

/// Событие, замеченное шлюзом в обмене агента с API.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum GatewayEvent {
    /// Конец хода: `stop_reason: end_turn`.
    EndTurn,
    /// Блок `tool_use` завершён. `path` — путь файла, который вызов читает или пишет, если он распознан.
    ToolUse {
        name: String,
        input: serde_json::Value,
        path: Option<String>,
    },
    /// Клиент оборвал поток до `message_stop`.
    Interrupted,
    /// Сервис принял учётные данные: ответ 2xx на запрос с ключом или токеном.
    /// Запрос без них (проверка связи `HEAD /api/hello` Messages) и
    /// служебные `/api/*` Messages о ключе ничего не говорят и этого
    /// события не дают.
    Accepted,
    /// Сервис отверг учётные данные: 401 или 403 (у Messages — на запрос
    /// `/v1/*`); у Gemini API ещё 400 с `API_KEY_INVALID`. `code` — код
    /// ошибки из тела.
    Unauthorized { status: u16, code: Option<String> },
    /// Исчерпан лимит подписки; агент такой запрос не повторяет. `reset` —
    /// когда лимит сбросится, если сервис это сообщил.
    SubscriptionLimitReached { reset: Option<LimitReset> },
    /// Окна лимитов подписки из заголовков ответа; приходят целым списком и заменяют прежние.
    Limits(Vec<UsageWindow>),
    /// Ограничение частоты, которое агент повторяет сам: событие приходит на
    /// каждую попытку.
    RequestRateLimited,
    /// Переполнен контекст: «prompt is too long» и аналоги других API.
    ContextOverflow,
    /// Ошибка ответа, которую агент по своему профилю не повторяет: Responses —
    /// 429 кроме лимита подписки, Gemini — 400 и 500. Основной ход на ней
    /// обрывается.
    UnretriedError { status: u16, code: Option<String> },
    /// Ответ 5xx, который агент повторяет сам.
    ServerError { status: u16, code: Option<String> },
    /// Upstream оборвал тело ответа, не дойдя до его конца.
    UpstreamBroken,
    /// Upstream недоступен. `exhausted` — это была последняя попытка.
    ConnectionError { exhausted: bool },
}

/// Когда сбросится исчерпанный лимит подписки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LimitReset {
    /// Момент сброса, секунды Unix.
    At(u64),
    /// Через сколько после ответа: так сообщает Gemini (`retryDelay`) и Responses
    /// (`resets_in_seconds`), если момента нет.
    After(Duration),
}

/// Сколько раз пробовать отправить запрос, если соединение упало до начала ответа.
const RETRY_ATTEMPTS: u32 = 5;
/// Пауза между попытками.
const RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(super) struct Config {
    /// Какое API говорит агент.
    pub api: Api,
    /// Адрес API без завершающего `/`.
    pub upstream: String,
    /// Предел тела запроса: оно буферизуется целиком для повторов.
    pub max_request_body: usize,
    /// Занятость агента по идущим запросам; подписка — `activity.subscribe()`.
    pub activity: Tracker,
}

impl Config {
    pub(super) fn new(api: Api, upstream: &str) -> Self {
        Self {
            api,
            upstream: upstream.trim_end_matches('/').to_owned(),
            max_request_body: 64 * 1024 * 1024,
            activity: Tracker::default(),
        }
    }
}

/// Рассылает событие подписчикам. Без подписчиков отправка не удаётся, и это не
/// ошибка: события никому не нужны, пока их никто не слушает.
fn publish(events: &broadcast::Sender<Observation>, request: &RequestKind, event: GatewayEvent) {
    let observation = Observation {
        request: request.clone(),
        event,
    };
    if events.send(observation).is_err() {
        tracing::trace!("событие шлюза никто не слушает");
    }
}

/// Код ошибки из тела ответа: короткое машинное слово вроде
/// `authentication_error`. Свободный текст сообщения в событие не попадает:
/// сервис может повторить в нём часть ключа.
fn error_code(value: &serde_json::Value) -> Option<String> {
    let code = value.as_str()?;
    let word = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.');
    (!code.is_empty() && code.len() <= 64 && code.bytes().all(word)).then(|| code.to_owned())
}

struct Shared {
    config: Config,
    client: reqwest::Client,
    events: broadcast::Sender<Observation>,
}

/// Принимает запросы агента на `listener` и пересылает их на `config.upstream`.
pub(super) async fn serve(listener: TcpListener, config: Config, events: broadcast::Sender<Observation>) -> anyhow::Result<()> {
    // Прокси из окружения (`HTTP_PROXY` и т. п.) не берётся: через него ушёл бы
    // токен агента из заголовков.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("не удалось создать HTTP-клиент шлюза")?;
    let shared = Arc::new(Shared { config, client, events });
    let app = Router::new().fallback(proxy).with_state(shared);
    axum::serve(listener, app).await.context("шлюз остановился с ошибкой")
}

async fn proxy(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let method = parts.method.to_string();
    let path = parts.uri.path().to_owned();
    let Ok(body) = axum::body::to_bytes(body, shared.config.max_request_body).await else {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "тело запроса не прочитано или больше допустимого");
    };

    let request_kind = classify::classify(shared.config.api, &parts.headers, &body);
    // Охранник живёт, пока идёт запрос: ранний возврат или обрыв клиента сбрасывают его.
    let busy = shared.config.activity.begin(&request_kind);

    let upstream_response = match send_with_retries(&shared, &request_kind, &parts, body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%method, %path, %error, elapsed_ms = started.elapsed().as_millis(), "upstream недоступен");
            return error_response(StatusCode::BAD_GATEWAY, "upstream недоступен");
        }
    };

    let status = upstream_response.status();
    if status.is_success() && carries_credentials(&parts) && speaks_for_credentials(shared.config.api, &path) {
        publish(&shared.events, &request_kind, GatewayEvent::Accepted);
    }
    let headers = forwarded_headers(upstream_response.headers(), &[]);
    let observer = Observer::new(
        shared.config.api,
        request_kind,
        shared.events.clone(),
        RequestLog { method, path, started },
        status,
        &headers,
        upstream_response.content_length(),
    )
    .holding(busy);

    let mut response = Response::new(Body::from_stream(observed_body(upstream_response, observer)));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Отдаёт куски тела ответа по мере прихода и показывает те же куски наблюдателю.
fn observed_body(response: reqwest::Response, observer: Observer) -> impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> {
    stream::unfold(
        (Box::pin(response.bytes_stream()), observer),
        |(mut upstream, mut observer)| async move {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    observer.feed(&chunk);
                    Some((Ok(chunk), (upstream, observer)))
                }
                Some(Err(error)) => {
                    observer.upstream_failed();
                    Some((Err(error), (upstream, observer)))
                }
                None => {
                    observer.finish();
                    None
                }
            }
        },
    )
}

/// Отправляет запрос; повторяет, только если ответа ещё нет. После
/// последней попытки возвращает ошибку. Каждая неудача — событие шлюза.
async fn send_with_retries(
    shared: &Shared,
    request_kind: &RequestKind,
    parts: &Parts,
    body: Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let path_and_query = parts.uri.path_and_query().map_or("/", |value| value.as_str());
    let url = format!("{}{path_and_query}", shared.config.upstream);
    let headers = forwarded_headers(&parts.headers, &["host", "content-length"]);

    let mut attempt = 1;
    loop {
        let mut request = shared.client.request(parts.method.clone(), &url).headers(headers.clone());
        if !body.is_empty() {
            request = request.body(body.clone());
        }
        let error = match request.send().await {
            Ok(response) => return Ok(response),
            // Текст ошибки reqwest несёт URL, а в нём бывает `?key=` Gemini API.
            Err(error) => error.without_url(),
        };
        let exhausted = attempt >= RETRY_ATTEMPTS;
        publish(&shared.events, request_kind, GatewayEvent::ConnectionError { exhausted });
        if exhausted {
            return Err(error);
        }
        tracing::warn!(attempt, attempts = RETRY_ATTEMPTS, %error, "соединение с upstream не удалось, повтор");
        tokio::time::sleep(RETRY_DELAY).await;
        attempt += 1;
    }
}

/// Заголовки, в которых агенты передают ключ или токен сервиса.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "x-api-key", "x-goog-api-key"];

/// Запрос несёт учётные данные: заголовком или параметром `key` Gemini API.
fn carries_credentials(parts: &Parts) -> bool {
    CREDENTIAL_HEADERS.iter().any(|name| parts.headers.contains_key(*name))
        || parts
            .uri
            .query()
            .is_some_and(|query| query.split('&').any(|pair| pair.starts_with("key=")))
}

/// Говорит ли ответ на этот путь о годности ключа: служебные запросы агента с тем же ключом отвечают по
/// доступу к функции, а не по ключу. Какие пути служебные, знает разборщик API.
fn speaks_for_credentials(api: Api, path: &str) -> bool {
    match api {
        Api::Messages => messages::speaks_for_credentials(path),
        Api::Responses | Api::Gemini => true,
    }
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Копия заголовков без hop-by-hop (включая названные в `Connection`) и без `skip`.
fn forwarded_headers(headers: &HeaderMap, skip: &[&str]) -> HeaderMap {
    let named_in_connection: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        let name_str = name.as_str();
        let dropped =
            HOP_BY_HOP.contains(&name_str) || skip.contains(&name_str) || named_in_connection.iter().any(|named| named == name_str);
        if !dropped {
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": message },
    });
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
#[path = "../../tests/session/gateway.rs"]
mod tests;
