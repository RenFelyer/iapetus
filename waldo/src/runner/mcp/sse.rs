//! Клиент MCP по устаревшему SSE: `GET url` открывает поток событий, первое событие `endpoint` задаёт адрес для
//! POST, ответы на запросы приходят событиями `message`.
//!
//! Адрес из `endpoint` должен быть на том же хосте, что и `url`: заголовки с секретами не должны уйти на другой.
//! Перенаправления не выполняются.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde_json::{Value, json};
use tokio::task::AbortHandle;

use super::client::{Pending, Response, answer, server_reply};
use super::events::Events;
use super::headers::Headers;
use super::http::{build_client, check_status, net, parse_url};

#[derive(Debug)]
pub(super) struct Sse {
    name: String,
    root: String,
    learned: Vec<Vec<u8>>,
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    next_id: AtomicU64,
    pending: Pending,
    reader: Mutex<Option<AbortHandle>>,
}

impl Sse {
    /// Открывает поток и ждёт `endpoint`; `headersHelper` запускается один раз перед подключением.
    pub(super) async fn connect(name: &str, url: &str, headers: Headers, root: String) -> Result<Arc<Self>, String> {
        let resolved = headers.resolve().await?;
        let (client, base, headers) = (build_client()?, parse_url(url)?, resolved.headers);
        let mut stream = client
            .get(base.clone())
            .headers(headers.clone())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(net)?;
        check_status(name, &stream)?;
        let mut events = Events::default();
        let endpoint = 'wait: loop {
            let Some(chunk) = stream.chunk().await.map_err(net)? else {
                return Err("поток закрыт до endpoint".to_owned());
            };
            for event in events.push(&chunk)? {
                if event.name == "endpoint" {
                    break 'wait event.data;
                }
            }
        };
        let endpoint = base.join(&endpoint).map_err(|e| format!("endpoint не разобран: {e}"))?;
        if endpoint.origin() != base.origin() {
            return Err("endpoint указывает на другой хост".to_owned());
        }
        let sse = Arc::new(Self {
            name: name.to_owned(),
            root,
            learned: resolved.learned,
            client,
            endpoint,
            headers,
            next_id: AtomicU64::new(1),
            pending: Pending::default(),
            reader: Mutex::new(None),
        });
        let reader = tokio::spawn(Arc::clone(&sse).read_loop(stream, events));
        *sse.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader.abort_handle());
        Ok(sse)
    }

    /// Читает события до конца потока; потом отпускает всех, кто ждёт ответа.
    async fn read_loop(self: Arc<Self>, mut stream: reqwest::Response, mut events: Events) {
        loop {
            let chunk = match stream.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %net(e), "поток MCP-сервера не прочитан");
                    break;
                }
            };
            let Ok(received) = events.push(&chunk) else {
                tracing::warn!("событие MCP-сервера больше предела");
                break;
            };
            for event in received.iter().filter(|event| event.name == "message") {
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                if let Some(reply) = server_reply(&self.root, &message) {
                    self.post(&reply).await.unwrap_or_default();
                } else if let Some((id, response)) = answer(&message) {
                    self.pending.resolve(id, response);
                }
            }
        }
        self.pending.close();
    }

    pub(super) async fn request(&self, method: &str, params: Value) -> Response {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.pending.ask(id, self.post(&message)).await
    }

    pub(super) async fn notify(&self, method: &str) -> Result<(), String> {
        self.post(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn post(&self, message: &Value) -> Result<(), String> {
        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(message.to_string())
            .send()
            .await
            .map_err(net)?;
        check_status(&self.name, &response)
    }

    pub(super) fn learned(&self) -> Vec<Vec<u8>> {
        self.learned.clone()
    }

    /// Закрывает поток; вызовы в полёте получают ошибку.
    pub(super) fn shutdown(&self) {
        if let Some(reader) = self.reader.lock().unwrap_or_else(PoisonError::into_inner).take() {
            reader.abort();
        }
        self.pending.close();
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/mcp/sse.rs"]
mod tests;
