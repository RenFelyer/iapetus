//! MCP-серверы проекта: runner запускает stdio-серверы из `.mcp.json` рабочего каталога и подключается к серверам
//! `http` (streamable HTTP) и `sse` на время назначения, шлёт server их сводку и выполняет вызовы, пришедшие по каналу
//! данных.
//!
//! Файл читается один раз, при начале назначения. `${VAR}` и `${VAR:-значение}` в нём подставляются из окружения
//! команд сессии: в `command`, `args`, `env`, `cwd` у stdio, в `url` и `headers` у сетевых серверов. Stdio-серверы
//! получают это окружение и `env` из файла. Значения секретов (а у сетевых серверов и все значения переменных,
//! подставленные в `url` и `headers`) заменяются на `***` в сводке, результатах вызовов, ошибках и stderr серверов,
//! который идёт в журнал; так же маскируются значения заголовков из `headersHelper`. Не запустившийся или не
//! ответивший сервер в сводку не попадает, остальные работают; причина неудачи сетевого сервера (в том числе 401 и
//! 403 с `WWW-Authenticate`) идёт в журнал и в ответ на вызов этого сервера. Вызов, на который сервер не ответил за
//! `CALL_TIMEOUT`, завершается ошибкой, сервер остаётся. Конец назначения останавливает серверы и отменяет вызовы в
//! полёте.

mod client;
mod config;
mod events;
mod headers;
mod http;
mod sse;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nerve::connect::server::ask;
use nerve::mux::Mux;
use nerve::protocol::{
    self, Assignment, Error, MAX_MCP_CATALOG, MAX_MCP_RESULT, McpCall, McpCatalog, McpError, McpResponse, RunnerRequest,
};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

use super::exec::{Executor, Masker, Resolved, Secrets};
use client::Client;
use config::{Launch, Remote, Spec};
use headers::{Headers, Helper};

#[cfg(test)]
#[path = "../../tests/runner/mcp.rs"]
mod tests;

/// Сколько сервер отвечает на вызов инструмента, ресурса или prompt: инструменты вроде сборки или поиска по сети
/// работают минуты, но вызов без ответа не должен держать агента бесконечно.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Подключённый сервер и секреты, которые не должны попасть в его ответы.
struct Server {
    client: Arc<Client>,
    secrets: Secrets,
}

impl Server {
    /// Секреты записи и значения, которые сервер получил от `headersHelper`.
    fn secrets(&self) -> Secrets {
        self.secrets.with(self.client.learned())
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct State {
    /// Назначение идёт: серверы можно запускать. После `stop` запоздавший запуск убивает свой сервер сам.
    running: bool,
    /// Все запущенные процессы, в том числе ещё не ответившие на `initialize`.
    spawned: Vec<Arc<Server>>,
    /// Ответившие серверы по имени из `.mcp.json`.
    ready: HashMap<String, Arc<Server>>,
    /// Почему сетевой сервер не подключился или не ответил (текст замаскирован): вызов сообщает это агенту.
    failed: HashMap<String, String>,
    starting: Option<JoinHandle<()>>,
}

/// MCP-серверы текущего назначения.
#[derive(Debug, Default)]
pub(super) struct Mcp {
    state: Mutex<State>,
    /// Сводка для server; `None` — сообщать нечего.
    catalog: watch::Sender<Option<McpCatalog>>,
}

impl Mcp {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Начало назначения: серверы запускаются в фоне, сводка уходит server, когда все ответили или не ответили.
    /// Прежнее назначение к этому времени остановлено (`stop`).
    pub(super) fn start(self: &Arc<Self>, assignment: Assignment, executor: &Arc<Executor>) {
        {
            let mut state = self.state();
            state.running = true;
            state.failed.clear();
        }
        let (mcp, executor) = (Arc::clone(self), Arc::clone(executor));
        let task = tokio::spawn(async move { mcp.launch(assignment, &executor).await });
        self.state().starting = Some(task);
    }

    /// Конец назначения: запуск прерван, серверы остановлены (SIGTERM, через 2 с SIGKILL), вызовы в полёте получили ошибку.
    pub(super) async fn stop(&self) {
        let (starting, servers) = {
            let mut state = self.state();
            state.running = false;
            state.ready.clear();
            state.failed.clear();
            (state.starting.take(), std::mem::take(&mut state.spawned))
        };
        if let Some(starting) = starting {
            starting.abort();
            // Прерванная задача отдаёт ошибку отмены: итога у неё нет.
            starting.await.unwrap_or_default();
        }
        self.catalog.send_replace(None);
        let mut stopping = JoinSet::new();
        for server in servers {
            stopping.spawn(async move { server.client.shutdown().await });
        }
        while stopping.join_next().await.is_some() {}
    }

    async fn launch(self: Arc<Self>, assignment: Assignment, executor: &Arc<Executor>) {
        let dir = match executor.project_dir() {
            Ok(dir) => dir,
            Err(e) => {
                tracing::warn!(error = ?e, "рабочий каталог недоступен, MCP-серверы не запущены");
                return;
            }
        };
        let mut starting = JoinSet::new();
        for (index, (name, launch)) in config::read(&dir).await.into_iter().enumerate() {
            let (mcp, executor, dir) = (Arc::clone(&self), Arc::clone(executor), dir.clone());
            starting.spawn(async move { (index, mcp.start_server(assignment, &executor, &dir, name, launch).await) });
        }
        let mut started = Vec::new();
        while let Some(done) = starting.join_next().await {
            match done {
                Ok((index, Some(server))) => started.push((index, server)),
                Ok((_, None)) => {}
                Err(e) => tracing::error!(error = %e, "запуск MCP-сервера прервался"),
            }
        }
        started.sort_by_key(|(index, _)| *index);
        let mut catalog = McpCatalog::default();
        for (_, server) in started {
            catalog.servers.push(server);
            if protocol::encode(&catalog).is_ok_and(|body| body.len() <= MAX_MCP_CATALOG) {
                continue;
            }
            if let Some(dropped) = catalog.servers.pop() {
                tracing::warn!(server = %dropped.name, "сводка MCP-сервера не помещается в предел сводки, сервер отброшен");
                self.discard(&dropped.name).await;
            }
        }
        tracing::info!(servers = catalog.servers.len(), "MCP-серверы проекта запущены");
        if self.state().running {
            self.catalog.send_replace(Some(catalog));
        }
    }

    /// Убирает сервер из числа доступных для вызовов и останавливает его.
    async fn discard(&self, name: &str) {
        let server = {
            let mut state = self.state();
            let server = state.ready.remove(name);
            if let Some(server) = &server {
                state.spawned.retain(|spawned| !Arc::ptr_eq(spawned, server));
            }
            server
        };
        if let Some(server) = server {
            server.client.shutdown().await;
        }
    }

    /// Запускает или подключает один сервер и опрашивает его; `None` — не получилось, причина в журнале.
    async fn start_server(
        &self,
        assignment: Assignment,
        executor: &Executor,
        dir: &Path,
        name: String,
        spec: Spec,
    ) -> Option<protocol::McpServer> {
        let session = match executor.process_env(assignment, BTreeMap::new()).await {
            Ok(session) => session,
            Err(e) => {
                tracing::warn!(server = %name, error = ?e, "окружение MCP-сервера не собрано");
                return None;
            }
        };
        let root = root_uri(dir);
        let (client, secrets) = match spec {
            Spec::Stdio(launch) => start_stdio(assignment, executor, dir, &name, launch, &session.vars, root).await?,
            Spec::Http(remote) => self.connect_remote(&name, Kind::Http, remote, &session, dir, root).await?,
            Spec::Sse(remote) => self.connect_remote(&name, Kind::Sse, remote, &session, dir, root).await?,
        };
        let server = Arc::new(Server { client, secrets });
        let registered = {
            let mut state = self.state();
            if state.running {
                state.spawned.push(Arc::clone(&server));
            }
            state.running
        };
        if !registered {
            server.client.shutdown().await;
            return None;
        }
        match server.client.describe(&name).await {
            Ok(mut described) => {
                mask_server(&server.secrets, &mut described);
                let mut state = self.state();
                if state.running {
                    state.ready.insert(name, server);
                    return Some(described);
                }
                None
            }
            Err(reason) => {
                let reason = mask_text(&server.secrets(), &reason);
                tracing::warn!(server = %name, %reason, "MCP-сервер не ответил, в сводку не попал");
                self.fail(&name, reason);
                self.state().spawned.retain(|spawned| !Arc::ptr_eq(spawned, &server));
                server.client.shutdown().await;
                None
            }
        }
    }

    fn fail(&self, name: &str, reason: String) {
        let mut state = self.state();
        if state.running {
            state.failed.insert(name.to_owned(), reason);
        }
    }

    /// Подключается к сетевому серверу. Секреты — секреты сессии и значения переменных, подставленные в `url` и
    /// `headers`; значения от `headersHelper` добавляет сам клиент. Причина неудачи уходит в журнал и в `failed`.
    async fn connect_remote(
        &self,
        name: &str,
        kind: Kind,
        remote: Remote,
        session: &Resolved,
        dir: &Path,
        root: String,
    ) -> Option<(Arc<Client>, Secrets)> {
        let remote = match remote.expand(&session.vars) {
            Ok(remote) => remote,
            Err(reason) => {
                tracing::warn!(server = %name, %reason, "запись MCP-сервера не раскрыта, сервер не подключён");
                return None;
            }
        };
        let secrets = session
            .secrets
            .with(remote.substituted.iter().map(|value| value.as_bytes().to_vec()));
        let helper = remote.headers_helper.map(|command| Helper {
            name: name.to_owned(),
            command,
            url: remote.url.clone(),
            dir: dir.to_owned(),
            vars: session.vars.clone(),
        });
        let connected = match Headers::new(&remote.headers, helper) {
            Ok(headers) => match kind {
                Kind::Http => Client::http(name, &remote.url, headers, root).await,
                Kind::Sse => Client::sse(name, &remote.url, headers, root).await,
            },
            Err(reason) => Err(reason),
        };
        match connected {
            Ok(client) => Some((client, secrets)),
            Err(reason) => {
                let reason = mask_text(&secrets, &reason);
                tracing::warn!(server = %name, %reason, "MCP-сервер не подключён");
                self.fail(name, reason);
                None
            }
        }
    }

    /// Отправляет server сводку: при каждом изменении и при каждом новом канале управления.
    pub(super) async fn follow(&self, mux: &Mux) {
        let mut catalog = self.catalog.subscribe();
        loop {
            let current = catalog.borrow_and_update().clone();
            if let Some(current) = current {
                match ask::<_, Result<(), Error>>(mux, &RunnerRequest::Mcp(current)).await {
                    Ok(Ok(())) => tracing::info!("сводка MCP-серверов отправлена server"),
                    Ok(Err(e)) => tracing::warn!(error = ?e, "server не принял сводку MCP-серверов"),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "сводка MCP-серверов не отправлена");
                        if mux.is_closed() {
                            return;
                        }
                    }
                }
            }
            if catalog.changed().await.is_err() {
                return;
            }
        }
    }

    /// Выполняет вызов у сервера из сводки. Значения секретов в ответе, как и в тексте ошибки, заменяются на `***`.
    pub(super) async fn call(&self, call: McpCall) -> McpResponse {
        let (name, method, params) = match call {
            McpCall::Tool { server, tool, arguments } => (server, "tools/call", json!({"name": tool, "arguments": object(&arguments)?})),
            McpCall::ReadResource { server, uri } => (server, "resources/read", json!({"uri": uri})),
            McpCall::GetPrompt { server, name, arguments } => {
                (server, "prompts/get", json!({"name": name, "arguments": object(&arguments)?}))
            }
        };
        let (server, failure) = {
            let state = self.state();
            (state.ready.get(&name).cloned(), state.failed.get(&name).cloned())
        };
        let Some(server) = server else {
            return Err(McpError(match failure {
                Some(reason) => format!("MCP-сервер {name} не запущен: {reason}"),
                None => format!("MCP-сервер {name} не запущен"),
            }));
        };
        let secrets = server.secrets();
        let answer = timeout(CALL_TIMEOUT, server.client.request(method, params))
            .await
            .map_err(|_| McpError(format!("сервер не ответил за {} с", CALL_TIMEOUT.as_secs())))?;
        let mut result = answer.map_err(|reason| McpError(mask_text(&secrets, &reason)))?;
        mask_value(&secrets, &mut result);
        let result = result.to_string();
        if result.len() > MAX_MCP_RESULT {
            return Err(McpError(format!("результат больше предела {MAX_MCP_RESULT} байт")));
        }
        Ok(result)
    }
}

/// Запускает stdio-сервер с окружением сессии и `env` записи.
async fn start_stdio(
    assignment: Assignment,
    executor: &Executor,
    dir: &Path,
    name: &str,
    launch: Launch,
    session_vars: &BTreeMap<OsString, OsString>,
    root: String,
) -> Option<(Arc<Client>, Secrets)> {
    let launch = match launch.expand(session_vars) {
        Ok(launch) => launch,
        Err(reason) => {
            tracing::warn!(server = %name, %reason, "запись MCP-сервера не раскрыта, сервер не запущен");
            return None;
        }
    };
    let env = match executor.process_env(assignment, launch.env).await {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(server = %name, error = ?e, "окружение MCP-сервера не собрано");
            return None;
        }
    };
    let cwd = launch.cwd.map_or_else(|| dir.to_owned(), |cwd| dir.join(cwd));
    match Client::spawn(name, &launch.command, &launch.args, &env.vars, &cwd, env.secrets.clone(), root) {
        Ok(client) => Some((client, env.secrets)),
        Err(reason) => {
            tracing::warn!(server = %name, %reason, "MCP-сервер не запущен");
            None
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Http,
    Sse,
}

/// Адрес каталога проекта для ответов на `roots/list`.
fn root_uri(dir: &Path) -> String {
    reqwest::Url::from_file_path(dir).map_or_else(|()| format!("file://{}", dir.display()), String::from)
}

/// Аргументы вызова: JSON-объект; пустая строка — пустой объект.
fn object(arguments: &str) -> Result<Value, McpError> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(_) => Err(McpError("аргументы вызова — не JSON-объект".to_owned())),
        Err(e) => Err(McpError(format!("аргументы вызова не разобраны: {e}"))),
    }
}

/// Сводка проходит ту же маскировку, что результаты вызовов.
fn mask_server(secrets: &Secrets, server: &mut protocol::McpServer) {
    let mask = |text: &mut String| *text = mask_text(secrets, text);
    server.instructions.iter_mut().for_each(mask);
    for tool in &mut server.tools {
        mask(&mut tool.name);
        mask(&mut tool.description);
        // Схема — JSON в строке: секрет с кавычкой записан в ней с экранированием.
        tool.input_schema = match serde_json::from_str::<Value>(&tool.input_schema) {
            Ok(mut schema) => {
                mask_value(secrets, &mut schema);
                schema.to_string()
            }
            Err(_) => mask_text(secrets, &tool.input_schema),
        };
    }
    for resource in &mut server.resources {
        mask(&mut resource.name);
        mask(&mut resource.uri);
        mask(&mut resource.description);
        mask(&mut resource.mime_type);
    }
    for prompt in &mut server.prompts {
        mask(&mut prompt.name);
        mask(&mut prompt.description);
        for argument in &mut prompt.arguments {
            mask(&mut argument.name);
            mask(&mut argument.description);
        }
    }
}

fn mask_text(secrets: &Secrets, text: &str) -> String {
    let mut stream = Masker::new(secrets.clone());
    let mut out = stream.push(text.as_bytes());
    out.extend(stream.finish());
    String::from_utf8_lossy(&out).into_owned()
}

/// Маскирует каждую строку значения отдельно: секрет с кавычкой или переводом строки в JSON записан с экранированием
/// и в тексте целиком не найдётся.
fn mask_value(secrets: &Secrets, value: &mut Value) {
    match value {
        Value::String(text) => *text = mask_text(secrets, text),
        Value::Array(items) => items.iter_mut().for_each(|item| mask_value(secrets, item)),
        Value::Object(fields) => {
            let masked = std::mem::take(fields)
                .into_iter()
                .map(|(key, mut field)| {
                    mask_value(secrets, &mut field);
                    (mask_text(secrets, &key), field)
                })
                .collect();
            *fields = masked;
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
