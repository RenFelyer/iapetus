//! Сведения об Antigravity CLI, общие для server, session и worker.

/// Идентификатор: строка реестра и метки Docker.
pub(super) const ID: &str = "antigravity";

/// Название для показа человеку.
pub(super) const NAME: &str = "Antigravity";

/// Каталог состояния агента в `AGENT_ROOT`.
pub(super) const STATE_DIR: &str = ".gemini";
