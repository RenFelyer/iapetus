//! Указания server waldo runner: клон и границы сессии.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::enroll::RenewRequest;
use crate::fingerprint::Fingerprint;
use crate::protocol::{Error, McpCatalog, RunnerHost};

/// Запросы runner к server по каналу управления (вход `iapetus-control`). Ответ — [`RenewResponse`] на `Renew`,
/// поток `EnvEvent` на `Env`: снимок, затем изменения до закрытия соединения, `Result<(), Error>` на `Mcp` и `Host`.
///
/// [`RenewResponse`]: super::RenewResponse
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerRequest {
    Renew(RenewRequest),
    Env,
    /// Сводка проектных MCP-серверов, пока runner назначен session; пустая снимает прежнюю.
    Mcp(McpCatalog),
    /// Окружение runner для агента session; первый запрос соединения: до него server runner в сети не считает.
    Host(RunnerHost),
}

/// Запросы server к runner: `Clone` — ответ [`CloneResponse`], `Session` — [`SessionResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToRunner {
    Clone(CloneRequest),
    Session(SessionMessage),
}

/// Создать временный клон в каталоге той же машины.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneRequest {
    /// Абсолютный путь каталога, в котором клон работает.
    pub dir: String,
}

/// Созданный клон.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneReply {
    pub fingerprint: Fingerprint,
    pub name: String,
}

pub type CloneResponse = Result<CloneReply, Error>;

/// Автор коммитов на время сессии.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// Номер назначения: server выдаёт его каждой новой паре session и runner, растёт без повторов. Отличает пары
/// друг от друга: сообщение о конце другой пары не касается идущей.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Assignment(pub u64);

/// Начало и конец пары session и runner. `Start` с тем же номером, что у идущей
/// пары, ничего не меняет; с другим — кончает прежнюю пару и начинает новую. `End` с чужим номером игнорируется.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMessage {
    Start {
        assignment: Assignment,
        author: Author,
        github_token: Option<String>,
    },
    End {
        assignment: Assignment,
    },
}

impl fmt::Debug for SessionMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start {
                assignment,
                author,
                github_token,
            } => f
                .debug_struct("Start")
                .field("assignment", assignment)
                .field("author", author)
                .field("github_token", &github_token.as_ref().map(|_| "<скрыт>"))
                .finish(),
            Self::End { assignment } => f.debug_struct("End").field("assignment", assignment).finish(),
        }
    }
}

pub type SessionResponse = Result<(), Error>;
