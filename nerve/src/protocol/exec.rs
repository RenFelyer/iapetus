//! Команды канала данных: запрос, поток ответов, подключение к потоку и отмена.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::Error;

/// Команда сессии по каналу данных; в ответ идёт поток [`ExecEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    pub id: u64,
    pub command: String,
    pub cwd: String,
    pub timeout_ms: Option<u64>,
}

/// Как завершилась команда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
}

/// Завершение команды.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecExit {
    pub status: ExitStatus,
    /// Каталог после команды (`pwd -P`).
    pub cwd: String,
}

/// Поток вывода команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Сообщение потока ответов на команду.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecEvent {
    /// Кусок вывода с пометкой потока; оболочка сессии пишет оба потока в один файл.
    Output { stream: Stream, data: Vec<u8> },
    /// Команда жива, но молчит: без него поток ответов оборвался бы по порогу простоя.
    Heartbeat,
    /// Последнее сообщение потока.
    Exit(ExecExit),
    /// Команда не запущена; последнее сообщение потока.
    Err(Error),
}

impl fmt::Debug for ExecEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output { stream, data } => write!(f, "Output({stream:?}, {} байт)", data.len()),
            Self::Heartbeat => f.write_str("Heartbeat"),
            Self::Exit(exit) => f.debug_tuple("Exit").field(exit).finish(),
            Self::Err(error) => f.debug_tuple("Err").field(error).finish(),
        }
    }
}

/// Забрать остаток потока ответов команды, которая шла по оборвавшемуся каналу. Команда обрыв переживает: она
/// дорабатывает на runner, а вывод и итог ждут возвращения сессии. Заново команда не запускается никогда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    pub id: u64,
    /// Сколько сообщений потока сессия уже получила: runner продолжает со следующего. Потерянные при обрыве сообщения
    /// сессия за полученные не считает.
    pub received: u64,
}

/// Остановить дерево процессов команды сессии; идёт по каналу данных.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    pub id: u64,
}

pub type CancelResponse = Result<(), Error>;
