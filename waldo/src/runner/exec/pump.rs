//! Вывод команды в поток событий: куски с пометкой потока, маскирование и heartbeat.

use std::time::Duration;

use nerve::protocol::{EXEC_HEARTBEAT, ExecEvent, Stream};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};

use super::mask::{Masker, Secrets};

/// Размер одного чтения: с запасом меньше предела сообщения.
const CHUNK: usize = 32 * 1024;

/// Сколько вывода ещё читать после выхода команды. Фоновый процесс, унаследовавший трубу, держит её открытой
/// и после команды: без предела поток ответов не закончился бы никогда.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Получатель событий исчез: слать вывод некому.
#[derive(Debug)]
pub(super) struct SinkClosed;

struct Output<'a> {
    events: &'a mpsc::Sender<ExecEvent>,
    /// Когда слать следующий heartbeat: через интервал после любого отправленного сообщения.
    next_heartbeat: Instant,
}

impl Output<'_> {
    async fn send(&mut self, event: ExecEvent) -> Result<(), SinkClosed> {
        self.events.send(event).await.map_err(|_| SinkClosed)?;
        self.next_heartbeat = Instant::now() + EXEC_HEARTBEAT;
        Ok(())
    }

    /// Отправляет кусок вывода, если в нём что-то осталось после маскирования.
    async fn chunk(&mut self, stream: Stream, data: Vec<u8>) -> Result<(), SinkClosed> {
        if data.is_empty() {
            return Ok(());
        }
        self.send(ExecEvent::Output { stream, data }).await
    }
}

/// Читает stdout и stderr до конца обоих и шлёт их в `events` кусками с пометкой потока. Потоки не склеиваются.
///
/// `Heartbeat` уходит, когда `EXEC_HEARTBEAT` не было ни вывода, ни другого сообщения. `exited` срабатывает, когда
/// команда завершилась: вывод дочитывается ещё не дольше [`DRAIN_GRACE`], а дальше труба считается брошенной.
pub(super) async fn pump(
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    secrets: Secrets,
    events: &mpsc::Sender<ExecEvent>,
    mut exited: oneshot::Receiver<()>,
) -> Result<(), SinkClosed> {
    let mut out = Output {
        events,
        next_heartbeat: Instant::now() + EXEC_HEARTBEAT,
    };
    let (mut stdout_mask, mut stderr_mask) = (Masker::new(secrets.clone()), Masker::new(secrets));
    let (mut stdout_buf, mut stderr_buf) = (vec![0; CHUNK], vec![0; CHUNK]);
    let (mut stdout_open, mut stderr_open) = (true, true);
    let mut exit_seen = false;
    let mut drain_until: Option<Instant> = None;

    while stdout_open || stderr_open {
        tokio::select! {
            read = stdout.read(&mut stdout_buf), if stdout_open => {
                // Ошибка чтения трубы — конец потока, как и EOF.
                let count = read.unwrap_or(0);
                if count == 0 {
                    stdout_open = false;
                } else {
                    out.chunk(Stream::Stdout, stdout_mask.push(&stdout_buf[..count])).await?;
                }
            }
            read = stderr.read(&mut stderr_buf), if stderr_open => {
                let count = read.unwrap_or(0);
                if count == 0 {
                    stderr_open = false;
                } else {
                    out.chunk(Stream::Stderr, stderr_mask.push(&stderr_buf[..count])).await?;
                }
            }
            () = sleep_until(out.next_heartbeat) => out.send(ExecEvent::Heartbeat).await?,
            _ = &mut exited, if !exit_seen => {
                exit_seen = true;
                drain_until = Some(Instant::now() + DRAIN_GRACE);
            }
            () = sleep_until(drain_until.unwrap_or_else(Instant::now)), if drain_until.is_some() => break,
        }
    }
    // Удержанный хвост уже не станет секретом.
    out.chunk(Stream::Stdout, stdout_mask.finish()).await?;
    out.chunk(Stream::Stderr, stderr_mask.finish()).await
}

#[cfg(test)]
#[path = "../../../tests/runner/exec/pump.rs"]
mod tests;
