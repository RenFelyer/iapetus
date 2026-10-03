//! Окружение runner, которое session воспроизводит для агента, и проверка пути проекта.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// Наибольший размер `/etc/os-release`, который принимают server и session.
pub const MAX_OS_RELEASE: usize = 16 * 1024;

/// Каким runner виден агенту: путь проекта, имя машины, `/etc/os-release` и домашний каталог.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerHost {
    /// Абсолютный канонический путь рабочего каталога: FUSE монтируется в session по тому же пути.
    pub project: String,
    pub hostname: String,
    /// Содержимое `/etc/os-release`; пусто — файл у runner не прочитан.
    pub os_release: String,
    /// Канонический путь `$HOME` runner; пусто — не задан или не существует. Если путь не разрешается, здесь значение
    /// `$HOME` как есть.
    pub home: String,
    /// Значение `$HOME`, как оно записано у runner, если отличается от канонического `home` (ссылка `/home` →
    /// `/var/home`); пусто — совпадает.
    pub home_origin: String,
    /// Runner отдаёт `$HOME` файловыми запросами `Root::Home`.
    pub home_served: bool,
}

/// Пути контейнера session. `*` в конце имени — любое имя с этим началом: `/lib`, `/lib64`, `/libx32`.
const CONTAINER_PATHS: &[&str] = &[
    "/bin", "/sbin", "/lib*", "/usr", "/etc", "/opt", "/dev", "/proc", "/sys", "/run", "/tmp", "/var", "/work",
];

/// Отказ, если `dir` — не абсолютный нормализованный путь (без `.`, `..`, `//` и завершающего `/`), совпадает с путём
/// контейнера session или каталогом состояния агентов `state_root`, лежит внутри него или содержит его. Корень `/`
/// содержит все пути контейнера.
///
/// # Errors
///
/// Причина отказа текстом.
pub fn check_project(dir: &Path, state_root: &Path) -> Result<(), String> {
    if !dir.is_absolute() {
        return Err(format!("{} — не абсолютный путь", dir.display()));
    }
    let mut normal = String::new();
    for part in dir.components().skip(1) {
        match part.as_os_str().to_str() {
            Some(name) if matches!(part, Component::Normal(_)) => {
                normal.push('/');
                normal.push_str(name);
            }
            _ => return Err(format!("{} — не нормализованный путь", dir.display())),
        }
    }
    if dir.to_str() != Some(if normal.is_empty() { "/" } else { &normal }) {
        return Err(format!("{} — не нормализованный путь", dir.display()));
    }
    let state = state_root.to_string_lossy();
    let patterns: Vec<&str> = CONTAINER_PATHS.iter().copied().chain([state.as_ref()]).collect();
    for pattern in &patterns {
        if let Some(relation) = relation(dir, pattern) {
            return Err(format!(
                "{} {relation} {pattern} — пути контейнера session: FUSE монтирует рабочий каталог в контейнер по тому же \
                 пути, выберите каталог вне {}",
                dir.display(),
                patterns.join(", ")
            ));
        }
    }
    Ok(())
}

/// Как `dir` пересекается с путём контейнера `pattern`; `None` — не пересекается. Сравниваются имена целиком:
/// `/workspace` не лежит в `/work`.
fn relation(dir: &Path, pattern: &str) -> Option<&'static str> {
    let mut names = dir.components().skip_while(|c| *c == Component::RootDir);
    for part in pattern.split('/').filter(|part| !part.is_empty()) {
        let Some(Component::Normal(name)) = names.next() else {
            return Some("содержит");
        };
        let name = name.as_encoded_bytes();
        let same = match part.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix.as_bytes()),
            None => name == part.as_bytes(),
        };
        if !same {
            return None;
        }
    }
    Some(if names.next().is_some() {
        "лежит внутри"
    } else {
        "совпадает с"
    })
}

#[cfg(test)]
#[path = "../../tests/protocol/host.rs"]
mod tests;
