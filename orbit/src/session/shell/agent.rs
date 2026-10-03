//! Сторона агента: сервер unix-сокета, на который приходят команды оболочки.
//!
//! Агент не выполняет команды. Каждую он передаёт получателю канала `mpsc` как
//! [`ShellTask`] и пересылает оболочке то, что получатель вернул.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use super::protocol::{ClientMessage, Reply, read_frame, write_frame};
use crate::session::launch::AsOwner;

/// Сколько ответов буферизует канал между исполнителем и оболочкой.
const REPLY_BUFFER: usize = 16;

/// Владелец сокета: пользователь, от которого работает агент.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// Команда оболочки, которую должен выполнить получатель канала.
#[derive(Debug)]
pub(crate) struct ShellTask {
    /// Номер команды, уникальный в процессе агента.
    pub id: u64,
    pub command: String,
    /// Текущий каталог процесса оболочки.
    pub cwd: String,
    /// Сюда уходит вывод, а последним сообщением — `Exit` или `Failed`.
    /// Закрытый канал без них оболочка считает обрывом и завершается с ошибкой.
    pub reply: mpsc::Sender<Reply>,
    /// Срабатывает, когда команду надо остановить: оболочка прислала отмену
    /// (значение) или пропала (обрыв отправителя).
    pub cancel: oneshot::Receiver<()>,
}

/// Занимает сокет по пути с правами `0600` и владельцем `owner`.
///
/// Каталог сокета создаётся, если его нет, но писать в него должен владелец. Остаток прошлого запуска на этом пути
/// удаляется. Сокет доступен только владельцу: остальным процессам контейнера подключаться нельзя.
pub(crate) fn bind(path: &Path, owner: Owner) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _as_owner = AsOwner::enter(owner.uid, owner.gid)?;
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Принимает подключения оболочек и передаёт их команды в `tasks`.
pub(crate) async fn serve(listener: UnixListener, tasks: mpsc::Sender<ShellTask>) -> io::Result<()> {
    let mut next_id = 0_u64;
    loop {
        let (stream, _) = listener.accept().await?;
        next_id += 1;
        let tasks = tasks.clone();
        let id = next_id;
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, id, tasks).await {
                tracing::warn!(id, %error, "соединение оболочки завершилось ошибкой");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, id: u64, tasks: mpsc::Sender<ShellTask>) -> io::Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let ClientMessage::Run { command, cwd } = read_frame(&mut reader).await? else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "первое сообщение — не Run"));
    };
    let (reply, mut replies) = mpsc::channel(REPLY_BUFFER);
    let (cancel_sender, cancel) = oneshot::channel();
    let task = ShellTask {
        id,
        command,
        cwd,
        reply,
        cancel,
    };
    if tasks.send(task).await.is_err() {
        let failed = Reply::Failed("получатель команд остановлен".to_owned());
        return write_frame(&mut writer, &failed).await;
    }
    // Чтение идёт отдельной задачей: `read_frame` нельзя прерывать посреди
    // сообщения, а ответы исполнителя приходят в это же время.
    let watcher = tokio::spawn(watch_client(reader, cancel_sender));
    let forwarded = forward_replies(&mut writer, &mut replies).await;
    watcher.abort();
    forwarded
}

/// Пересылает ответы исполнителя оболочке до `Exit` или `Failed`.
async fn forward_replies(writer: &mut tokio::net::unix::OwnedWriteHalf, replies: &mut mpsc::Receiver<Reply>) -> io::Result<()> {
    while let Some(reply) = replies.recv().await {
        write_frame(writer, &reply).await?;
        if matches!(reply, Reply::Exit { .. } | Reply::Failed(_)) {
            return Ok(());
        }
    }
    let failed = Reply::Failed("исполнитель команды оборвался без кода выхода".to_owned());
    write_frame(writer, &failed).await
}

/// Ждёт от оболочки отмену или обрыв соединения. И то и другое будит
/// `cancel` команды: оболочка, которой нет, ждать результата не станет.
async fn watch_client(mut reader: OwnedReadHalf, cancel: oneshot::Sender<()>) {
    let mut cancel = Some(cancel);
    while let Ok(ClientMessage::Cancel) = read_frame(&mut reader).await {
        if let Some(cancel) = cancel.take() {
            // Получатель мог уйти: команда уже кончилась.
            cancel.send(()).unwrap_or_default();
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/session/shell/agent.rs"]
mod tests;
