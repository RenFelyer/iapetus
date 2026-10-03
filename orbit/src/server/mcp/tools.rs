//! Четыре инструмента MCP. Сессия берётся из токена запроса, а не из аргументов: агент не может назвать чужую.

use std::sync::Arc;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::Error;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use super::Caller;
use crate::server::control::Control;

#[derive(Clone)]
pub(super) struct Tools {
    control: Arc<Control>,
    tool_router: ToolRouter<Self>,
}

impl Tools {
    pub(super) fn new(control: Arc<Control>) -> Self {
        Self {
            control,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RunnerUseParams {
    /// Имя или fingerprint свободного runner из `runner_list`.
    runner: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RunnerCloneParams {
    /// Абсолютный путь каталога на текущем runner, в котором будет работать клон.
    dir: String,
}

#[derive(Debug, Serialize)]
struct Used<'a> {
    runner: &'a Fingerprint,
}

/// Вызывающая сессия из запроса; без неё (запрос не прошёл проверку токена) — отказ.
fn caller(parts: &axum::http::request::Parts) -> Result<Fingerprint, String> {
    parts
        .extensions
        .get::<Caller>()
        .map(|caller| caller.0.clone())
        .ok_or_else(|| "запрос без токена MCP".to_owned())
}

fn json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

/// Ошибка server для агента: подробности реестра остаются в журнале server.
fn failure(error: &Error) -> String {
    match error {
        Error::NotFound => "не найдено".to_owned(),
        Error::Conflict => "runner занят или назначение конфликтует".to_owned(),
        Error::Access => "отказано в доступе".to_owned(),
        Error::Io(text) => text.clone(),
        other => other.to_string(),
    }
}

#[tool_router]
impl Tools {
    #[tool(
        description = "Список runner: имя, fingerprint, состояние (free — свободен, busy — занят сессией, offline — не в сети) и временный ли (клон)"
    )]
    async fn runner_list(&self, Extension(parts): Extension<axum::http::request::Parts>) -> Result<String, String> {
        caller(&parts)?;
        json(&self.control.runner_list().map_err(|error| failure(&error))?)
    }

    #[tool(description = "Переключить эту сессию на свободный runner: имя или fingerprint из runner_list")]
    async fn runner_use(
        &self,
        Extension(parts): Extension<axum::http::request::Parts>,
        Parameters(RunnerUseParams { runner }): Parameters<RunnerUseParams>,
    ) -> Result<String, String> {
        let session = caller(&parts)?;
        let runners = self.control.runner_list().map_err(|error| failure(&error))?;
        let chosen = runners
            .iter()
            .find(|info| info.fingerprint.as_str() == runner || info.name == runner)
            .ok_or_else(|| format!("runner {runner} не найден"))?;
        self.control
            .reassign(&session, &chosen.fingerprint, true)
            .await
            .map_err(|error| failure(&error))?;
        json(&Used {
            runner: &chosen.fingerprint,
        })
    }

    #[tool(description = "Создать временный клон текущего runner в указанном каталоге; возвращает имя и fingerprint клона")]
    async fn runner_clone(
        &self,
        Extension(parts): Extension<axum::http::request::Parts>,
        Parameters(RunnerCloneParams { dir }): Parameters<RunnerCloneParams>,
    ) -> Result<String, String> {
        let session = caller(&parts)?;
        let cloned = self
            .control
            .clone_current_runner(&session, dir)
            .await
            .map_err(|error| failure(&error))?;
        json(&cloned)
    }

    #[tool(description = "Состояние этой сессии: состояние, причина, runner, чем занят агент и лимиты подписки")]
    async fn session_status(&self, Extension(parts): Extension<axum::http::request::Parts>) -> Result<String, String> {
        let session = caller(&parts)?;
        json(&self.control.session_status(&session).map_err(|error| failure(&error))?)
    }
}

#[expect(clippy::unused_async_trait_impl, reason = "макрос tool_handler делает методы трейта асинхронными")]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}
