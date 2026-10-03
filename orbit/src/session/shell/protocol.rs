//! Канал между оболочкой и агентом по unix-сокету: сообщения postcard, каждое с
//! префиксом длины. Канал внутренний для контейнера сессии.

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Предел одного сообщения. Сокет открыт процессам пользователя агента, и
/// без предела длина в префиксе заставила бы агента выделить любую память.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Сообщение оболочки агенту.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ClientMessage {
    /// Выполнить команду; первое сообщение соединения.
    Run {
        command: String,
        /// Текущий каталог процесса оболочки.
        cwd: String,
    },
    /// Остановить команду: оболочка получила SIGINT или SIGTERM.
    Cancel,
}

/// Как завершилась команда.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ExitStatus {
    Code(i32),
    /// Команду убил сигнал с этим номером.
    Signal(i32),
}

/// Сообщение агента оболочке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reply {
    /// Кусок вывода: stdout и stderr команды вместе, как их пишет файл задачи
    /// агента.
    Output(Vec<u8>),
    /// Команда завершилась; последнее сообщение.
    Exit {
        status: ExitStatus,
        /// Текущий каталог после команды (`pwd -P`).
        cwd: String,
    },
    /// Команду не удалось выполнить; последнее сообщение.
    Failed(String),
}

pub(super) async fn write_frame<W, T>(writer: &mut W, message: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(message).map_err(io::Error::other)?;
    let length = u32::try_from(body.len())
        .ok()
        .filter(|&length| length <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "сообщение больше предела"))?;
    // Префикс и тело уходят одной записью, чтобы читатель не увидел половину.
    let mut frame = length.to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Читает одно сообщение. Оборванное соединение — `UnexpectedEof`, мусор и
/// слишком длинный префикс — `InvalidData`.
pub(super) async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0; 4];
    reader.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix);
    if length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "длина сообщения больше предела"));
    }
    let mut body = vec![0; length as usize];
    reader.read_exact(&mut body).await?;
    postcard::from_bytes(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
#[path = "../../../tests/session/shell/protocol.rs"]
mod tests;
