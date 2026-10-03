//! Описания, которые server отдаёт в списках worker и session: runner, сессии, узлы.

use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::fingerprint::Fingerprint;
use crate::session_state::SessionState;

use super::session::{Cause, UsageWindow};

/// Состояние runner для списка и выбора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerPresence {
    /// 🔓 В сети, сессии нет.
    Free,
    /// 🔒 В сети, с ним работает сессия.
    Busy,
    /// 📴 Канал управления закрыт; назначение при этом сохраняется.
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub presence: RunnerPresence,
    /// Клон, созданный от другого runner.
    pub temporary: bool,
    /// У клона — runner, от которого он создан.
    pub parent: Option<Fingerprint>,
}

/// Узел в списке управления: runner с ОС и пометкой отзыва.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub runner: RunnerInfo,
    pub os: String,
    pub revoked: bool,
}

/// Сессия в списке worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub fingerprint: Fingerprint,
    pub agent: Agent,
    pub state: SessionState,
    /// Почему такое состояние: последнее сообщение session; `None` — она не в сети или причины нет.
    pub reason: Option<String>,
    /// Вид причины: нужен вход, лимит и прочее; `None` — причины нет.
    pub cause: Option<Cause>,
    /// Когда сбросится исчерпанный лимит подписки, секунды Unix.
    pub limit_reset: Option<u64>,
    pub runner: Option<Fingerprint>,
    /// Агент запущен в pty: сессия в сети и её состояние — `success` или `warning`.
    pub agent_running: bool,
    /// Окна лимитов подписки; пусто — сервис их не сообщил.
    pub limits: Vec<UsageWindow>,
    /// Перезапуск агента затребован и ждёт конца основного хода.
    pub restart_pending: bool,
}
