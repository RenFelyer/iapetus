//! Запросы канала данных: сессия → runner.

use serde::{Deserialize, Serialize};

use super::{AttachRequest, Attr, CancelRequest, ExecRequest, FsRequest, McpCall};

/// Каталог runner, к которому относятся пути файловых запросов и изменений.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Root {
    /// Рабочий каталог проекта.
    Project,
    /// Домашний каталог пользователя runner.
    Home,
}

/// Запрос сессии к runner. Ответ зависит от варианта: [`FsResponse`](super::FsResponse) на `Fs`, поток
/// [`ExecEvent`](super::ExecEvent) на `Exec` и `Attach` (последнее сообщение — `Exit` или `Err`),
/// [`CancelResponse`](super::CancelResponse) на `Cancel`, [`McpResponse`](super::McpResponse) на `Mcp`.
///
/// В обратную сторону идут только [`WatchBatch`] без ответа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataRequest {
    Fs { root: Root, request: FsRequest },
    Exec(ExecRequest),
    Cancel(CancelRequest),
    Attach(AttachRequest),
    Mcp(McpCall),
}

/// Что случилось с путём.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// Путь появился или изменился; новые атрибуты. Для каталога содержимое не передаётся.
    Updated(Attr),
    /// Пути больше нет. Для каталога это и всё, что под ним: кэш сессии удаляет поддерево.
    Removed,
    /// Каталог больше не наблюдается: лимит inotify, переполнение очереди, символическая ссылка, смена каталога или
    /// ошибка доступа. Кэш его записей перестаёт быть достоверным, пока сессия не прочтёт каталог заново (`ReadDir`):
    /// он снова встаёт под наблюдение.
    Unwatched,
}

/// Изменение одного пути относительно корня пачки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

/// Пачка изменений в прочитанных каталогах, которую runner шлёт сессии по каналу данных. Изменения применяются по
/// порядку: за `Removed` каталога может идти `Updated` того же пути, если на его месте уже другой каталог.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchBatch {
    /// Каталог, относительно которого записаны пути изменений.
    pub root: Root,
    pub changes: Vec<Change>,
}
