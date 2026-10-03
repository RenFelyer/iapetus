//! Файл настроек worker: строки `ключ = значение` (`orbit_url`, `orbit_ca`, `orbit_key`, `name`), `#` — комментарий.

#[cfg(test)]
#[path = "../../tests/worker/settings.rs"]
mod tests;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nerve::connect::OrbitKey;
use nerve::protocol::{Author, SessionIdentity};

/// Настройки worker. Секрет — `orbit_key`, поэтому файл с ним доступен только владельцу.
#[derive(Debug)]
pub(super) struct Settings {
    /// Адрес SSH orbit server, `хост:порт`.
    pub(super) orbit_url: String,
    /// Открытый ключ CA server (OpenSSH): им подписан хост-сертификат server.
    pub(super) orbit_ca: String,
    pub(super) orbit_key: OrbitKey,
    /// Имя входа в сессии; без него — имя локального пользователя.
    pub(super) name: Option<String>,
}

/// Файл по умолчанию: `$XDG_CONFIG_HOME/iapetus/worker.conf` или `~/.config/iapetus/worker.conf`.
pub(super) fn default_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("iapetus/worker.conf"));
    }
    let home = std::env::var_os("HOME").context("HOME не задан: укажите файл настроек через --settings")?;
    Ok(PathBuf::from(home).join(".config/iapetus/worker.conf"))
}

/// Читает настройки. Права проверяются по открытому файлу, а не по пути: подмена между проверкой и чтением невозможна.
pub(super) fn load(path: &Path) -> Result<Settings> {
    let file = File::open(path).with_context(|| format!("файл настроек {} не открыт", path.display()))?;
    let mode = file.metadata()?.permissions().mode() & 0o777;
    if mode & !0o600 != 0 {
        bail!(
            "файл настроек {} доступен не только владельцу (права {mode:04o}): выставьте 0600, в нём orbit_key",
            path.display()
        );
    }
    let text = std::io::read_to_string(file).with_context(|| format!("файл настроек {} не прочитан", path.display()))?;
    parse(&text).with_context(|| format!("файл настроек {}", path.display()))
}

fn parse(text: &str) -> Result<Settings> {
    let mut orbit_url = None;
    let mut orbit_ca = None;
    let mut orbit_key = None;
    let mut name = None;
    for (number, line) in text.lines().enumerate().map(|(index, line)| (index + 1, line.trim())) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("строка {number}: ожидалось `ключ = значение`"))?;
        let slot = match key.trim() {
            "orbit_url" => &mut orbit_url,
            "orbit_ca" => &mut orbit_ca,
            "orbit_key" => &mut orbit_key,
            "name" => &mut name,
            other => bail!("строка {number}: неизвестный ключ {other}"),
        };
        if slot.replace(value.trim().to_owned()).is_some() {
            bail!("строка {number}: ключ {} повторён", key.trim());
        }
    }
    Ok(Settings {
        orbit_url: orbit_url.context("нет orbit_url")?,
        orbit_ca: orbit_ca.context("нет orbit_ca")?,
        orbit_key: OrbitKey::new(orbit_key.context("нет orbit_key")?),
        name: name.filter(|name| !name.is_empty()),
    })
}

/// Автор коммитов и `GITHUB_TOKEN` из `.env` worker. Нет файла или нет нужных переменных — поле не задано.
/// Автор задан, только когда есть и `GIT_AUTHOR_NAME`, и `GIT_AUTHOR_EMAIL`.
pub(super) fn session_identity(env_file: &Path) -> Result<SessionIdentity> {
    let text = match std::fs::read_to_string(env_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(SessionIdentity::default()),
        Err(error) => return Err(error).with_context(|| format!("файл .env {} не прочитан", env_file.display())),
    };
    let mut vars = nerve::dotenv::parse_dotenv(&text);
    let mut take = |name: &str| vars.remove(name).filter(|value| !value.is_empty());
    let author = match (take("GIT_AUTHOR_NAME"), take("GIT_AUTHOR_EMAIL")) {
        (Some(name), Some(email)) => Some(Author { name, email }),
        _ => None,
    };
    Ok(SessionIdentity {
        author,
        github_token: take("GITHUB_TOKEN"),
    })
}
