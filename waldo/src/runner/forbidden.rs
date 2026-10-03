//! Пути контейнера orbit session, с которыми не может пересекаться рабочий каталог runner и клона.
//!
//! FUSE монтирует рабочий каталог в контейнере session по тому же абсолютному пути; перечень путей и проверка общие
//! с session.

use std::path::Path;

use nerve::agent::AGENT_ROOT;

/// Отказ, если канонический каталог `dir` пересекается с путём контейнера session.
pub(super) fn check(dir: &Path) -> Result<(), String> {
    nerve::protocol::check_project(dir, Path::new(AGENT_ROOT))
}
