//! Протокол между orbit и waldo: сообщения по каналам (`control`, `exec`, `data`, `fs`) и их кодирование.
//!
//! Тело сообщения — postcard, его несёт [`crate::mux`]. Он не переносит изменений схемы и порядка
//! вариантов enum, поэтому любое такое изменение требует нового [`VERSION`].

mod control;
mod data;
mod exec;
mod fs;
mod host;
mod mcp;

#[cfg(test)]
#[path = "../tests/protocol.rs"]
mod tests;

use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use crate::agent::Agent;
pub use control::{
    Activity, AgentRelease, AgentRun, Artifact, ArtifactFormat, AssignedRunner, Assignment, Author, Cause, CloneReply, CloneRequest,
    CloneResponse, Control, ControlResponse, Digest, DocsEvent, Document, EnrollRequest, EnvChange, EnvChangeResponse, EnvEntry, EnvEvent,
    EnvOp, NodeInfo, NodesOp, Platform, RENEW_NAMESPACE, RegisterReply, RegisterRequest, RegisterResponse, RenewReply, RenewRequest,
    RenewResponse, RunnerInfo, RunnerPresence, RunnerRequest, RunnersOp, SessionEvent, SessionIdentity, SessionInfo, SessionMessage,
    SessionRequest, SessionResponse, SessionsOp, Status, ToRunner, UsageWindow, WorkerCertificateReply, WorkerCertificateRequest,
    WorkerCertificateResponse, WorkerCloneRequest, WorkerRequest,
};
pub use data::{Change, ChangeKind, DataRequest, Root, WatchBatch};
pub use exec::{AttachRequest, CancelRequest, CancelResponse, ExecEvent, ExecExit, ExecRequest, ExitStatus, Stream};
pub use fs::{Attr, Content, DirEntry, Expect, FsReply, FsRequest, FsResponse, Kind};
pub use host::{MAX_OS_RELEASE, RunnerHost, check_project};
pub use mcp::{
    MAX_MCP_CATALOG, MAX_MCP_RESULT, McpCall, McpCatalog, McpError, McpPrompt, McpPromptArgument, McpResource, McpResponse, McpServer,
    McpTool,
};

/// Версия протокола. Получатель с другой версией не разбирает тело.
pub const VERSION: u16 = 23;

/// Наибольший файл, который сессия и runner передают друг другу: файл идёт одним сообщением канала данных, и
/// предел ограничивает память получателя.
pub const MAX_FILE: usize = 1 << 30;

/// Как часто runner шлёт `Heartbeat`, пока команда молчит. Сессия считает runner пропавшим, если не получила ни
/// вывода, ни `Heartbeat` за два интервала.
pub const EXEC_HEARTBEAT: Duration = Duration::from_secs(30);

/// Ошибка в теле ответа: запрос понят, но не выполнен.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Error {
    /// Путь выходит за рабочий каталог: `..`, абсолютный путь или ссылка наружу.
    OutsideWorkdir,
    /// Нет файла или каталога.
    NotFound,
    /// Содержимое на runner не совпало с ожидаемым, или путь уже занят.
    Conflict,
    /// Отказ в доступе.
    Access,
    /// Прочая ошибка ввода-вывода с текстом.
    Io(String),
    /// Каталог не пуст.
    NotEmpty,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkdir => f.write_str("путь вне рабочего каталога"),
            Self::NotFound => f.write_str("нет такого файла или каталога"),
            Self::Conflict => f.write_str("конфликт: содержимое на runner изменилось"),
            Self::Access => f.write_str("отказано в доступе"),
            Self::NotEmpty => f.write_str("каталог не пуст"),
            Self::Io(text) => write!(f, "ошибка ввода-вывода: {text}"),
        }
    }
}

impl std::error::Error for Error {}

/// Тело сообщения не удалось разобрать.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Тело не соответствует схеме или за сообщением остались лишние байты.
    Malformed,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("тело сообщения не разобрано")
    }
}

impl std::error::Error for DecodeError {}

/// Кодирует тело сообщения.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_stdvec(message)
}

/// Разбирает тело сообщения; версию сверил первый кадр соединения.
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, DecodeError> {
    let (message, rest) = postcard::take_from_bytes(body).map_err(|_| DecodeError::Malformed)?;
    if !rest.is_empty() {
        return Err(DecodeError::Malformed);
    }
    Ok(message)
}
