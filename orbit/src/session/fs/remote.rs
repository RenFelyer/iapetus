//! Связь с runner: файловые запросы по каналу данных.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use nerve::mux::Closed;
use nerve::protocol::{self, DataRequest, FsRequest, FsResponse, Root};

use crate::session::data::Link;

/// Запрос не дошёл до runner или ответ нельзя разобрать. Отказ самого runner — это [`FsResponse`], а не ошибка связи.
#[derive(Debug)]
pub(super) enum RemoteError {
    Timeout,
    /// runner не подключён.
    Disconnected,
    /// Канал оборвался, закрыт или говорит на другой версии протокола.
    Channel(Closed),
    Malformed,
}

impl RemoteError {
    /// Запрос мог дойти до runner и выполниться, а ответ потерян: канал оборвался или runner замолчал после отправки.
    pub(super) fn reply_may_be_lost(&self) -> bool {
        matches!(self, Self::Timeout | Self::Channel(_))
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("runner не ответил вовремя"),
            Self::Disconnected => f.write_str("runner не подключён"),
            Self::Channel(closed) => write!(f, "{closed}"),
            Self::Malformed => f.write_str("ответ runner не разобран"),
        }
    }
}

/// Скорость, при которой передача файла ещё не считается зависшей: на каждые такие байты запрос получает
/// ещё секунду к таймауту. Без этого файл в гигабайт не уложился бы в таймаут одного запроса.
const MIN_THROUGHPUT: u64 = 4 << 20;

/// Файловые запросы к runner.
#[derive(Debug, Clone)]
pub(super) struct Remote {
    link: Link,
    timeout: Duration,
    /// Каталог runner, к которому относятся пути запросов.
    root: Root,
}

impl Remote {
    pub(super) fn new(link: Link, timeout: Duration, root: Root) -> Self {
        Self { link, timeout, root }
    }

    /// Ждёт открытого канала не дольше таймаута запроса: текущего, если он жив, иначе переподключения runner.
    /// `false` — канал не вернулся.
    pub(super) async fn wait_open(&self) -> bool {
        let mut link = self.link.clone();
        matches!(tokio::time::timeout(self.timeout, link.open()).await, Ok(Some(_)))
    }

    pub(super) async fn call(&self, request: FsRequest) -> Result<FsResponse, RemoteError> {
        self.call_expecting(request, 0).await
    }

    /// Запрос, на который ждут ответ около `reply_bytes` байт: таймаут растёт вместе с передаваемым.
    pub(super) async fn call_expecting(&self, request: FsRequest, reply_bytes: u64) -> Result<FsResponse, RemoteError> {
        let mux = self.link.current().ok_or(RemoteError::Disconnected)?;
        let body = protocol::encode(&DataRequest::Fs { root: self.root, request }).map_err(|_| RemoteError::Malformed)?;
        let transfer = (body.len() as u64).saturating_add(reply_bytes) / MIN_THROUGHPUT;
        let timeout = self.timeout.saturating_add(Duration::from_secs(transfer));
        let reply = tokio::time::timeout(timeout, mux.request(Bytes::from(body)))
            .await
            .map_err(|_| RemoteError::Timeout)?
            .map_err(RemoteError::Channel)?;
        protocol::decode_body(&reply).map_err(|_| RemoteError::Malformed)
    }
}
