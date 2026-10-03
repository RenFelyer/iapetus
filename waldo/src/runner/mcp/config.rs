//! `.mcp.json` рабочего каталога: список серверов stdio, http и sse.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// Как запустить один stdio-сервер.
#[derive(Debug, Deserialize)]
pub(super) struct Launch {
    pub(super) command: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    /// Переменные сервера: перекрывают окружение команд сессии.
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    /// Рабочий каталог сервера; относительный отсчитывается от рабочего каталога runner.
    pub(super) cwd: Option<String>,
}

impl Launch {
    /// Подставляет `${VAR}` и `${VAR:-значение}` в `command`, `args`, значения `env` и `cwd`. Значения берутся из `vars`;
    /// неизвестная переменная без значения по умолчанию — ошибка с её именем.
    pub(super) fn expand(self, vars: &BTreeMap<OsString, OsString>) -> Result<Self, String> {
        Ok(Self {
            command: expand(&self.command, vars, &mut Vec::new())?,
            args: self
                .args
                .iter()
                .map(|arg| expand(arg, vars, &mut Vec::new()))
                .collect::<Result<_, _>>()?,
            env: self
                .env
                .into_iter()
                .map(|(name, value)| Ok((name, expand(&value, vars, &mut Vec::new())?)))
                .collect::<Result<_, String>>()?,
            cwd: self.cwd.as_deref().map(|cwd| expand(cwd, vars, &mut Vec::new())).transpose()?,
        })
    }
}

/// Как подключиться к серверу по сети: `type` `http` (streamable HTTP) или `sse`.
#[derive(Debug, Deserialize)]
pub(super) struct Remote {
    pub(super) url: String,
    #[serde(default)]
    pub(super) headers: BTreeMap<String, String>,
    /// Команда, печатающая JSON с заголовками; `${VAR}` в ней не подставляется, переменные раскрывает её оболочка.
    #[serde(default, rename = "headersHelper")]
    pub(super) headers_helper: Option<String>,
    /// Значения переменных, подставленные в `url` и `headers` при `expand`: это секреты.
    #[serde(skip)]
    pub(super) substituted: Vec<String>,
}

impl Remote {
    /// Подставляет `${VAR}` и `${VAR:-значение}` в `url` и значения `headers` и запоминает подставленные значения
    /// переменных (не значения по умолчанию).
    pub(super) fn expand(self, vars: &BTreeMap<OsString, OsString>) -> Result<Self, String> {
        let mut substituted = Vec::new();
        Ok(Self {
            url: expand(&self.url, vars, &mut substituted)?,
            headers: self
                .headers
                .into_iter()
                .map(|(name, value)| Ok((name, expand(&value, vars, &mut substituted)?)))
                .collect::<Result<_, String>>()?,
            headers_helper: self.headers_helper,
            substituted,
        })
    }
}

/// Запись `.mcp.json` по виду сервера.
#[derive(Debug)]
pub(super) enum Spec {
    Stdio(Launch),
    Http(Remote),
    Sse(Remote),
}

/// `${VAR}` — значение переменной; `${VAR:-значение}` — значение переменной, а если она не задана или пуста, то
/// `значение`. Остальной текст, в том числе `$VAR` и `${` без закрывающей скобки, остаётся как есть. Подставленные
/// значения переменных дописываются в `used`.
fn expand(text: &str, vars: &BTreeMap<OsString, OsString>, used: &mut Vec<String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let inner = &rest[start + 2..];
        let Some(end) = inner.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let (name, default) = match inner[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&inner[..end], None),
        };
        let value = vars.get(std::ffi::OsStr::new(name)).map(|value| value.to_string_lossy());
        match (value, default) {
            (Some(value), _) if !value.is_empty() || default.is_none() => {
                out.push_str(&value);
                used.push(value.into_owned());
            }
            (_, Some(default)) => out.push_str(default),
            _ => return Err(format!("переменная {name} не задана и без значения по умолчанию")),
        }
        rest = &inner[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct File {
    #[serde(rename = "mcpServers", default)]
    servers: Map<String, Value>,
}

/// Запись по `type`: `stdio` или без него — процесс, `http` и `sse` — сеть. `None` — вид не поддерживается.
fn parse(kind: Option<&str>, value: Value) -> Option<Result<Spec, serde_json::Error>> {
    Some(match kind {
        None | Some("stdio") => serde_json::from_value(value).map(Spec::Stdio),
        Some("http") => serde_json::from_value(value).map(Spec::Http),
        Some("sse") => serde_json::from_value(value).map(Spec::Sse),
        Some(_) => return None,
    })
}

/// Серверы из `.mcp.json` в порядке файла. Нет файла — нет серверов; нечитаемый файл, нечитаемая запись (например, без
/// `command` или `url`) и запись неизвестного `type` пропускаются с записью в журнал.
pub(super) async fn read(dir: &Path) -> Vec<(String, Spec)> {
    let path = dir.join(".mcp.json");
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, ".mcp.json не прочитан");
            return Vec::new();
        }
    };
    let file: File = match serde_json::from_slice(&bytes) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, ".mcp.json не разобран");
            return Vec::new();
        }
    };
    let mut specs = Vec::new();
    for (name, value) in file.servers {
        let kind = value.get("type").and_then(Value::as_str).map(str::to_owned);
        match parse(kind.as_deref(), value) {
            Some(Ok(spec)) => specs.push((name, spec)),
            Some(Err(e)) => tracing::warn!(server = %name, error = %e, "запись MCP-сервера не разобрана, пропущена"),
            None => {
                tracing::warn!(server = %name, kind = kind.as_deref().unwrap_or_default(), "тип MCP-сервера не поддерживается, пропущен");
            }
        }
    }
    specs
}
