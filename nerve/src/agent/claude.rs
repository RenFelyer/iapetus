//! Сведения о Claude Code, общие для server, session и worker.

/// Идентификатор: строка реестра и метки Docker.
pub(super) const ID: &str = "claude";

/// Название для показа человеку.
pub(super) const NAME: &str = "Claude Code";

/// Каталог состояния агента в `AGENT_ROOT`.
pub(super) const STATE_DIR: &str = ".claude";
