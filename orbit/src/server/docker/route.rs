//! Разбор метода, пути и запроса: что клиент хочет сделать, в каноничном виде.
//!
//! Путь сравнивается побуквенно, без декодирования: процент, пустой сегмент, другой регистр и
//! лишний слэш не совпадают ни с одной операцией. Параметры запроса из списка, значения
//! только из `[A-Za-z0-9._-]`; наружу параметры уходят пересобранными, а не пересланными.

use hyper::Method;

use super::{Denied, is_name};

const MIN_API_MINOR: u32 = 41;
const MAX_API_MINOR: u32 = 60;
const MAX_STOP_SECONDS: u32 = 600;

/// Операция над контейнером сессии по имени или id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContainerOp {
    Inspect,
    Start,
    Stop,
    Remove,
    Logs,
    Wait,
}

impl ContainerOp {
    pub(super) fn method(self) -> Method {
        match self {
            Self::Inspect | Self::Logs => Method::GET,
            Self::Start | Self::Stop | Self::Wait => Method::POST,
            Self::Remove => Method::DELETE,
        }
    }

    pub(super) fn suffix(self) -> &'static str {
        match self {
            Self::Inspect => "/json",
            Self::Start => "/start",
            Self::Stop => "/stop",
            Self::Remove => "",
            Self::Logs => "/logs",
            Self::Wait => "/wait",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    Ping,
    ContainerCreate {
        name: String,
    },
    ContainerList {
        all: bool,
    },
    Container {
        id: String,
        op: ContainerOp,
        query: Vec<(&'static str, String)>,
    },
    VolumeCreate,
    VolumeInspect {
        name: String,
    },
    VolumeRemove {
        name: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Routed {
    /// `/v1.NN` или пусто, если клиент не указал версию.
    pub(super) prefix: String,
    pub(super) route: Route,
}

pub(super) fn parse(method: &Method, path: &str, query: Option<&str>) -> Result<Routed, Denied> {
    let rest = path.strip_prefix('/').ok_or(Denied::Endpoint)?;
    let mut segments: Vec<&str> = rest.split('/').collect();
    let mut prefix = String::new();
    if let Some(minor) = segments[0].strip_prefix("v1.") {
        let minor: u32 = minor.parse().map_err(|_| Denied::Version)?;
        if !(MIN_API_MINOR..=MAX_API_MINOR).contains(&minor) || minor.to_string() != segments[0][3..] {
            return Err(Denied::Version);
        }
        prefix = format!("/v1.{minor}");
        segments.remove(0);
    }
    let route = match (method, segments.as_slice()) {
        (&Method::GET, ["_ping"]) => {
            Query::parse(query, &[])?;
            Route::Ping
        }
        (&Method::POST, ["containers", "create"]) => {
            let query = Query::parse(query, &["name"])?;
            let name = query.get("name").ok_or_else(|| Denied::Query("name is required".to_owned()))?;
            if !is_name(name) {
                return Err(Denied::Name);
            }
            Route::ContainerCreate { name: name.to_owned() }
        }
        (&Method::GET, ["containers", "json"]) => {
            let all = Query::parse(query, &["all"])?.bool("all")?.unwrap_or(false);
            Route::ContainerList { all }
        }
        (method, ["containers", id, rest @ ..]) => container(method, id, rest, query)?,
        (&Method::POST, ["volumes", "create"]) => {
            Query::parse(query, &[])?;
            Route::VolumeCreate
        }
        (&Method::GET, ["volumes", name]) => {
            Query::parse(query, &[])?;
            Route::VolumeInspect { name: volume_name(name)? }
        }
        (&Method::DELETE, ["volumes", name]) => {
            Query::parse(query, &[])?;
            Route::VolumeRemove { name: volume_name(name)? }
        }
        _ => return Err(Denied::Endpoint),
    };
    Ok(Routed { prefix, route })
}

fn volume_name(name: &str) -> Result<String, Denied> {
    if is_name(name) { Ok(name.to_owned()) } else { Err(Denied::Name) }
}

fn container(method: &Method, id: &str, rest: &[&str], query: Option<&str>) -> Result<Route, Denied> {
    let op = match (method, rest) {
        (&Method::GET, ["json"]) => ContainerOp::Inspect,
        (&Method::POST, ["start"]) => ContainerOp::Start,
        (&Method::POST, ["stop"]) => ContainerOp::Stop,
        (&Method::DELETE, []) => ContainerOp::Remove,
        (&Method::GET, ["logs"]) => ContainerOp::Logs,
        (&Method::POST, ["wait"]) => ContainerOp::Wait,
        _ => return Err(Denied::Endpoint),
    };
    let mut canonical: Vec<(&'static str, String)> = Vec::new();
    match op {
        ContainerOp::Inspect | ContainerOp::Start => {
            Query::parse(query, &[])?;
        }
        ContainerOp::Stop => {
            let query = Query::parse(query, &["t"])?;
            if let Some(seconds) = query.uint("t", MAX_STOP_SECONDS)? {
                canonical.push(("t", seconds.to_string()));
            }
        }
        ContainerOp::Remove => {
            let query = Query::parse(query, &["force", "v"])?;
            for key in ["force", "v"] {
                if let Some(flag) = query.bool(key)? {
                    canonical.push((key, flag.to_string()));
                }
            }
        }
        ContainerOp::Logs => {
            let query = Query::parse(query, &["follow", "stdout", "stderr", "timestamps", "tail"])?;
            for key in ["follow", "stdout", "stderr", "timestamps"] {
                if let Some(flag) = query.bool(key)? {
                    canonical.push((key, flag.to_string()));
                }
            }
            match query.get("tail") {
                None => {}
                Some("all") => canonical.push(("tail", "all".to_owned())),
                Some(_) => canonical.push(("tail", query.uint("tail", 100_000)?.unwrap_or(0).to_string())),
            }
        }
        ContainerOp::Wait => {
            let query = Query::parse(query, &["condition"])?;
            match query.get("condition") {
                None => {}
                Some(condition @ ("not-running" | "next-exit" | "removed")) => {
                    canonical.push(("condition", condition.to_owned()));
                }
                Some(_) => return Err(Denied::Query("condition".to_owned())),
            }
        }
    }
    if !is_name(id) {
        return Err(Denied::Name);
    }
    Ok(Route::Container {
        id: id.to_owned(),
        op,
        query: canonical,
    })
}

/// `?k=v&k2=v2` из каноничных пар или пустая строка.
pub(super) fn render_query(pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = pairs.iter().map(|(key, value)| format!("{key}={value}")).collect();
    format!("?{}", joined.join("&"))
}

/// Параметры запроса: только разрешённые ключи, без повторов, значения из безопасных символов.
struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    fn parse(query: Option<&'a str>, allowed: &[&str]) -> Result<Self, Denied> {
        let Some(query) = query else {
            return Ok(Self(Vec::new()));
        };
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or_else(|| Denied::Query("pair without `=`".to_owned()))?;
            if !allowed.contains(&key) {
                return Err(Denied::Query(format!("parameter `{}` is not allowed", key.escape_default())));
            }
            let safe = !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !safe {
                return Err(Denied::Query(format!("value of `{key}` has unsafe characters")));
            }
            if pairs.iter().any(|(known, _)| *known == key) {
                return Err(Denied::Query(format!("parameter `{key}` is repeated")));
            }
            pairs.push((key, value));
        }
        Ok(Self(pairs))
    }

    fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(known, _)| *known == key).map(|(_, value)| *value)
    }

    fn bool(&self, key: &str) -> Result<Option<bool>, Denied> {
        match self.get(key) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(_) => Err(Denied::Query(format!("`{key}` must be true or false"))),
        }
    }

    fn uint(&self, key: &str, max: u32) -> Result<Option<u32>, Denied> {
        let Some(value) = self.get(key) else {
            return Ok(None);
        };
        match value.parse::<u32>() {
            Ok(number) if number <= max && number.to_string() == value => Ok(Some(number)),
            _ => Err(Denied::Query(format!("`{key}` must be a number up to {max}"))),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/server/docker/route.rs"]
mod tests;
