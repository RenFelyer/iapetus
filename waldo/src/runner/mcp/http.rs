//! Клиент MCP по streamable HTTP: каждый запрос — POST JSON-RPC на `url`; ответ приходит телом `application/json`
//! или потоком `text/event-stream`, в котором ищется ответ на этот запрос.
//!
//! Сессия, выданная в ответ на `initialize` (`Mcp-Session-Id`), и согласованная версия протокола идут во всех
//! следующих запросах; при закрытии сессия удаляется `DELETE`. Ответ 404 на запрос с сессией — сессия истекла: клиент
//! заново запускает `headersHelper`, шлёт `initialize` и повторяет запрос один раз. Перенаправления не выполняются: заголовки с
//! секретами не должны уйти на другой хост.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use reqwest::{Response as HttpResponse, StatusCode, Url, redirect};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::timeout;

use super::client::{EXITED, MAX_MESSAGE, PROTOCOL_VERSION, Response, answer, initialize_params, server_reply, wait_closed};
use super::events::Events;
use super::headers::Headers;

const SESSION_ID: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько ждать `DELETE` сессии при закрытии.
const DELETE_TIMEOUT: Duration = Duration::from_secs(5);

/// Сколько символов `WWW-Authenticate` идёт в текст ошибки.
const MAX_CHALLENGE: usize = 500;

/// Клиент без перенаправлений: заголовки с секретами не уходят на чужой хост.
pub(super) fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(net)
}

/// Адрес сервера: только `http` и `https`.
pub(super) fn parse_url(url: &str) -> Result<Url, String> {
    let url = Url::parse(url).map_err(|e| format!("адрес не разобран: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("схема {} не поддерживается", url.scheme()));
    }
    Ok(url)
}

/// Заголовки записи; ошибка называет заголовок, но не значение.
pub(super) fn header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let parsed = HeaderName::from_bytes(name.as_bytes()).ok().zip(HeaderValue::from_str(value).ok());
        let Some((header, value)) = parsed else {
            return Err(format!("заголовок {name} не принят"));
        };
        map.insert(header, value);
    }
    Ok(map)
}

/// Текст ошибки запроса без адреса (в нём могут быть секреты), но с причиной.
pub(super) fn net(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Тело ответа не больше `MAX_MESSAGE`.
async fn read_body(mut response: HttpResponse) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(net)? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_MESSAGE {
            return Err("ответ больше предела".to_owned());
        }
    }
    Ok(body)
}

/// Ошибка по статусу ответа. `401` и `403` называют сервер и подсказывают, что делать; `WWW-Authenticate` сервера
/// идёт в текст как есть (маскируется там, где ошибка выводится).
pub(super) fn check_status(name: &str, response: &HttpResponse) -> Result<(), String> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    if !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(format!("HTTP {status}"));
    }
    let challenge = response
        .headers()
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let mut text = format!("сервер {name} требует авторизации (HTTP {status}): задайте токен в `headers` через `${{VAR}}`");
    if !challenge.is_empty() {
        let challenge: String = challenge.chars().take(MAX_CHALLENGE).collect();
        text.push_str("; WWW-Authenticate: ");
        text.push_str(&challenge);
    }
    Err(text)
}

/// Ответ на POST или признак того, что сервер забыл сессию.
enum Posted {
    Reply(HttpResponse),
    Expired,
}

/// Ответ на запрос или признак того, что сервер забыл сессию.
enum Asked {
    Answer(Response),
    Expired,
}

#[derive(Debug)]
pub(super) struct Http {
    name: String,
    client: reqwest::Client,
    url: Url,
    source: Headers,
    headers: Mutex<HeaderMap>,
    learned: Mutex<Vec<Vec<u8>>>,
    root: String,
    next_id: AtomicU64,
    session: Mutex<Option<HeaderValue>>,
    version: Mutex<HeaderValue>,
    /// Сколько раз сессия начиналась заново: запрос, увидевший 404 от старой сессии, не начинает её ещё раз.
    generation: AtomicU64,
    renewing: tokio::sync::Mutex<()>,
    closed: watch::Sender<bool>,
}

impl Http {
    /// Клиент сервера `name`; запускает `headersHelper`, если он есть, сеть больше не трогает.
    pub(super) async fn new(name: &str, url: &str, source: Headers, root: String) -> Result<Self, String> {
        let resolved = source.resolve().await?;
        Ok(Self {
            name: name.to_owned(),
            client: build_client()?,
            url: parse_url(url)?,
            source,
            headers: Mutex::new(resolved.headers),
            learned: Mutex::new(resolved.learned),
            root,
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
            version: Mutex::new(HeaderValue::from_static(PROTOCOL_VERSION)),
            generation: AtomicU64::new(0),
            renewing: tokio::sync::Mutex::new(()),
            closed: watch::channel(false).0,
        })
    }

    pub(super) fn learned(&self) -> Vec<Vec<u8>> {
        self.learned.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Запрос и его ответ: `result` как есть или текст ошибки сервера. Закрытие прерывает ожидание.
    pub(super) async fn request(&self, method: &str, params: Value) -> Response {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            biased;
            () = wait_closed(&mut closed) => Err(EXITED.to_owned()),
            answered = self.request_renewing(method, &params) => answered,
        }
    }

    /// Сервер ответил 404 на запрос с сессией: она истекла. Тогда сессия начинается заново (`initialize`), и запрос
    /// повторяется один раз.
    async fn request_renewing(&self, method: &str, params: &Value) -> Response {
        let generation = self.generation.load(Ordering::SeqCst);
        match self.exchange(method, params).await? {
            Asked::Answer(answered) => return answered,
            Asked::Expired => {}
        }
        self.renew(generation).await?;
        match self.exchange(method, params).await? {
            Asked::Answer(answered) => answered,
            Asked::Expired => Err("сессия сервера истекла снова сразу после нового initialize".to_owned()),
        }
    }

    /// Один запрос без повторов.
    async fn exchange(&self, method: &str, params: &Value) -> Result<Asked, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let initialize = method == "initialize";
        let asked = self.ask(id, initialize, &message).await?;
        if initialize
            && let Asked::Answer(Ok(answered)) = &asked
            && let Some(version) = answered.get("protocolVersion").and_then(Value::as_str)
            && let Ok(version) = HeaderValue::from_str(version)
        {
            *self.version.lock().unwrap_or_else(PoisonError::into_inner) = version;
        }
        Ok(asked)
    }

    /// Новая сессия: заголовки (с новым запуском `headersHelper`), `initialize` и `initialized`. Если сессию уже
    /// начали заново после того, как запрос был отправлен (`generation`), ничего не делает.
    async fn renew(&self, generation: u64) -> Result<(), String> {
        let _turn = self.renewing.lock().await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return Ok(());
        }
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let resolved = self.source.resolve().await?;
        *self.headers.lock().unwrap_or_else(PoisonError::into_inner) = resolved.headers;
        self.learned.lock().unwrap_or_else(PoisonError::into_inner).extend(resolved.learned);
        match self.exchange("initialize", &initialize_params()).await? {
            Asked::Answer(answered) => drop(answered?),
            Asked::Expired => return Err("сервер отверг новый initialize как истёкшую сессию".to_owned()),
        }
        self.notify_inner("notifications/initialized").await?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn ask(&self, id: u64, initialize: bool, message: &Value) -> Result<Asked, String> {
        let Posted::Reply(response) = self.post(message, initialize).await? else {
            return Ok(Asked::Expired);
        };
        if initialize && let Some(session) = response.headers().get(SESSION_ID) {
            *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(session.clone());
        }
        let is_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if is_stream {
            return Ok(Asked::Answer(self.stream_answer(response, id).await));
        }
        let body = read_body(response).await?;
        let message: Value = serde_json::from_slice(&body).map_err(|_| "ответ не JSON".to_owned())?;
        Ok(Asked::Answer(match answer(&message) {
            Some((got, answered)) if got == id => answered,
            _ => Err("в ответе нет ответа на запрос".to_owned()),
        }))
    }

    /// Уведомление: сервер отвечает без тела (`202`).
    pub(super) async fn notify(&self, method: &str) -> Result<(), String> {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            biased;
            () = wait_closed(&mut closed) => Err(EXITED.to_owned()),
            sent = self.notify_inner(method) => sent,
        }
    }

    async fn notify_inner(&self, method: &str) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method});
        match self.post(&message, false).await? {
            Posted::Reply(_) => Ok(()),
            Posted::Expired => Err("сессия сервера истекла".to_owned()),
        }
    }

    /// `404` на запрос с сессией — она истекла; остальные неуспешные статусы — ошибка.
    async fn post(&self, message: &Value, initialize: bool) -> Result<Posted, String> {
        let headers = self.headers.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut request = self
            .client
            .post(self.url.clone())
            .headers(headers)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(message.to_string());
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let had_session = session.is_some();
        if let Some(session) = session {
            request = request.header(SESSION_ID, session);
        }
        if !initialize {
            let version = self.version.lock().unwrap_or_else(PoisonError::into_inner).clone();
            request = request.header(PROTOCOL_HEADER, version);
        }
        let response = request.send().await.map_err(net)?;
        if had_session && response.status() == StatusCode::NOT_FOUND {
            return Ok(Posted::Expired);
        }
        check_status(&self.name, &response)?;
        Ok(Posted::Reply(response))
    }

    /// Прерывает вызовы в полёте и удаляет сессию, если сервер её выдал.
    pub(super) async fn shutdown(&self) {
        self.closed.send_replace(true);
        let session = self.session.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(session) = session else { return };
        let version = self.version.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let headers = self.headers.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let delete = self
            .client
            .delete(self.url.clone())
            .headers(headers)
            .header(SESSION_ID, session)
            .header(PROTOCOL_HEADER, version)
            .send();
        // Сервер вправе не поддерживать удаление (405) и вправе не ответить: сессия всё равно закрыта.
        drop(timeout(DELETE_TIMEOUT, delete).await);
    }

    /// Ждёт в потоке событий ответ на запрос `id`; запросы сервера в этом потоке получают ответ отдельным POST,
    /// остальные сообщения пропускаются.
    async fn stream_answer(&self, mut response: HttpResponse, id: u64) -> Response {
        let mut events = Events::default();
        while let Some(chunk) = response.chunk().await.map_err(net)? {
            for event in events.push(&chunk)? {
                if event.name != "message" {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                if let Some(reply) = server_reply(&self.root, &message) {
                    self.post(&reply, false).await.map(drop).unwrap_or_default();
                    continue;
                }
                if let Some((got, answered)) = answer(&message)
                    && got == id
                {
                    return answered;
                }
            }
        }
        Err("поток закрыт без ответа".to_owned())
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/mcp/http.rs"]
mod tests;
