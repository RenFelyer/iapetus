//! MCP-сервер Iapetus для агента: четыре инструмента (`runner_list`, `runner_use`, `runner_clone`,
//! `session_status`) по HTTP (streamable) на внутреннем адресе server. Это единственный MCP, который видит агент.
//!
//! Каждая session получает свой токен MCP, отдельный от её секрета для входа по SSH. Токен знает, чья он:
//! инструменты действуют только на сессию токена, а запрос без действующего токена отклоняется до MCP.
//! Токен видит агент, поэтому он не даёт ничего сверх этих четырёх инструментов своей сессии.
//!
//! Токен — `<fingerprint>.<HMAC-SHA256 от fingerprint на ключе server>`: server не хранит выданное, а контейнер
//! session переживает перезапуск server с тем же токеном. Сессию, которой нет в реестре, инструменты не знают,
//! поэтому удалённая сессия теряет доступ вместе с записью.

mod tools;

use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use hmac::{Hmac, KeyInit as _, Mac as _};
use nerve::fingerprint::Fingerprint;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sha2::Sha256;
use tokio::net::TcpListener;

use super::control::Control;

/// Путь MCP-сервера: URL для агента — адрес внутреннего слушателя и этот путь.
pub(crate) const PATH: &str = "/mcp";

const KEY_LEN: usize = 32;

/// Ключ, которым server подписывает токены MCP.
pub(crate) struct Tokens {
    mac: Hmac<Sha256>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(***)")
    }
}

impl Tokens {
    /// Читает ключ из файла; если файла нет, создаёт его со случайным ключом и правами `0600`. Файл с правами шире
    /// `0600` или не из 32 байт — ошибка: молча чинить ключ нельзя.
    pub(crate) fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        match OpenOptions::new().read(true).open(path) {
            Ok(file) => {
                if file.metadata()?.permissions().mode() & 0o077 != 0 {
                    bail!("права файла ключа MCP {} шире 0600", path.display());
                }
                let mut bytes = Vec::new();
                file.take(KEY_LEN as u64 + 1).read_to_end(&mut bytes)?;
                let key: [u8; KEY_LEN] = bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("ключ MCP {} не из {KEY_LEN} байт", path.display()))?;
                Self::from_key(&key)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; KEY_LEN];
                getrandom::fill(&mut key).context("случайные байты для ключа MCP")?;
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
                file.write_all(&key)?;
                file.sync_all()?;
                Self::from_key(&key)
            }
            Err(error) => Err(error).with_context(|| format!("ключ MCP {}", path.display())),
        }
    }

    fn from_key(key: &[u8; KEY_LEN]) -> anyhow::Result<Self> {
        let mac = Hmac::<Sha256>::new_from_slice(key).context("ключ MCP не принят HMAC")?;
        Ok(Self { mac })
    }

    /// Подпись сессии: HMAC от её fingerprint.
    fn mac(&self, session: &Fingerprint) -> Hmac<Sha256> {
        let mut mac = self.mac.clone();
        mac.update(session.as_str().as_bytes());
        mac
    }

    /// Токен MCP сессии: server кладёт его в окружение её контейнера.
    pub(crate) fn issue(&self, session: &Fingerprint) -> String {
        let tag = self.mac(session).finalize().into_bytes();
        format!("{session}.{}", encode_hex(&tag))
    }

    /// Сессия, которой выдан токен; `None` — токен чужой, подделан или повреждён.
    pub(in crate::server) fn session(&self, token: &str) -> Option<Fingerprint> {
        let (session, tag) = token.split_once('.')?;
        let session = Fingerprint::from_str(session).ok()?;
        let tag = decode_hex(tag)?;
        self.mac(&session).verify_slice(&tag).ok()?;
        Some(session)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|digit| char::from(DIGITS[usize::from(digit)]))
        .collect()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Сессия, от имени которой пришёл запрос: кладётся в запрос после проверки токена.
#[derive(Debug, Clone)]
struct Caller(Fingerprint);

/// Пускает дальше только запрос с действующим токеном MCP в `Authorization: Bearer`.
async fn authenticate(State(tokens): State<Arc<Tokens>>, mut request: Request, next: Next) -> Response {
    let session = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| tokens.session(token));
    let Some(session) = session else {
        let mut denied = Response::new(axum::body::Body::empty());
        *denied.status_mut() = StatusCode::UNAUTHORIZED;
        return denied;
    };
    request.extensions_mut().insert(Caller(session));
    next.run(request).await
}

/// Маршруты MCP-сервера.
pub(crate) fn router(control: Arc<Control>, tokens: Arc<Tokens>) -> Router {
    // Сессий MCP нет: каждый запрос самодостаточен и проверяется токеном, а не идентификатором сессии MCP.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        // Защита от DNS rebinding рассчитана на браузер и локальные серверы; здесь вход закрывает токен, а
        // контейнер обращается по имени server в сети Docker.
        .disable_allowed_hosts()
        // Клиенты агентов не браузеры и `Origin` не шлют; любой присутствующий отклоняется с 403.
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(tools::Tools::new(Arc::clone(&control))),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .nest_service(PATH, service)
        .layer(middleware::from_fn_with_state(tokens, authenticate))
}

/// Обслуживает MCP на `listener`, пока не будет ошибки.
pub(crate) async fn serve(listener: TcpListener, control: Arc<Control>, tokens: Arc<Tokens>) -> std::io::Result<()> {
    axum::serve(listener, router(control, tokens)).await
}

#[cfg(test)]
#[path = "../../tests/server/mcp.rs"]
mod tests;
