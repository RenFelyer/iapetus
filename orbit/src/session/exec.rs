//! Мост от подменённой оболочки к waldo runner.
//!
//! Команду, которую оболочка передала агенту ([`ShellTask`]), мост отправляет на runner по каналу данных и
//! транслирует поток ответов обратно: stdout и stderr — одним выходом, как их ждёт агент, завершение — кодом
//! и cwd. После каждой команды меняется эпоха FUSE: файлы на runner могли измениться.
//!
//! Обрыв канала команду не останавливает: она дорабатывает на runner, а мост после переподключения забирает остаток
//! её потока и итог (`Attach`). Заново команда не отправляется никогда. Пока канала нет, мост ждёт его возвращения:
//! тишина команду не отменяет, её конец — конец сессии. Но пока канал открыт, runner обязан подавать голос: за
//! [`SILENCE_LIMIT`] без вывода и heartbeat команда завершается ошибкой. Время без канала в срок не входит.
//!
//! [`relay`] работает с каналом готовых событий и от транспорта не зависит.

use bytes::Bytes;
use nerve::mux::Responses;
use nerve::protocol::{self, AttachRequest, CancelRequest, DataRequest, Error, ExecEvent, ExecRequest};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::data::Link;
use super::fs::Epoch;
use crate::session::shell::{ExitStatus, Reply, ShellTask};

/// Сколько событий ждёт оболочку: полный канал останавливает чтение ответов runner.
const EVENT_BUFFER: usize = 32;

/// Сколько runner может молчать во время команды: два интервала heartbeat по 30 с.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);

/// Событие от runner или текст ошибки канала.
type Wire = Result<ExecEvent, String>;

/// Отправляет команды оболочки на runner.
#[derive(Debug, Clone)]
pub(super) struct Bridge {
    link: Link,
    epoch: Epoch,
}

impl Bridge {
    pub(super) fn new(link: Link, epoch: Epoch) -> Self {
        Self { link, epoch }
    }

    /// Выполняет команды оболочки, пока канал открыт; каждая идёт своей задачей.
    pub(super) async fn serve(self, mut tasks: mpsc::Receiver<ShellTask>) {
        let bridge = Arc::new(self);
        while let Some(task) = tasks.recv().await {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move { bridge.execute(task).await });
        }
    }

    async fn execute(&self, task: ShellTask) {
        let ShellTask {
            id,
            command,
            cwd,
            reply,
            cancel,
        } = task;
        let request = ExecRequest {
            id,
            command,
            cwd,
            timeout_ms: None,
        };
        let (events, mut received) = mpsc::channel(EVENT_BUFFER);
        let (stop_runner, stop_requested) = oneshot::channel();
        let last = match self.send(&request, events, stop_requested).await {
            Ok(()) => relay(&mut received, &reply, cancel, stop_runner).await,
            Err(failure) => Reply::Failed(failure),
        };
        // Эпоха меняется до ответа: оболочка вернёт результат агенту, и его следующее обращение к файлу уже в
        // новой эпохе.
        self.epoch.advance();
        // Оболочки могло уже не быть: тогда отвечать некому.
        reply.send(last).await.unwrap_or_default();
    }

    /// Отправляет команду и запускает чтение ответов и отправку отмены.
    async fn send(&self, request: &ExecRequest, events: mpsc::Sender<Wire>, stop_requested: oneshot::Receiver<()>) -> Result<(), String> {
        let mux = self.link.current().ok_or_else(|| "runner не подключён".to_owned())?;
        let body = protocol::encode(&DataRequest::Exec(request.clone())).map_err(|e| format!("команда не закодирована: {e}"))?;
        let replies = mux
            .call(Bytes::from(body))
            .await
            .map_err(|e| format!("команда не отправлена: {e}"))?;
        let id = request.id;
        tokio::spawn(read_replies(self.link.clone(), id, replies, events));
        let mut link = self.link.clone();
        tokio::spawn(async move {
            // Отправитель закрыт без значения — команда закончилась сама, отменять нечего.
            if stop_requested.await.is_err() {
                return;
            }
            let Ok(body) = protocol::encode(&DataRequest::Cancel(CancelRequest { id })) else {
                return;
            };
            // Команда пережила обрыв канала, поэтому отмена идёт текущим соединением, а не тем, которым ушла команда,
            // и ждёт его, если канал сейчас оборван. В канале бывает только назначенный runner.
            let Some(mux) = link.open().await else {
                tracing::warn!(id, "отмена команды не отправлена: сессия кончилась");
                return;
            };
            if let Err(e) = mux.request(Bytes::from(body)).await {
                tracing::warn!(id, error = %e, "отмена команды не отправлена");
            }
        });
        Ok(())
    }
}

/// Читает ответы runner в канал событий до завершения команды. Оборвался канал — ждёт следующего и забирает остаток
/// потока с первого сообщения, которого не получил.
async fn read_replies(mut link: Link, id: u64, mut replies: Responses, events: mpsc::Sender<Wire>) {
    // Сколько сообщений потока получено: runner нумерует все, `Heartbeat` тоже.
    let mut received: u64 = 0;
    let mut reattached = false;
    loop {
        // Команда закончилась или мост её бросил: ответы читать незачем.
        // Срок молчания отсчитывается заново для каждого сообщения и после каждого переподключения.
        let next = tokio::select! {
            () = events.closed() => return,
            next = timeout(SILENCE_LIMIT, replies.next()) => next,
        };
        let Ok(next) = next else {
            events
                .send(Err("runner пропал: нет ни вывода, ни heartbeat".to_owned()))
                .await
                .unwrap_or_default();
            return;
        };
        let wire = match next {
            Ok(Some(body)) => {
                received += 1;
                protocol::decode_body::<ExecEvent>(&body).map_err(|_| "ответ runner не разобран".to_owned())
            }
            Ok(None) => return,
            Err(closed) => {
                tracing::warn!(id, received, reason = %closed, "канал оборвался посреди команды: итог заберётся после переподключения");
                let Some(rest) = reattach(&mut link, id, received, &events).await else {
                    return;
                };
                replies = rest;
                reattached = true;
                continue;
            }
        };
        // Команды, о которой runner не знает, у него после обрыва нет: он перезапущен или вытеснил итог из журнала.
        let wire = match wire {
            Ok(ExecEvent::Err(Error::NotFound)) if reattached => Err("runner перезапущен, итог неизвестен".to_owned()),
            other => other,
        };
        let last = !matches!(&wire, Ok(ExecEvent::Output { .. } | ExecEvent::Heartbeat));
        if events.send(wire).await.is_err() || last {
            return;
        }
    }
}

/// Ждёт открытого канала и просит у runner остаток потока команды `id` после `received` сообщений. `None` — ждать
/// некому: мост бросил команду или сессия кончилась.
async fn reattach(link: &mut Link, id: u64, received: u64, events: &mpsc::Sender<Wire>) -> Option<Responses> {
    let body = Bytes::from(protocol::encode(&DataRequest::Attach(AttachRequest { id, received })).ok()?);
    loop {
        let mux = tokio::select! {
            () = events.closed() => return None,
            mux = link.open() => mux?,
        };
        match mux.call(body.clone()).await {
            Ok(rest) => return Some(rest),
            // И этот канал успел закрыться: ждать следующего.
            Err(closed) => tracing::warn!(id, reason = %closed, "остаток потока команды не запрошен"),
        }
    }
}

/// Транслирует события runner оболочке: вывод — сразу, завершение — возвращается последним ответом.
///
/// `cancel` срабатывает, когда оболочка прислала отмену или пропала: тогда `stop_runner` просит остановить
/// команду, а ответы продолжают приходить до её завершения. Ожидание не ограничено: молчание runner обрывает
/// `read_replies`, а оборвавшийся канал мост переоткрывает сам.
async fn relay(
    events: &mut mpsc::Receiver<Wire>,
    reply: &mpsc::Sender<Reply>,
    mut cancel: oneshot::Receiver<()>,
    stop_runner: oneshot::Sender<()>,
) -> Reply {
    let mut stop_runner = Some(stop_runner);
    let mut cancel_pending = true;
    loop {
        tokio::select! {
            _ = &mut cancel, if cancel_pending => {
                cancel_pending = false;
                if let Some(stop) = stop_runner.take() {
                    // Задача отправки отмены могла уже завершиться: команда закончилась.
                    stop.send(()).unwrap_or_default();
                }
            }
            event = events.recv() => match event {
                None => return Reply::Failed("поток ответов runner оборвался без завершения команды".to_owned()),
                Some(Err(failure)) => return Reply::Failed(failure),
                Some(Ok(ExecEvent::Output { data, .. })) => {
                    // Оболочка могла исчезнуть: её отмена уже обработана, а вывод некому читать.
                    reply.send(Reply::Output(data)).await.unwrap_or_default();
                }
                Some(Ok(ExecEvent::Heartbeat)) => {}
                Some(Ok(ExecEvent::Exit(exit))) => {
                    let status = match exit.status {
                        protocol::ExitStatus::Code(code) => ExitStatus::Code(code),
                        protocol::ExitStatus::Signal(signal) => ExitStatus::Signal(signal),
                    };
                    return Reply::Exit { status, cwd: exit.cwd };
                }
                Some(Ok(ExecEvent::Err(error))) => return Reply::Failed(format!("runner отклонил команду: {error}")),
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/session/exec.rs"]
mod tests;
