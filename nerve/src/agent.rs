//! Агент в коробке: идентификатор, название и каталог состояния. Знание о конкретном агенте лежит в его файле.

mod antigravity;
mod claude;
mod codex;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Корень состояния агента в контейнере session: каталоги `state_dir()` всех агентов лежат в нём.
pub const AGENT_ROOT: &str = "/home/agent";

/// Uid пользователя агента в образе session.
pub const AGENT_UID: u32 = 1000;

/// Gid пользователя агента в образе session.
pub const AGENT_GID: u32 = 1000;

/// Агент в коробке. Порядок вариантов — часть протокола: `postcard` пишет индекс.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Antigravity,
}

/// Строка не называет агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownAgent(String);

impl fmt::Display for UnknownAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "неизвестный агент {:?}", self.0)
    }
}

impl std::error::Error for UnknownAgent {}

impl Agent {
    pub const ALL: [Self; 3] = [Self::ClaudeCode, Self::Codex, Self::Antigravity];

    /// Идентификатор в реестре и метках Docker.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => claude::ID,
            Self::Codex => codex::ID,
            Self::Antigravity => antigravity::ID,
        }
    }

    /// Название для показа.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => claude::NAME,
            Self::Codex => codex::NAME,
            Self::Antigravity => antigravity::NAME,
        }
    }

    /// Каталог состояния агента относительно `AGENT_ROOT`.
    #[must_use]
    pub const fn state_dir(self) -> &'static str {
        match self {
            Self::ClaudeCode => claude::STATE_DIR,
            Self::Codex => codex::STATE_DIR,
            Self::Antigravity => antigravity::STATE_DIR,
        }
    }
}

impl FromStr for Agent {
    type Err = UnknownAgent;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|agent| agent.id() == text)
            .ok_or_else(|| UnknownAgent(text.to_owned()))
    }
}

#[cfg(test)]
#[path = "../tests/agent.rs"]
mod tests;
