use serde::{Deserialize, Serialize};

/// Состояние orbit session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Работает штатно.
    Success,
    /// Ждёт авторизации своего агента.
    Waiting,
    /// Есть ошибки, работа приостановлена.
    Warning,
    /// Агент упал без возможности подняться, нужен человек.
    Failure,
}
