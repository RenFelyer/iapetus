//! Общие помощники тестов waldo: временный каталог.

use std::path::Path;

use tempfile::TempDir;

/// Временный каталог вне `/tmp`, в `target` сборки: рабочий каталог runner и клона не может лежать на путях
/// контейнера session, и `/tmp` — один из них.
pub(crate) fn workdir() -> TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/workdirs");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}
