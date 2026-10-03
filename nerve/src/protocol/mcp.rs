//! Проектные MCP-серверы runner: сводка для session и вызовы по каналу данных.
//!
//! Схемы и аргументы — JSON-строки: postcard не переносит `serde_json::Value`, а содержимое для протокола
//! непрозрачно.

use serde::{Deserialize, Serialize};

/// Наибольшая сводка в байтах postcard: сводка целиком идёт одним сообщением и целиком попадает в правила агента.
/// Получатель отвергает большую.
pub const MAX_MCP_CATALOG: usize = 1 << 20;

/// Наибольший результат вызова в байтах: он целиком попадает в контекст агента, где больший не поместится.
/// Получатель отвергает больший, отправитель обрезать не вправе.
pub const MAX_MCP_RESULT: usize = 16 << 20;

/// Сводка MCP-серверов проекта, запущенных runner. Каждая сводка заменяет прежнюю целиком; пустая снимает её.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCatalog {
    pub servers: Vec<McpServer>,
}

/// Один MCP-сервер проекта: то, что он назвал в `initialize`, `tools/list`, `resources/list`, `prompts/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Имя из `.mcp.json`; агент видит его префиксом имён.
    pub name: String,
    pub instructions: Option<String>,
    pub tools: Vec<McpTool>,
    pub resources: Vec<McpResource>,
    pub prompts: Vec<McpPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// JSON-схема входа, строкой.
    pub input_schema: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Вызов MCP-сервера runner, `server` — имя из сводки. Ответ — [`McpResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpCall {
    /// `tools/call`; `arguments` — JSON-объект строкой.
    Tool { server: String, tool: String, arguments: String },
    /// `resources/read`.
    ReadResource { server: String, uri: String },
    /// `prompts/get`; `arguments` — JSON-объект строкой.
    GetPrompt { server: String, name: String, arguments: String },
}

/// Ответ сервера на вызов: JSON-строка `result` как есть либо текст ошибки. Не больше [`MAX_MCP_RESULT`].
pub type McpResponse = Result<String, McpError>;

/// Вызов не удался: сервера нет в сводке, он отказал или завершился.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpError(pub String);
