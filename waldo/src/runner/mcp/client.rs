//! Минимальный клиент MCP: JSON-RPC 2.0 по stdio процесса сервера (одно сообщение на строку), по streamable HTTP
//! или по SSE. Рукопожатие и списки одинаковы для всех видов.
//!
//! Stdio-сервер — отдельная группа процессов; из неё же он убирается. Клиент объявляет возможность `roots` и на
//! запросы сервера отвечает сам: `ping` — пусто, `roots/list` — каталог проекта, остальное — ошибка `-32601`.
//! Запросы сервера доходят до клиента по stdio, по потоку SSE и внутри потока ответа на POST у streamable HTTP;
//! отдельного GET-потока у streamable HTTP нет, и запросы по нему сервер послать не может.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nerve::protocol::{MAX_MCP_RESULT, McpPrompt, McpPromptArgument, McpResource, McpServer, McpTool};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep, timeout};

use crate::runner::exec::{Masker, Secrets};
use crate::runner::reaper;

use super::headers::Headers;
use super::http::Http;
use super::sse::Sse;

/// Версия протокола, с которой клиент представляется серверу.
pub(super) const PROTOCOL_VERSION: &str = "2025-06-18";

/// Сколько сервер отвечает на `initialize` и на все списки вместе: `npx` и `uvx` при первом запуске скачивают пакеты.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Сколько страниц одного списка читать: сервер с бесконечным `nextCursor` не должен держать запуск.
const MAX_PAGES: usize = 100;

/// Наибольшее сообщение сервера: результат вызова целиком идёт одним сообщением.
pub(super) const MAX_MESSAGE: usize = MAX_MCP_RESULT + (1 << 20);

const MAX_LINE: u64 = MAX_MESSAGE as u64;

/// Сколько ждать после SIGTERM, прежде чем убить группу процессов SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Самая длинная строка stderr сервера в журнале: без перевода строки сервер не заполнит память, а длинная строка
/// уходит в журнал частями.
const MAX_STDERR_LINE: usize = 8 << 10;

const POLL: Duration = Duration::from_millis(20);

pub(super) const EXITED: &str = "сервер завершился";

pub(super) type Response = Result<Value, String>;

/// Параметры `initialize`: клиент объявляет только `roots`, без `listChanged`.
pub(super) fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"roots": {}},
        "clientInfo": {"name": "iapetus", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Ответ клиента на запрос сервера (сообщение с `method` и `id`); `None` — сообщение не запрос. `root` — адрес
/// каталога проекта для `roots/list`.
pub(super) fn server_reply(root: &str, message: &Value) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?;
    let method = message.get("method")?.as_str()?;
    Some(match method {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": [{"uri": root}]}}),
        _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "метод не поддерживается"}}),
    })
}

/// Ожидающие ответа запросы по их номерам.
#[derive(Debug)]
pub(super) struct Pending {
    /// `None` — ответов больше не будет.
    map: Mutex<Option<HashMap<u64, oneshot::Sender<Response>>>>,
    closed: watch::Sender<bool>,
}

impl Default for Pending {
    fn default() -> Self {
        Self {
            map: Mutex::new(Some(HashMap::new())),
            closed: watch::channel(false).0,
        }
    }
}

/// Убирает запрос из ожидающих, когда его перестали ждать.
struct Waiting<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(map) = self.pending.map().as_mut() {
            map.remove(&self.id);
        }
    }
}

impl Pending {
    fn map(&self) -> MutexGuard<'_, Option<HashMap<u64, oneshot::Sender<Response>>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Отправляет запрос `id` через `send` и ждёт ответа, пока его не отдадут в `resolve` или не вызовут `close`.
    pub(super) async fn ask(&self, id: u64, send: impl Future<Output = Result<(), String>>) -> Response {
        let (reply, response) = oneshot::channel();
        match self.map().as_mut() {
            Some(map) => map.insert(id, reply),
            None => return Err(EXITED.to_owned()),
        };
        let _waiting = Waiting { pending: self, id };
        let mut closed = self.closed.subscribe();
        tokio::select! {
            biased;
            () = wait_closed(&mut closed) => Err(EXITED.to_owned()),
            answered = async {
                send.await?;
                response.await.unwrap_or_else(|_| Err(EXITED.to_owned()))
            } => answered,
        }
    }

    pub(super) fn resolve(&self, id: u64, response: Response) {
        if let Some(reply) = self.map().as_mut().and_then(|map| map.remove(&id)) {
            reply.send(response).unwrap_or_default();
        }
    }

    /// Ответов больше не будет: ждущие получают ошибку.
    pub(super) fn close(&self) {
        self.map().take();
        self.closed.send_replace(true);
    }
}

/// Ждёт, пока `closed` не станет истинным (или его отправитель не исчезнет).
pub(super) async fn wait_closed(closed: &mut watch::Receiver<bool>) {
    drop(closed.wait_for(|closed| *closed).await);
}

/// Номер и ответ из сообщения сервера; `None` — это не ответ.
pub(super) fn answer(message: &Value) -> Option<(u64, Response)> {
    if message.get("method").is_some() {
        return None;
    }
    let id = message.get("id")?.as_u64()?;
    let response = match message.get("error") {
        Some(error) => Err(error.get("message").and_then(Value::as_str).unwrap_or("ошибка сервера").to_owned()),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    Some((id, response))
}

/// Запущенный stdio-сервер.
#[derive(Debug)]
pub(super) struct Process {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    root: String,
    group: Pid,
    reader: Mutex<Option<AbortHandle>>,
    stderr: Mutex<Option<AbortHandle>>,
}

impl Process {
    /// Запускает сервер `name` в `dir` с окружением `vars` и начинает читать его вывод. Строки stderr сервера идут в
    /// журнал с замаскированными `secrets`. `root` — адрес каталога проекта для `roots/list`.
    pub(super) fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        vars: &BTreeMap<OsString, OsString>,
        dir: &Path,
        secrets: Secrets,
        root: String,
    ) -> Result<Arc<Self>, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(vars)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Своя группа: остановка убивает и потомков сервера.
            .process_group(0)
            .spawn()
            .map_err(|e| format!("не запущен: {e}"))?;
        let group = child.id().and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?));
        let (Some(group), Some(stdin), Some(stdout), Some(stderr)) = (group, child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            if let Some(pid) = child.id().and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?)) {
                kill_process_group(pid, Signal::KILL).unwrap_or_default();
            }
            return Err("у процесса нет pid, stdin, stdout или stderr".to_owned());
        };
        let mark = reaper::own_child(group.as_raw_nonzero().get().unsigned_abs());
        // Статус ребёнка забирает runner: без ожидания сервер остался бы зомби.
        tokio::spawn(async move {
            let _own = mark;
            match child.wait().await {
                Ok(status) => tracing::debug!(%status, "MCP-сервер завершился"),
                Err(e) => tracing::warn!(error = %e, "ожидание MCP-сервера не удалось"),
            }
        });
        let client = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Pending::default(),
            next_id: AtomicU64::new(1),
            root,
            group,
            reader: Mutex::new(None),
            stderr: Mutex::new(None),
        });
        let reader = tokio::spawn(Arc::clone(&client).read_loop(stdout));
        *client.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader.abort_handle());
        let log = tokio::spawn(log_stderr(name.to_owned(), stderr, secrets));
        *client.stderr.lock().unwrap_or_else(PoisonError::into_inner) = Some(log.abort_handle());
        Ok(client)
    }

    /// Останавливает сервер: SIGTERM группе процессов, через `KILL_GRACE` SIGKILL тем, кто остался; ждёт, пока
    /// группа опустеет. Вызовы в полёте получают ошибку.
    pub(super) async fn shutdown(&self) {
        kill_process_group(self.group, Signal::TERM).unwrap_or_default();
        let deadline = Instant::now() + KILL_GRACE;
        while test_kill_process_group(self.group).is_ok() && Instant::now() < deadline {
            sleep(POLL).await;
        }
        if test_kill_process_group(self.group).is_ok() {
            kill_process_group(self.group, Signal::KILL).unwrap_or_default();
        }
        for task in [&self.reader, &self.stderr] {
            if let Some(task) = task.lock().unwrap_or_else(PoisonError::into_inner).take() {
                task.abort();
            }
        }
        self.pending.close();
    }

    /// Запрос и его ответ: `result` как есть или текст ошибки сервера.
    async fn request(&self, method: &str, params: Value) -> Response {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.pending.ask(id, self.send(&message)).await
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn send(&self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|_| EXITED.to_owned())?;
        stdin.flush().await.map_err(|_| EXITED.to_owned())
    }

    /// Читает сообщения сервера до конца его вывода; потом отпускает всех, кто ждёт ответа.
    async fn read_loop(self: Arc<Self>, stdout: tokio::process::ChildStdout) {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_line(&mut reader, &mut line).await {
                Ok(true) => self.dispatch(&line).await,
                Ok(false) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "вывод MCP-сервера не прочитан");
                    break;
                }
            }
        }
        self.pending.close();
    }

    async fn dispatch(&self, line: &[u8]) {
        if line.trim_ascii().is_empty() {
            return;
        }
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            tracing::debug!("MCP-сервер прислал не JSON, строка пропущена");
            return;
        };
        if message.get("id").is_none_or(Value::is_null) {
            return;
        }
        if let Some(reply) = server_reply(&self.root, &message) {
            self.send(&reply).await.unwrap_or_default();
            return;
        }
        if let Some((id, response)) = answer(&message) {
            self.pending.resolve(id, response);
        }
    }
}

/// Сервер MCP: процесс, streamable HTTP или SSE.
#[derive(Debug)]
pub(super) enum Client {
    Stdio(Arc<Process>),
    Http(Box<Http>),
    Sse(Arc<Sse>),
}

impl Client {
    /// Запускает stdio-сервер `name` в `dir` с окружением `vars`; строки его stderr идут в журнал без `secrets`.
    pub(super) fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        vars: &BTreeMap<OsString, OsString>,
        dir: &Path,
        secrets: Secrets,
        root: String,
    ) -> Result<Arc<Self>, String> {
        Process::spawn(name, program, args, vars, dir, secrets, root).map(|stdio| Arc::new(Self::Stdio(stdio)))
    }

    /// Сервер `name` по streamable HTTP; кроме запуска `headersHelper`, сеть до `describe` не трогается.
    pub(super) async fn http(name: &str, url: &str, headers: Headers, root: String) -> Result<Arc<Self>, String> {
        Http::new(name, url, headers, root)
            .await
            .map(|http| Arc::new(Self::Http(Box::new(http))))
    }

    /// Сервер `name` по SSE: открывает поток и ждёт адрес для запросов не дольше `HANDSHAKE_TIMEOUT`.
    pub(super) async fn sse(name: &str, url: &str, headers: Headers, root: String) -> Result<Arc<Self>, String> {
        let connected = timeout(HANDSHAKE_TIMEOUT, Sse::connect(name, url, headers, root))
            .await
            .map_err(|_| "не ответил в срок".to_owned())?;
        connected.map(|sse| Arc::new(Self::Sse(sse)))
    }

    /// Значения, которые клиент узнал от `headersHelper` и которые нельзя показывать.
    pub(super) fn learned(&self) -> Vec<Vec<u8>> {
        match self {
            Self::Stdio(_) => Vec::new(),
            Self::Http(http) => http.learned(),
            Self::Sse(sse) => sse.learned(),
        }
    }

    /// Останавливает сервер; вызовы в полёте получают ошибку.
    pub(super) async fn shutdown(&self) {
        match self {
            Self::Stdio(stdio) => stdio.shutdown().await,
            Self::Http(http) => http.shutdown().await,
            Self::Sse(sse) => sse.shutdown(),
        }
    }

    /// Запрос и его ответ: `result` как есть или текст ошибки сервера.
    pub(super) async fn request(&self, method: &str, params: Value) -> Response {
        match self {
            Self::Stdio(stdio) => stdio.request(method, params).await,
            Self::Http(http) => http.request(method, params).await,
            Self::Sse(sse) => sse.request(method, params).await,
        }
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        match self {
            Self::Stdio(stdio) => stdio.notify(method).await,
            Self::Http(http) => http.notify(method).await,
            Self::Sse(sse) => sse.notify(method).await,
        }
    }

    /// `initialize` и списки того, что сервер объявил в возможностях; всё вместе не дольше `HANDSHAKE_TIMEOUT`.
    pub(super) async fn describe(&self, name: &str) -> Result<McpServer, String> {
        timeout(HANDSHAKE_TIMEOUT, self.describe_inner(name))
            .await
            .map_err(|_| "не ответил в срок".to_owned())?
    }

    async fn describe_inner(&self, name: &str) -> Result<McpServer, String> {
        let init = self.request("initialize", initialize_params()).await?;
        self.notify("notifications/initialized").await?;
        let declared = |capability: &str| init.pointer("/capabilities").is_some_and(|c| c.get(capability).is_some());
        let mut server = McpServer {
            name: name.to_owned(),
            instructions: init.get("instructions").and_then(Value::as_str).map(str::to_owned),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        };
        if declared("tools") {
            server.tools = self.list("tools/list", "tools").await?.iter().filter_map(tool).collect();
        }
        if declared("resources") {
            server.resources = self
                .list("resources/list", "resources")
                .await?
                .iter()
                .filter_map(resource)
                .collect();
        }
        if declared("prompts") {
            server.prompts = self.list("prompts/list", "prompts").await?.iter().filter_map(prompt).collect();
        }
        Ok(server)
    }

    /// Все страницы списка `key` из ответа на `method`.
    async fn list(&self, method: &str, key: &str) -> Result<Vec<Value>, String> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor.as_ref().map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.request(method, params).await?;
            if let Some(found) = page.get(key).and_then(Value::as_array) {
                items.extend(found.iter().cloned());
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }
}

/// Строки stderr сервера в журнал (`debug`), значения секретов заменены на `***`. Маскируется поток, а не строки:
/// секрет не выдаст себя ни на границе чтений, ни переводом строки внутри значения.
async fn log_stderr(name: String, mut stderr: impl AsyncRead + Unpin, secrets: Secrets) {
    let mut stream = Masker::new(secrets);
    let mut line = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = stderr.read(&mut chunk).await.unwrap_or(0);
        let safe = if read == 0 { stream.finish() } else { stream.push(&chunk[..read]) };
        for byte in safe {
            if byte != b'\n' {
                line.push(byte);
            }
            if byte == b'\n' || line.len() >= MAX_STDERR_LINE {
                log_line(&name, &mut line);
            }
        }
        if read == 0 {
            log_line(&name, &mut line);
            return;
        }
    }
}

fn log_line(name: &str, line: &mut Vec<u8>) {
    if !line.is_empty() {
        tracing::debug!(server = %name, stderr = %String::from_utf8_lossy(line), "stderr MCP-сервера");
    }
    line.clear();
}

/// Читает одну строку; `false` — вывод кончился. Строка длиннее `MAX_LINE` — ошибка, а не неограниченный буфер.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, line: &mut Vec<u8>) -> std::io::Result<bool> {
    let read = (&mut *reader).take(MAX_LINE).read_until(b'\n', line).await?;
    if read == 0 {
        return Ok(false);
    }
    if !line.ends_with(b"\n") && read as u64 >= MAX_LINE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "сообщение больше предела"));
    }
    Ok(true)
}

fn text(item: &Value, key: &str) -> String {
    item.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn tool(item: &Value) -> Option<McpTool> {
    Some(McpTool {
        name: item.get("name")?.as_str()?.to_owned(),
        description: text(item, "description"),
        input_schema: item
            .get("inputSchema")
            .map_or_else(|| r#"{"type":"object"}"#.to_owned(), Value::to_string),
    })
}

fn resource(item: &Value) -> Option<McpResource> {
    let uri = item.get("uri")?.as_str()?.to_owned();
    Some(McpResource {
        name: item.get("name").and_then(Value::as_str).map_or_else(|| uri.clone(), str::to_owned),
        uri,
        description: text(item, "description"),
        mime_type: text(item, "mimeType"),
    })
}

fn prompt(item: &Value) -> Option<McpPrompt> {
    let arguments = item.get("arguments").and_then(Value::as_array).map_or_else(Vec::new, |arguments| {
        arguments
            .iter()
            .filter_map(|argument| {
                Some(McpPromptArgument {
                    name: argument.get("name")?.as_str()?.to_owned(),
                    description: text(argument, "description"),
                    required: argument.get("required").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect()
    });
    Some(McpPrompt {
        name: item.get("name")?.as_str()?.to_owned(),
        description: text(item, "description"),
        arguments,
    })
}
