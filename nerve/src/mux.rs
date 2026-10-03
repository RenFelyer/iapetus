//! Мультиплексор запросов поверх одного байтового потока: основа каналов управления и данных между узлами.
//!
//! Обе стороны равноправны: каждая шлёт запросы ([`Mux::call`], [`Mux::notify`]) и принимает чужие
//! ([`Incoming`]). На запрос идёт один ответ или поток ответов; последний помечен. Большие сообщения режутся на
//! кадры [`FRAME_BODY`] и уходят по кругу вместе с другими, поэтому мелкий запрос не ждёт большой файл.
//!
//! Первый кадр каждой стороны несёт версию протокола. Обрыв потока, мусор и чужая версия закрывают мультиплексор с
//! причиной [`Closed`]: все ожидающие вызовы и ответы получают её ошибкой, а не зависают.

mod frame;

#[cfg(test)]
#[path = "../tests/mux.rs"]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, watch};

use crate::protocol::VERSION;
use frame::{Assembler, FrameReader, HEADER, LAST, Message, Outgoing, REQUEST, Scheduler, encode_frame};

pub use frame::{FRAME_BODY, FrameError};

/// Предел собираемого сообщения: самый большой файл плюс служебные поля.
pub const MAX_MESSAGE: usize = crate::protocol::MAX_FILE + (1 << 20);

/// Сколько байт кадров копится в одной записи в поток: кадры разных сообщений уходят вместе.
const ROUND_BYTES: usize = 48 * 1024;

/// Сколько чужих запросов ждут обработчика, прежде чем чтение потока остановится.
const INCOMING_BUFFER: usize = 1024;

/// Id первого кадра: версия протокола. Запросы получают id от 1.
const HELLO_ID: u32 = 0;

/// Id всех уведомлений. Сообщения одного id очередь отправки не смешивает и не переставляет, поэтому уведомления
/// приходят в том порядке, в каком отправлены, хотя большое и мелкое запросы с разными id обгоняют друг друга.
const NOTIFY_ID: u32 = u32::MAX;

/// Почему мультиплексор закрыт.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// Закрыт этой стороной.
    Local,
    /// Собеседник закрыл поток или он оборвался.
    Io(String),
    /// Поток не разбирается.
    Protocol(String),
    /// Собеседник говорит на другой версии протокола.
    Version(Option<u16>),
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("канал закрыт"),
            Self::Io(text) => write!(f, "канал оборвался: {text}"),
            Self::Protocol(text) => write!(f, "канал не разбирается: {text}"),
            Self::Version(Some(got)) => write!(f, "собеседник работает с версией протокола {got}, ожидалась {VERSION}"),
            Self::Version(None) => write!(f, "собеседник не прислал версию протокола, ожидалась {VERSION}"),
        }
    }
}

impl std::error::Error for Closed {}

#[derive(Debug)]
struct State {
    pending: Mutex<HashMap<u32, mpsc::UnboundedSender<(Bytes, bool)>>>,
    closed: watch::Sender<Option<Closed>>,
}

impl State {
    fn pending(&self) -> MutexGuard<'_, HashMap<u32, mpsc::UnboundedSender<(Bytes, bool)>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Первая причина остаётся; ожидающие ответа узнают о закрытии по оборванному каналу.
    fn close(&self, reason: Closed) {
        self.closed.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
        self.pending().clear();
    }

    fn reason(&self) -> Closed {
        self.closed.borrow().clone().unwrap_or(Closed::Local)
    }

    async fn wait_closed(&self) -> Closed {
        let mut closed = self.closed.subscribe();
        match closed.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or(Closed::Local),
            Err(_) => Closed::Local,
        }
    }
}

#[derive(Debug)]
struct Inner {
    state: Arc<State>,
    out: mpsc::Sender<Outgoing>,
    next_id: AtomicU32,
    /// Срабатывает, когда handles больше нет: чтение прекращается, а запись дописывает очередь и закрывает поток.
    hangup: Arc<Notify>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.hangup.notify_one();
    }
}

/// Одна сторона канала. Клонируется: все копии работают с одним потоком; поток закрывается, когда не
/// остаётся ни копий, ни [`Requests`].
#[derive(Debug, Clone)]
pub struct Mux {
    inner: Arc<Inner>,
}

/// Запросы собеседника. Пока приёмник жив, поток открыт, даже если `Mux` никто не держит.
#[derive(Debug)]
pub struct Requests {
    queue: mpsc::Receiver<Incoming>,
    _alive: Mux,
}

impl Requests {
    /// Следующий запрос; `None` — канал закрыт.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.queue.recv().await
    }
}

/// Запрос собеседника; ответить на него можно несколько раз, последний ответ помечается.
#[derive(Debug)]
pub struct Incoming {
    id: u32,
    body: Bytes,
    out: mpsc::Sender<Outgoing>,
    state: Arc<State>,
}

impl Incoming {
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    pub fn into_body(self) -> Bytes {
        self.body
    }

    /// Отправляет ответ; `last` завершает ответы на этот запрос.
    pub async fn respond(&self, body: Bytes, last: bool) -> Result<(), Closed> {
        let flags = if last { LAST } else { 0 };
        self.out
            .send(Outgoing { id: self.id, flags, body })
            .await
            .map_err(|_| self.state.reason())
    }
}

/// Ответы на один запрос.
#[derive(Debug)]
pub struct Responses {
    replies: mpsc::UnboundedReceiver<(Bytes, bool)>,
    state: Arc<State>,
    finished: bool,
}

impl Responses {
    /// Следующий ответ; `None` — последний уже получен. Закрытие канала до последнего ответа — ошибка.
    pub async fn next(&mut self) -> Result<Option<Bytes>, Closed> {
        if self.finished {
            return Ok(None);
        }
        match self.replies.recv().await {
            Some((body, last)) => {
                self.finished = last;
                Ok(Some(body))
            }
            None => Err(self.state.reason()),
        }
    }
}

impl Mux {
    /// Запускает чтение и запись `stream`; чужие запросы приходят через второй результат.
    pub fn new<S>(stream: S) -> (Self, Requests)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let (out, queue) = mpsc::channel(256);
        let (incoming_tx, incoming) = mpsc::channel(INCOMING_BUFFER);
        let state = Arc::new(State {
            pending: Mutex::default(),
            closed: watch::channel(None).0,
        });
        let hangup = Arc::new(Notify::new());
        tokio::spawn(run(write_loop(write, queue), Arc::clone(&state), None));
        tokio::spawn(run(
            read_loop(read, Arc::clone(&state), out.clone(), incoming_tx),
            Arc::clone(&state),
            Some(Arc::clone(&hangup)),
        ));
        let mux = Self {
            inner: Arc::new(Inner {
                state,
                out,
                next_id: AtomicU32::new(1),
                hangup,
            }),
        };
        let requests = Requests {
            queue: incoming,
            _alive: mux.clone(),
        };
        (mux, requests)
    }

    fn allocate_id(&self) -> u32 {
        loop {
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            if id != HELLO_ID && id != NOTIFY_ID {
                return id;
            }
        }
    }

    /// Отправляет запрос и возвращает его ответы.
    pub async fn call(&self, body: Bytes) -> Result<Responses, Closed> {
        let state = &self.inner.state;
        if state.closed.borrow().is_some() {
            return Err(state.reason());
        }
        let id = self.allocate_id();
        let (reply_tx, replies) = mpsc::unbounded_channel();
        state.pending().insert(id, reply_tx);
        // Закрытие между проверкой и вставкой очистило бы таблицу раньше: тогда ответов не будет.
        if state.closed.borrow().is_some() {
            state.pending().remove(&id);
            return Err(state.reason());
        }
        let sent = self
            .inner
            .out
            .send(Outgoing {
                id,
                flags: REQUEST | LAST,
                body,
            })
            .await;
        if sent.is_err() {
            state.pending().remove(&id);
            return Err(state.reason());
        }
        Ok(Responses {
            replies,
            state: Arc::clone(state),
            finished: false,
        })
    }

    /// Запрос с одним ответом.
    pub async fn request(&self, body: Bytes) -> Result<Bytes, Closed> {
        let mut responses = self.call(body).await?;
        match responses.next().await? {
            Some(reply) => Ok(reply),
            None => Err(Closed::Protocol("ответа нет".to_owned())),
        }
    }

    /// Запрос без ответа: собеседник отвечать на него не должен. Уведомления доходят в порядке отправки.
    pub async fn notify(&self, body: Bytes) -> Result<(), Closed> {
        let state = &self.inner.state;
        if state.closed.borrow().is_some() {
            return Err(state.reason());
        }
        self.inner
            .out
            .send(Outgoing {
                id: NOTIFY_ID,
                flags: REQUEST | LAST,
                body,
            })
            .await
            .map_err(|_| state.reason())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.state.closed.borrow().is_some()
    }

    /// Ждёт закрытия и возвращает причину.
    pub async fn closed(&self) -> Closed {
        self.inner.state.wait_closed().await
    }

    /// Закрывает поток: ожидающие вызовы получают [`Closed::Local`].
    pub fn close(&self) {
        self.inner.state.close(Closed::Local);
    }
}

/// Запускает половину мультиплексора: её завершение закрывает всё, а закрытие снаружи её останавливает. `hangup`
/// останавливает только эту половину: когда handles ушли, запись ещё дописывает ответы, которые обработчики успели
/// поставить в очередь.
async fn run(half: impl Future<Output = Closed>, state: Arc<State>, hangup: Option<Arc<Notify>>) {
    let hung_up = async {
        match &hangup {
            Some(hangup) => hangup.notified().await,
            None => std::future::pending().await,
        }
    };
    let reason = tokio::select! {
        reason = half => reason,
        _ = state.wait_closed() => return,
        () = hung_up => return,
    };
    state.close(reason);
}

async fn write_loop<W: AsyncWrite + Unpin>(mut write: W, mut queue: mpsc::Receiver<Outgoing>) -> Closed {
    let mut buf = BytesMut::with_capacity(ROUND_BYTES + HEADER + FRAME_BODY);
    encode_frame(&mut buf, HELLO_ID, 0, &VERSION.to_be_bytes());
    let mut scheduler = Scheduler::default();
    loop {
        if let Err(e) = write.write_all(&buf).await {
            return Closed::Io(e.to_string());
        }
        if let Err(e) = write.flush().await {
            return Closed::Io(e.to_string());
        }
        buf.clear();
        if scheduler.is_empty() {
            match queue.recv().await {
                Some(message) => scheduler.push(message),
                // Очередь дописана и handles ушли.
                None => return Closed::Local,
            }
        }
        while let Ok(message) = queue.try_recv() {
            scheduler.push(message);
        }
        scheduler.round(&mut buf, ROUND_BYTES);
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut read: R,
    state: Arc<State>,
    out: mpsc::Sender<Outgoing>,
    incoming: mpsc::Sender<Incoming>,
) -> Closed {
    let mut frames = FrameReader::default();
    let mut assembler = Assembler::new(MAX_MESSAGE);
    let mut chunk = vec![0; ROUND_BYTES];
    let mut greeted = false;
    loop {
        loop {
            let frame = match frames.next() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return Closed::Protocol(e.to_string()),
            };
            if !greeted {
                // Первый кадр — версия; остальное не разбирается, пока она не сошлась.
                let version = <[u8; 2]>::try_from(&*frame.body).ok().map(u16::from_be_bytes);
                if frame.id != HELLO_ID || frame.flags != 0 || version.is_none() {
                    return Closed::Version(None);
                }
                if version != Some(VERSION) {
                    return Closed::Version(version);
                }
                greeted = true;
                continue;
            }
            let message = match assembler.add(frame) {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(e) => return Closed::Protocol(e.to_string()),
            };
            deliver(&state, &out, &incoming, message).await;
        }
        match read.read(&mut chunk).await {
            Ok(0) => return Closed::Io("собеседник закрыл поток".to_owned()),
            Ok(n) => frames.push(&chunk[..n]),
            Err(e) => return Closed::Io(e.to_string()),
        }
    }
}

async fn deliver(state: &Arc<State>, out: &mpsc::Sender<Outgoing>, incoming: &mpsc::Sender<Incoming>, message: Message) {
    let Message { id, request, last, body } = message;
    if request {
        let request = Incoming {
            id,
            body,
            out: out.clone(),
            state: Arc::clone(state),
        };
        // Обработчик ушёл: запросы собеседника некому разбирать, а канал живёт ради своих запросов.
        drop(incoming.send(request).await);
        return;
    }
    let mut pending = state.pending();
    let Some(reply) = pending.get(&id) else {
        // Ответ на запрос, от которого отказались, или чужой id.
        return;
    };
    if reply.send((body, last)).is_err() || last {
        pending.remove(&id);
    }
}
