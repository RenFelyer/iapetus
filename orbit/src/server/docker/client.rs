//! Клиент Docker API для server: говорит с сокетом прокси, а не с `docker.sock`, поэтому каждый его запрос
//! проходит те же проверки шаблона, что и чужой.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

use super::template::{ROLE_LABEL, ROLE_VALUE};

/// Ответ Docker: код и тело.
struct Answer {
    status: StatusCode,
    body: Bytes,
}

impl Answer {
    /// Тело ошибки Docker — `{"message": …}`; причина идёт в сообщение.
    fn failure(&self, what: &str) -> anyhow::Error {
        let message = serde_json::from_slice::<Value>(&self.body)
            .ok()
            .and_then(|json| json.get("message").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        anyhow::anyhow!("{what}: Docker ответил {} {message}", self.status)
    }
}

/// Склеивает кадры журнала Docker: 8 байт заголовка (поток, три нуля, длина в big-endian) и данные. Оборванный
/// последний кадр отдаёт то, что в нём есть.
fn join_frames(raw: &[u8]) -> Vec<u8> {
    let mut text = Vec::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((header, after)) = rest.split_first_chunk::<8>() {
        let size = usize::try_from(u32::from_be_bytes([header[4], header[5], header[6], header[7]])).unwrap_or(usize::MAX);
        let (frame, next) = after.split_at(size.min(after.len()));
        text.extend_from_slice(frame);
        rest = next;
    }
    text
}

#[derive(Debug, Clone)]
pub(crate) struct Docker {
    socket: PathBuf,
}

impl Docker {
    /// `socket` — сокет прокси.
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Создаёт том; метка роли сессии добавляется к `labels` сама.
    pub(crate) async fn create_volume(&self, name: &str, labels: &BTreeMap<String, String>) -> Result<()> {
        let mut labels = labels.clone();
        labels.insert(ROLE_LABEL.to_owned(), ROLE_VALUE.to_owned());
        let body = serde_json::json!({ "Name": name, "Labels": labels });
        let answer = self.call(Method::POST, "/volumes/create", Some(&body)).await?;
        match answer.status {
            StatusCode::CREATED => Ok(()),
            _ => Err(answer.failure("том не создан")),
        }
    }

    /// Метки тома или `None`, если тома нет.
    pub(crate) async fn volume_labels(&self, name: &str) -> Result<Option<BTreeMap<String, String>>> {
        let answer = self.call(Method::GET, &format!("/volumes/{name}"), None).await?;
        match answer.status {
            StatusCode::OK => {
                let json: Value = serde_json::from_slice(&answer.body).context("ответ Docker о томе не разобран")?;
                let labels = serde_json::from_value(json.get("Labels").cloned().unwrap_or_default()).context("метки тома не разобраны")?;
                Ok(Some(labels))
            }
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(answer.failure("том не прочитан")),
        }
    }

    /// Удаляет том; тома, которого нет, это не ошибка.
    pub(crate) async fn remove_volume(&self, name: &str) -> Result<()> {
        let answer = self.call(Method::DELETE, &format!("/volumes/{name}"), None).await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(answer.failure("том не удалён")),
        }
    }

    pub(crate) async fn create_container(&self, name: &str, body: &Value) -> Result<()> {
        let answer = self
            .call(Method::POST, &format!("/containers/create?name={name}"), Some(body))
            .await?;
        match answer.status {
            StatusCode::CREATED => Ok(()),
            _ => Err(answer.failure("контейнер не создан")),
        }
    }

    pub(crate) async fn start_container(&self, name: &str) -> Result<()> {
        let answer = self.call(Method::POST, &format!("/containers/{name}/start"), None).await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED => Ok(()),
            _ => Err(answer.failure("контейнер не запущен")),
        }
    }

    /// Останавливает контейнер: ждёт `seconds`, затем убивает. Остановленного или отсутствующего контейнера
    /// это не касается.
    pub(crate) async fn stop_container(&self, name: &str, seconds: u32) -> Result<()> {
        let answer = self
            .call(Method::POST, &format!("/containers/{name}/stop?t={seconds}"), None)
            .await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(answer.failure("контейнер не остановлен")),
        }
    }

    /// Удаляет контейнер без тома; контейнера, которого нет, это не ошибка.
    pub(crate) async fn remove_container(&self, name: &str) -> Result<()> {
        let answer = self.call(Method::DELETE, &format!("/containers/{name}?force=true"), None).await?;
        match answer.status {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(answer.failure("контейнер не удалён")),
        }
    }

    /// Журнал контейнера (stdout и stderr) — последние `tail` строк; `None`, если контейнера нет. Контейнеры сессий
    /// без tty, поэтому Docker отдаёт кадры по потокам: их полезная нагрузка склеивается в текст.
    pub(crate) async fn container_logs(&self, name: &str, tail: u32) -> Result<Option<Vec<u8>>> {
        let path = format!("/containers/{name}/logs?stdout=true&stderr=true&tail={tail}");
        let answer = self.call(Method::GET, &path, None).await?;
        match answer.status {
            StatusCode::OK => Ok(Some(join_frames(&answer.body))),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(answer.failure("журнал контейнера не прочитан")),
        }
    }

    /// Имена всех контейнеров сессий, включая остановленные.
    pub(crate) async fn container_names(&self) -> Result<HashSet<String>> {
        let answer = self.call(Method::GET, "/containers/json?all=true", None).await?;
        if answer.status != StatusCode::OK {
            return Err(answer.failure("список контейнеров не прочитан"));
        }
        let list: Vec<Value> = serde_json::from_slice(&answer.body).context("список контейнеров не разобран")?;
        Ok(list
            .iter()
            .filter_map(|container| container.get("Names")?.as_array())
            .flatten()
            .filter_map(Value::as_str)
            .map(|name| name.trim_start_matches('/').to_owned())
            .collect())
    }

    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Answer> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("сокет прокси Docker {}", self.socket.display()))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "соединение с прокси Docker оборвалось");
            }
        });
        let mut builder = Request::builder().method(method).uri(path).header(HOST, "docker");
        let body = match body {
            Some(json) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Bytes::from(serde_json::to_vec(json)?)
            }
            None => Bytes::new(),
        };
        let Ok(request) = builder.body(Full::new(body)) else {
            bail!("запрос {path} не собран");
        };
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        Ok(Answer { status, body })
    }
}
