//! Запросы waldo worker к server.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::runner::{Author, CloneRequest};
use crate::agent::Agent;
use crate::fingerprint::Fingerprint;
use crate::protocol::Error;

/// Запросы worker к server: сертификат и операции TUI. Ответы — по вариантам `Op`: `Sessions(List)` —
/// `Result<Vec<SessionInfo>, Error>`, `Sessions(Start)` — поток [`SessionEvent`] (последний — конец сессии или отказ),
/// `Runners(List)` — `Result<Vec<RunnerInfo>, Error>`, `Runners(Clone)` — [`CloneResponse`](super::CloneResponse),
/// `Nodes(List)` — `Result<Vec<NodeInfo>, Error>`, `Nodes(NewOrbitKey)` — `Result<String, Error>` (секрет), `Env(List)` —
/// `Result<Vec<EnvEntry>, Error>`, остальные — `Result<(), Error>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerRequest {
    WorkerCertificate(WorkerCertificateRequest),
    Sessions(SessionsOp),
    Runners(RunnersOp),
    Nodes(NodesOp),
    Env(EnvOp),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionsOp {
    List,
    /// Поднять сессию: ответ — поток [`SessionEvent`].
    Start {
        agent: Agent,
        runner: Fingerprint,
        identity: SessionIdentity,
    },
    End(Fingerprint),
    /// Поднять сессию из `failure` новым контейнером на том же томе.
    Restart(Fingerprint),
    /// Перезапустить работающего агента с продолжением разговора. Первый запрос ждёт конца основного хода, повторный,
    /// пока перезапуск ещё ожидает, перезапускает сразу.
    RestartAgent(Fingerprint),
}

/// Что worker сообщает о пользователе сессии: автор коммитов и `GITHUB_TOKEN`. Оба берутся из его `.env`; пустое
/// значение — «не задано».
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub author: Option<Author>,
    pub github_token: Option<String>,
}

impl fmt::Debug for SessionIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionIdentity")
            .field("author", &self.author)
            .field("github_token", &self.github_token.as_ref().map(|_| "<скрыт>"))
            .finish()
    }
}

/// Ответы на `Sessions(Start)`: агент запущен и можно входить, затем — сессия завершена. `Refused` — единственный
/// ответ, если сессию поднять не удалось: текст причины для пользователя.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    Ready(Fingerprint),
    Refused(String),
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnersOp {
    List,
    /// Сменить runner сессии (или назначить заново тот же, если агент остановлен); `resume` — продолжить разговор.
    Assign {
        session: Fingerprint,
        runner: Fingerprint,
        resume: bool,
    },
    Rename {
        runner: Fingerprint,
        name: String,
    },
    Clone(WorkerCloneRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodesOp {
    List,
    Revoke(Fingerprint),
    NewOrbitKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvOp {
    List,
    Change(EnvChange),
}

/// Запись окружения: ключ `all.VAR` или `<fingerprint>.VAR` и значение (секрет — в `Debug` не показывается).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
}

impl fmt::Debug for EnvEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvEntry").field("key", &self.key).finish_non_exhaustive()
    }
}

/// Ответы на подписку runner `Env`: снимок положенных ему слоёв, затем изменения до закрытия соединения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvEvent {
    Snapshot(Vec<EnvEntry>),
    Change(EnvChange),
}

/// Запрос waldo worker: сертификат SSH для входа в сессию через jump, первый и продлённый.
/// Ключ создан на этот запуск worker и живёт только в его памяти; сертификат выдаётся по действующему `orbit_key`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCertificateRequest {
    /// Секрет действующего `orbit_key`.
    pub orbit_key: String,
    /// Открытый ключ Ed25519 в формате OpenSSH.
    pub public_key: String,
    /// Имя входа в сессию: единственный принципал сертификата.
    pub name: String,
}

impl fmt::Debug for WorkerCertificateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerCertificateRequest")
            .field("orbit_key", &"<скрыт>")
            .field("public_key", &self.public_key)
            .field("name", &self.name)
            .finish()
    }
}

/// Сертификат waldo worker в формате OpenSSH: короткий, с `permit-pty`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCertificateReply {
    pub certificate: String,
}

pub type WorkerCertificateResponse = Result<WorkerCertificateReply, Error>;

/// waldo worker меняет окружение через server. Ключ — `all.VAR` для всех runner
/// или `<fingerprint>.VAR` для одного. Значение может быть секретом, поэтому `Debug` его не показывает.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvChange {
    Set { key: String, value: String },
    Delete { key: String },
}

impl fmt::Debug for EnvChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set { key, .. } => f.debug_struct("Set").field("key", key).finish_non_exhaustive(),
            Self::Delete { key } => f.debug_struct("Delete").field("key", key).finish(),
        }
    }
}

pub type EnvChangeResponse = Result<(), Error>;

/// waldo worker просит server создать клон на runner; server шлёт запрос runner (`ToRunner::Clone`) и возвращает
/// его ответ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCloneRequest {
    pub runner: Fingerprint,
    pub request: CloneRequest,
}
