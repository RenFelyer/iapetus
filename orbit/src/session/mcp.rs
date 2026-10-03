//! MCP проектных серверов runner для агента: второй MCP на `127.0.0.1` контейнера, который пересылает вызовы runner.
//!
//! Сводку приносит server (`Control::McpCatalog`), сессия показывает её агенту как инструменты, ресурсы и prompts с
//! префиксом сервера. Это HTTP с ответами `application/json`: без SSE и без токена, сокет виден только процессам
//! контейнера. Вызов уходит по каналу данных текущего runner и не повторяется: обрыв оставляет результат неизвестным.
//!
//! Имя инструмента и prompt — `<сервер>__<имя>`. Агент принимает полное имя `mcp__iapetus-project__<имя>` из
//! `[a-zA-Z0-9_-]` не длиннее предела профиля (`Profile::mcp_name_limit`), поэтому недопустимые символы заменяются
//! на `_`, а изменённое или длинное имя заканчивается хешем исходной пары. Имя, которое совпало бы с другим, тоже
//! получает хеш. URI ресурса — `mcp-project://<сервер>/<исходный URI>`, оба куска закодированы процентами.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use nerve::protocol::{self, DataRequest, MAX_MCP_RESULT, McpCall, McpCatalog, McpError, McpResponse, McpServer};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::data::Link;

#[cfg(test)]
#[path = "../../tests/session/mcp.rs"]
mod tests;

/// Путь MCP: адрес для агента — `http://127.0.0.1:<порт>` и этот путь.
pub(super) const PATH: &str = "/mcp";

/// Имя сервера в конфигах агентов.
pub(super) const SERVER_NAME: &str = "iapetus-project";

/// Сколько запуск агента ждёт первую сводку после подключения runner, прежде чем стартовать без неё.
pub(super) const FIRST_CATALOG_WAIT: Duration = Duration::from_secs(30);

/// Версии протокола MCP, которые понимает сервер, от старой к новой.
const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Что говорит агенту обрыв связи с runner во время вызова.
const LOST: &str = "связь оборвалась, результат неизвестен";

/// Наибольшая длина имени без префикса `mcp__iapetus-project__` при пределе `limit` на полное имя.
fn max_name(limit: usize) -> usize {
    limit.saturating_sub("mcp__".len() + SERVER_NAME.len() + "__".len())
}

/// Сколько шестнадцатеричных цифр хеша получает изменённое имя.
const HASH_DIGITS: usize = 10;

/// Начало URI ресурса, который видит агент.
const URI_PREFIX: &str = "mcp-project://";

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;

/// Сводка проектных серверов, как её видит агент, и канал данных, по которому идут вызовы. Копии разделяют состояние.
#[derive(Debug, Clone)]
pub(super) struct Project {
    /// `None` — сводки нет (runner ещё не прислал, или назначение снято); пустая сводка — `Some` без серверов.
    catalog: Arc<watch::Sender<Option<Arc<Index>>>>,
    link: Link,
    /// Наибольшая длина имени без префикса сервера.
    max_name: usize,
}

impl Project {
    /// `name_limit` — наибольшая длина полного имени инструмента у агента.
    pub(super) fn new(link: Link, name_limit: usize) -> Self {
        Self {
            catalog: Arc::new(watch::channel(None).0),
            link,
            max_name: max_name(name_limit),
        }
    }

    /// Заменяет сводку целиком; пустая снимает прежнюю.
    pub(super) fn set(&self, catalog: McpCatalog) {
        self.catalog.send_replace(Some(Arc::new(Index::new(catalog, self.max_name))));
    }

    /// Забывает сводку: до новой от нового runner её нет.
    pub(super) fn reset(&self) {
        self.catalog.send_replace(None);
    }

    /// Ждёт первую сводку, но не дольше [`FIRST_CATALOG_WAIT`]; без неё агент стартует с предупреждением.
    pub(super) async fn wait_first_catalog(&self) {
        let mut catalog = self.catalog.subscribe();
        let received = tokio::time::timeout(FIRST_CATALOG_WAIT, catalog.wait_for(Option::is_some))
            .await
            .is_ok();
        if !received {
            tracing::warn!(wait = ?FIRST_CATALOG_WAIT, "сводка MCP проекта не пришла: агент стартует без проектных MCP-серверов");
        }
    }

    /// Текущая сводка; без неё — пустая.
    fn index(&self) -> Arc<Index> {
        self.catalog.borrow().clone().unwrap_or_else(|| Arc::new(Index::empty()))
    }

    /// Вызов runner. `Err` — текст для агента: ошибка сервера или обрыв связи.
    async fn call(&self, call: McpCall) -> Result<String, String> {
        let Some(mux) = self.link.current() else {
            return Err(LOST.to_owned());
        };
        let Ok(body) = protocol::encode(&DataRequest::Mcp(call)) else {
            return Err("вызов не закодирован".to_owned());
        };
        let Ok(reply) = mux.request(Bytes::from(body)).await else {
            return Err(LOST.to_owned());
        };
        match protocol::decode_body::<McpResponse>(&reply) {
            Ok(Ok(result)) if result.len() > MAX_MCP_RESULT => Err("результат больше предела".to_owned()),
            Ok(Ok(result)) => Ok(result),
            Ok(Err(McpError(text))) => Err(text),
            Err(_) => Err(LOST.to_owned()),
        }
    }

    async fn dispatch(&self, method: &str, params: &Value) -> Result<String, RpcError> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok("{}".to_owned()),
            "tools/list" => Ok(self.index().list_tools()),
            "tools/call" => self.call_tool(params).await,
            "resources/list" => Ok(self.index().list_resources()),
            "resources/read" => self.read_resource(params).await,
            "prompts/list" => Ok(self.index().list_prompts()),
            "prompts/get" => self.get_prompt(params).await,
            _ => Err(RpcError::new(METHOD_NOT_FOUND, format!("метод {method} не поддерживается"))),
        }
    }

    async fn call_tool(&self, params: &Value) -> Result<String, RpcError> {
        let name = text_param(params, "name")?;
        let (server, tool) = self
            .index()
            .find_tool(name)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("инструмента {name} нет")))?;
        let call = McpCall::Tool {
            server,
            tool,
            arguments: arguments_param(params),
        };
        Ok(match self.call(call).await {
            Ok(result) => result,
            Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }).to_string(),
        })
    }

    async fn read_resource(&self, params: &Value) -> Result<String, RpcError> {
        let uri = text_param(params, "uri")?;
        let (server, uri) = self
            .index()
            .find_resource(uri)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("ресурса {uri} нет")))?;
        self.call(McpCall::ReadResource { server, uri })
            .await
            .map_err(|text| RpcError::new(INTERNAL_ERROR, text))
    }

    async fn get_prompt(&self, params: &Value) -> Result<String, RpcError> {
        let name = text_param(params, "name")?;
        let (server, name) = self
            .index()
            .find_prompt(name)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("prompt {name} нет")))?;
        let call = McpCall::GetPrompt {
            server,
            name,
            arguments: arguments_param(params),
        };
        self.call(call).await.map_err(|text| RpcError::new(INTERNAL_ERROR, text))
    }
}

/// Строковый параметр запроса.
fn text_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("нет параметра {name}")))
}

/// Аргументы вызова JSON-строкой; без них — пустой объект.
fn arguments_param(params: &Value) -> String {
    params.get("arguments").map_or_else(|| "{}".to_owned(), Value::to_string)
}

/// Ответ на `initialize`: версия протокола — запрошенная, если она поддержана, иначе самая новая.
fn initialize(params: &Value) -> String {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|version| Some(**version) == requested)
        .unwrap_or(&PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
    })
    .to_string()
}

#[derive(Debug)]
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: String) -> Self {
        Self { code, message }
    }
}

/// Сводка с именами, которые видит агент.
#[derive(Debug)]
struct Index {
    catalog: McpCatalog,
    tools: Vec<Named>,
    prompts: Vec<Named>,
}

/// Имя агента и место исходного в сводке: сервер и номер.
#[derive(Debug)]
struct Named {
    name: String,
    server: usize,
    item: usize,
}

impl Index {
    fn empty() -> Self {
        Self::new(McpCatalog::default(), 0)
    }

    fn new(catalog: McpCatalog, max_name: usize) -> Self {
        let tools = names(&catalog, max_name, |server| {
            server.tools.iter().map(|tool| tool.name.as_str()).collect()
        });
        let prompts = names(&catalog, max_name, |server| {
            server.prompts.iter().map(|prompt| prompt.name.as_str()).collect()
        });
        Self { catalog, tools, prompts }
    }

    fn find_tool(&self, name: &str) -> Option<(String, String)> {
        let found = self.tools.iter().find(|tool| tool.name == name)?;
        let server = &self.catalog.servers[found.server];
        Some((server.name.clone(), server.tools[found.item].name.clone()))
    }

    fn find_prompt(&self, name: &str) -> Option<(String, String)> {
        let found = self.prompts.iter().find(|prompt| prompt.name == name)?;
        let server = &self.catalog.servers[found.server];
        Some((server.name.clone(), server.prompts[found.item].name.clone()))
    }

    /// Сервер и исходный URI ресурса по URI агента; ресурса нет в сводке — `None`.
    fn find_resource(&self, uri: &str) -> Option<(String, String)> {
        let (server, original) = uri.strip_prefix(URI_PREFIX)?.split_once('/')?;
        let (server, original) = (decode(server)?, decode(original)?);
        let known = self.catalog.servers.iter().find(|candidate| candidate.name == server)?;
        known
            .resources
            .iter()
            .any(|resource| resource.uri == original)
            .then_some((server, original))
    }

    fn list_tools(&self) -> String {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|found| {
                let tool = &self.catalog.servers[found.server].tools[found.item];
                let schema = serde_json::from_str(&tool.input_schema).unwrap_or_else(|_| json!({ "type": "object" }));
                json!({ "name": found.name, "description": tool.description, "inputSchema": schema })
            })
            .collect();
        json!({ "tools": tools }).to_string()
    }

    fn list_resources(&self) -> String {
        let resources: Vec<Value> = self
            .catalog
            .servers
            .iter()
            .flat_map(|server| {
                server.resources.iter().map(|resource| {
                    json!({
                        "uri": format!("{URI_PREFIX}{}/{}", encode(&server.name), encode(&resource.uri)),
                        "name": resource.name,
                        "description": resource.description,
                        "mimeType": resource.mime_type,
                    })
                })
            })
            .collect();
        json!({ "resources": resources }).to_string()
    }

    fn list_prompts(&self) -> String {
        let prompts: Vec<Value> = self
            .prompts
            .iter()
            .map(|found| {
                let prompt = &self.catalog.servers[found.server].prompts[found.item];
                let arguments: Vec<Value> = prompt
                    .arguments
                    .iter()
                    .map(|argument| json!({ "name": argument.name, "description": argument.description, "required": argument.required }))
                    .collect();
                json!({ "name": found.name, "description": prompt.description, "arguments": arguments })
            })
            .collect();
        json!({ "prompts": prompts }).to_string()
    }
}

/// Имена агента для всех элементов одного вида (`items` выбирает имена элементов сервера), в порядке сводки. Имена
/// не повторяются: повтор одной и той же пары отбрасывается, а разные пары с одним именем получают хеш.
fn names(catalog: &McpCatalog, max_name: usize, items: impl Fn(&McpServer) -> Vec<&str>) -> Vec<Named> {
    let mut all = Vec::new();
    for (server_index, server) in catalog.servers.iter().enumerate() {
        for (item, name) in items(server).into_iter().enumerate() {
            all.push((server_index, item, server.name.as_str(), name));
        }
    }
    let plain: Vec<(String, bool)> = all.iter().map(|(_, _, server, name)| plain_name(server, name, max_name)).collect();
    let mut named: Vec<Named> = Vec::new();
    for (&(server_index, item, server, name), (text, with_hash)) in all.iter().zip(&plain) {
        let own = (server, name);
        let clashes = all
            .iter()
            .zip(&plain)
            .any(|(&(_, _, other_server, other_name), (other, _))| other == text && (other_server, other_name) != own);
        let name = if *with_hash || !clashes {
            text.clone()
        } else {
            hashed(server, name, text, max_name)
        };
        if named.iter().all(|existing| existing.name != name) {
            named.push(Named {
                name,
                server: server_index,
                item,
            });
        }
    }
    named
}

/// Имя без хеша и признак того, что оно уже с хешем: изменённое символами или длиной имя получает хеш сразу.
fn plain_name(server: &str, name: &str, max_name: usize) -> (String, bool) {
    let original = format!("{server}__{name}");
    let sanitized: String = original
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == original && sanitized.len() <= max_name {
        return (sanitized, false);
    }
    (hashed(server, name, &sanitized, max_name), true)
}

/// Начало имени `sanitized`, сокращённое до предела с хешем исходной пары на конце.
fn hashed(server: &str, name: &str, sanitized: &str, max_name: usize) -> String {
    let digest = Sha256::digest(format!("{server}\0{name}"));
    let mut hash = String::new();
    for byte in digest.iter().take(HASH_DIGITS / 2) {
        write!(hash, "{byte:02x}").unwrap_or_default();
    }
    let stem: String = sanitized.chars().take(max_name - 1 - HASH_DIGITS).collect();
    format!("{stem}_{hash}")
}

/// Процентное кодирование: без изменений остаются только символы `[A-Za-z0-9._~-]`.
fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").unwrap_or_default();
        }
    }
    encoded
}

/// Обратное к [`encode`]; `None` — кодирование нарушено или результат не UTF-8.
fn decode(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = text.bytes();
    while let Some(byte) = rest.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = char::from(rest.next()?).to_digit(16)?;
        let low = char::from(rest.next()?).to_digit(16)?;
        bytes.push(u8::try_from(high * 16 + low).ok()?);
    }
    String::from_utf8(bytes).ok()
}

/// Маршруты MCP.
pub(super) fn router(project: Project) -> Router {
    Router::new().route(PATH, post(handle)).with_state(project)
}

/// Обслуживает MCP на `listener`, пока не будет ошибки.
pub(super) async fn serve(listener: TcpListener, project: Project) -> std::io::Result<()> {
    axum::serve(listener, router(project)).await
}

/// Один JSON-RPC запрос; запрос с `Origin` — 403, пакет (массив) — 400 и `-32600`. Уведомление и ответ клиента
/// (нет `method` или `id`) принимаются с 202 без тела.
async fn handle(State(project): State<Project>, headers: HeaderMap, body: Bytes) -> Response {
    // Клиенты агентов не браузеры и `Origin` не шлют: он есть только у запроса веб-страницы.
    if headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return json_response(&error_body(&Value::Null, PARSE_ERROR, "JSON не разобран"));
    };
    if message.is_array() {
        let mut response = json_response(&error_body(&Value::Null, INVALID_REQUEST, "пакеты JSON-RPC не поддерживаются"));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return response;
    }
    let (Some(method), Some(id)) = (message.get("method").and_then(Value::as_str), message.get("id")) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    let reply = match project.dispatch(method, params).await {
        // Результат runner вставляется как есть, без разбора и пересборки.
        Ok(result) => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
        Err(error) => error_body(id, error.code, &error.message),
    };
    json_response(&reply)
}

fn error_body(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn json_response(body: &str) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body.to_owned()).into_response()
}
