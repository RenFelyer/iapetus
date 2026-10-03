//! Сведения о Codex CLI, общие для server, session и worker.

/// Идентификатор: строка реестра и метки Docker.
pub(super) const ID: &str = "codex";

/// Название для показа человеку.
pub(super) const NAME: &str = "Codex";

/// Каталог состояния агента в `AGENT_ROOT`.
pub(super) const STATE_DIR: &str = ".codex";
