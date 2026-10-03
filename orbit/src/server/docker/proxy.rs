//! HTTP-сервер прокси на unix-сокете и клиент к `docker.sock`.
//!
//! К Docker уходит новый запрос: заголовки клиента не пересылаются, путь и запрос собраны из
//! разобранной операции, тело `create` — из проверенных полей. Операции над контейнером сначала
//! читают его через inspect, проверяют метку и дальше работают по полному id из ответа Docker,
//! чтобы имя нельзя было перенацелить между проверкой и действием.

use std::convert::Infallible;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, TRANSFER_ENCODING, UPGRADE};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{UnixListener, UnixStream};

use super::route::{self, ContainerOp, Route, Routed, render_query};
use super::template::{ROLE_LABEL, ROLE_VALUE, Template};
use super::{Denied, is_name};

/// Тело запроса `create` с запасом: настоящее — сотни байт.
const MAX_REQUEST_BODY: usize = 64 * 1024;
/// Ответ inspect, который прокси читает сам.
const MAX_INSPECT: usize = 4 * 1024 * 1024;
/// Фильтр списка контейнеров `{"label":["iapetus.role=session"]}` в виде значения запроса.
const LIST_FILTER: &str = "filters=%7B%22label%22%3A%5B%22iapetus.role%3Dsession%22%5D%7D";

type Body = BoxBody<Bytes, hyper::Error>;
type Reply = Response<Body>;

#[derive(Debug)]
struct State {
    upstream: PathBuf,
    template: Template,
}

/// Открывает сокет прокси с правами `0600`, заменяя оставшийся от прошлого запуска.
pub(crate) fn bind(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Принимает соединения на `listener`, пока не оборвётся приём; `upstream` — путь к `docker.sock`.
pub(crate) async fn serve(listener: UnixListener, upstream: PathBuf, template: Template) -> io::Result<()> {
    let state = Arc::new(State { upstream, template });
    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(state.handle(request).await) }
            });
            if let Err(error) = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                tracing::debug!(%error, "соединение с прокси Docker оборвалось");
            }
        });
    }
}

enum Failure {
    Denied(Denied),
    Upstream(String),
    /// Ответ Docker, который идёт клиенту как есть (например, 404 при inspect).
    Relay(Box<Reply>),
}

impl From<Denied> for Failure {
    fn from(denied: Denied) -> Self {
        Self::Denied(denied)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Upstream(error.to_string())
    }
}

impl From<hyper::Error> for Failure {
    fn from(error: hyper::Error) -> Self {
        Self::Upstream(error.to_string())
    }
}

impl State {
    async fn handle(&self, request: Request<Incoming>) -> Reply {
        let (parts, body) = request.into_parts();
        match self.dispatch(&parts, body).await {
            Ok(reply) => reply,
            Err(Failure::Relay(reply)) => *reply,
            Err(Failure::Denied(denied)) => {
                tracing::warn!(method = %parts.method, path = %parts.uri.path().escape_default(), %denied, "прокси Docker отказал");
                error_reply(StatusCode::FORBIDDEN, &denied.to_string())
            }
            Err(Failure::Upstream(why)) => {
                tracing::warn!(%why, "Docker не ответил прокси");
                error_reply(StatusCode::BAD_GATEWAY, "docker is unavailable")
            }
        }
    }

    async fn dispatch(&self, parts: &Parts, body: Incoming) -> Result<Reply, Failure> {
        let Routed { prefix, route } = route::parse(&parts.method, parts.uri.path(), parts.uri.query())?;
        let body = Limited::new(body, MAX_REQUEST_BODY)
            .collect()
            .await
            .map_err(|_| Denied::Body)?
            .to_bytes();
        let takes_body = matches!(route, Route::ContainerCreate { .. } | Route::VolumeCreate);
        if !takes_body && !body.is_empty() {
            return Err(Denied::Body.into());
        }
        match route {
            Route::Ping => self.send(Method::GET, &format!("{prefix}/_ping"), None).await,
            Route::ContainerList { all } => {
                let path = format!("{prefix}/containers/json?all={all}&{LIST_FILTER}");
                self.send(Method::GET, &path, None).await
            }
            Route::ContainerCreate { name } => self.create_container(&prefix, &name, &body).await,
            Route::Container { id, op, query } => {
                let (full_id, inspect) = self.session_container(&prefix, &id).await?;
                if op == ContainerOp::Inspect {
                    return Ok(json_reply(inspect));
                }
                let path = format!("{prefix}/containers/{full_id}{}{}", op.suffix(), render_query(&query));
                self.send(op.method(), &path, None).await
            }
            Route::VolumeCreate => {
                let rebuilt = Template::check_volume(&body)?;
                self.send(Method::POST, &format!("{prefix}/volumes/create"), Some(rebuilt)).await
            }
            Route::VolumeInspect { name } => {
                let inspect = self.session_volume(&prefix, &name).await?;
                Ok(json_reply(inspect))
            }
            Route::VolumeRemove { name } => {
                self.session_volume(&prefix, &name).await?;
                self.send(Method::DELETE, &format!("{prefix}/volumes/{name}"), None).await
            }
        }
    }

    async fn create_container(&self, prefix: &str, name: &str, body: &[u8]) -> Result<Reply, Failure> {
        let checked = self.template.check_create(body)?;
        self.session_volume(prefix, &checked.volume).await?;
        self.send(Method::POST, &format!("{prefix}/containers/create?name={name}"), Some(checked.body))
            .await
    }

    /// Читает контейнер из Docker и возвращает его полный id и тело inspect, если метка сессии на месте.
    async fn session_container(&self, prefix: &str, id: &str) -> Result<(String, Bytes), Failure> {
        let inspect = self.inspect(&format!("{prefix}/containers/{id}/json")).await?;
        let full_id = inspect
            .json
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .filter(|full| full.len() == 64 && full.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or(Denied::NotSession)?
            .to_owned();
        require_session_label(inspect.json.pointer("/Config/Labels"))?;
        Ok((full_id, inspect.raw))
    }

    async fn session_volume(&self, prefix: &str, name: &str) -> Result<Bytes, Failure> {
        if !is_name(name) {
            return Err(Denied::Name.into());
        }
        let inspect = self.inspect(&format!("{prefix}/volumes/{name}")).await?;
        require_session_label(inspect.json.get("Labels"))?;
        Ok(inspect.raw)
    }

    async fn inspect(&self, path: &str) -> Result<Inspected, Failure> {
        let response = self.request(Method::GET, path, None).await?;
        if response.status() != StatusCode::OK {
            return Err(Failure::Relay(Box::new(relay(response))));
        }
        let raw = Limited::new(response.into_body(), MAX_INSPECT)
            .collect()
            .await
            .map_err(|error| Failure::Upstream(error.to_string()))?
            .to_bytes();
        let json = serde_json::from_slice(&raw).map_err(|error| Failure::Upstream(error.to_string()))?;
        Ok(Inspected { json, raw })
    }

    async fn send(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Reply, Failure> {
        Ok(relay(self.request(method, path, body).await?))
    }

    /// Новое соединение с `docker.sock` на один запрос: заголовков клиента в нём нет.
    async fn request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Response<Incoming>, Failure> {
        let stream = UnixStream::connect(&self.upstream).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "соединение с Docker оборвалось");
            }
        });
        let mut builder = Request::builder().method(method).uri(path).header(HOST, "docker");
        let body = match body {
            Some(bytes) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Bytes::from(bytes)
            }
            None => Bytes::new(),
        };
        let request = builder
            .body(Full::new(body))
            .map_err(|error| Failure::Upstream(error.to_string()))?;
        Ok(sender.send_request(request).await?)
    }
}

struct Inspected {
    json: serde_json::Value,
    raw: Bytes,
}

fn require_session_label(labels: Option<&serde_json::Value>) -> Result<(), Denied> {
    let role = labels.and_then(|labels| labels.get(ROLE_LABEL)).and_then(serde_json::Value::as_str);
    if role == Some(ROLE_VALUE) {
        Ok(())
    } else {
        Err(Denied::NotSession)
    }
}

/// Ответ Docker клиенту: статус, заголовки без управляющих соединением и поток тела.
fn relay(response: Response<Incoming>) -> Reply {
    let (mut parts, body) = response.into_parts();
    for name in [CONNECTION, TRANSFER_ENCODING, UPGRADE] {
        parts.headers.remove(name);
    }
    parts.headers.remove("keep-alive");
    Response::from_parts(parts, body.boxed())
}

fn json_reply(raw: Bytes) -> Reply {
    let mut reply = Response::new(Full::new(raw).map_err(|never| match never {}).boxed());
    reply
        .headers_mut()
        .insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
    reply
}

fn error_reply(status: StatusCode, message: &str) -> Reply {
    let body = serde_json::json!({ "message": message }).to_string();
    let mut reply = Response::new(Full::new(Bytes::from(body)).map_err(|never| match never {}).boxed());
    *reply.status_mut() = status;
    reply
        .headers_mut()
        .insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
    reply.headers_mut().remove(CONTENT_LENGTH);
    reply
}

#[cfg(test)]
#[path = "../../../tests/server/docker/proxy.rs"]
mod tests;
