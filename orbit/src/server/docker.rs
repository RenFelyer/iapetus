//! Прокси orbit server к Docker API: слушает свой unix-сокет и пропускает к `docker.sock`
//! только то, что нужно для запуска и остановки контейнеров orbit session.
//!
//! `POST /containers/create` разбирается и сверяется со строгим шаблоном сессии; наружу уходит
//! тело, собранное заново из проверенных полей, а не пересланное клиентское. Контейнеры и тома
//! доступны только с меткой `iapetus.role=session`. Всё остальное, включая `exec`, `attach`,
//! `archive`, `build`, `commit` и тома с драйвером, — отказ 403.

mod client;
mod proxy;
mod route;
mod template;

pub(super) use client::Docker;
pub(super) use proxy::{bind, serve};
pub(super) use template::{Limits, SessionSpec, Template};

use std::fmt;

/// Причина отказа. Клиент получает её текст в поле `message` ответа 403.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Denied {
    /// Метод и путь не входят в список разрешённых операций.
    Endpoint,
    /// Версия API в пути вне поддерживаемого диапазона.
    Version,
    /// Параметр запроса не разрешён или его значение не в каноничном виде.
    Query(String),
    /// Имя или идентификатор контейнера, тома или сети в недопустимом виде.
    Name,
    /// У операции без тела оно есть, либо тело больше предела.
    Body,
    /// Тело не разобралось как JSON шаблона: лишнее, повторное или пропущенное поле.
    Malformed(String),
    /// Образ не из списка разрешённых digest.
    Image,
    /// Нет метки `iapetus.role=session` или есть чужая метка.
    Label,
    /// Переменная окружения вне разрешённого вида.
    Env(String),
    /// Включён `Privileged`.
    Privileged,
    /// Набор capabilities не равен шаблону.
    Capabilities,
    /// `Tmpfs` не равен паре `/work` и `/run/secrets` с параметрами шаблона.
    Tmpfs,
    /// `SecurityOpt` вне шаблона: нет `no-new-privileges` или есть чужая опция.
    SecurityOpt,
    /// Набор устройств не равен `/dev/fuse`.
    Devices,
    /// Непустой `Binds`.
    Binds,
    /// Монтирование не единственный том для каталога состояния агента.
    Mounts,
    /// Режим пространства имён не пустой (`host`, `container:…`).
    HostMode(&'static str),
    /// Сеть не равна сети сессий из шаблона.
    Network,
    /// Лимит не задан или вне допустимого диапазона.
    Limit(&'static str),
    /// Том создаётся с драйвером или его параметрами.
    VolumeDriver,
    /// Контейнер или том без метки сессии.
    NotSession,
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Endpoint => f.write_str("endpoint is not allowed"),
            Self::Version => f.write_str("API version is not supported"),
            Self::Query(why) => write!(f, "query is not allowed: {why}"),
            Self::Name => f.write_str("name or id is not allowed"),
            Self::Body => f.write_str("request body is not allowed"),
            Self::Malformed(why) => write!(f, "body does not match the session template: {why}"),
            Self::Image => f.write_str("image is not an allowed digest"),
            Self::Label => f.write_str("labels do not match the session template"),
            Self::Env(why) => write!(f, "environment is not allowed: {why}"),
            Self::Privileged => f.write_str("Privileged is not allowed"),
            Self::Capabilities => f.write_str("capabilities do not match the session template"),
            Self::Tmpfs => f.write_str("tmpfs mounts do not match the session template"),
            Self::SecurityOpt => f.write_str("security options do not match the session template"),
            Self::Devices => f.write_str("devices do not match the session template"),
            Self::Binds => f.write_str("Binds are not allowed"),
            Self::Mounts => f.write_str("mounts do not match the session template"),
            Self::HostMode(field) => write!(f, "{field} must be empty"),
            Self::Network => f.write_str("network does not match the session template"),
            Self::Limit(field) => write!(f, "{field} is missing or out of range"),
            Self::VolumeDriver => f.write_str("volume driver and its options are not allowed"),
            Self::NotSession => f.write_str("object is not labeled iapetus.role=session"),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/server/docker/fixtures.rs"]
mod fixtures;

/// Имя контейнера, тома или сети: буква или цифра, дальше буквы, цифры, `_`, `.`, `-`.
/// Процентов и слэшей нет, поэтому декодировать путь не нужно.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}
