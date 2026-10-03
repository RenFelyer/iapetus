//! Команды сессии, которые переживают обрыв канала данных.
//!
//! Команда принадлежит runner, а не соединению: пока канал оборван, она дорабатывает, а её вывод и итог ждут
//! в журнале. Сессия после переподключения забирает остаток потока (`Attach`) с того сообщения, которое она ещё не
//! получила. Заново команда не запускается никогда: её id остаётся занятым, а повторный запрос с ним получает поток
//! той же команды, пока не кончится сессия.
//!
//! Журнал хранит последние [`RETAIN_BYTES`] вывода. Если сессия просит то, что уже вытеснено, она получает ошибку,
//! а не молчаливый пропуск.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use nerve::mux::Incoming;
use nerve::protocol::{self, Assignment, Error, ExecEvent, ExecRequest};
use tokio::sync::{mpsc, watch};

use crate::runner::exec::Executor;

#[cfg(test)]
#[path = "../../../tests/runner/data/jobs.rs"]
mod tests;

/// Сколько байт вывода одной команды хранит журнал.
const RETAIN_BYTES: usize = 64 << 20;

/// Сколько завершённых команд помнит runner: их итог ждёт сессию, пока она не начала новую или не кончилась.
const KEEP_FINISHED: usize = 256;

/// Сколько событий команды ждёт журнала: полный канал останавливает чтение вывода команды, а с ним и саму команду.
const EVENT_BUFFER: usize = 32;

/// Сколько событий уходит одним заходом, прежде чем читатель снова посмотрит в журнал.
const SEND_BATCH: usize = 64;

#[derive(Debug, Default)]
struct Log {
    events: VecDeque<ExecEvent>,
    /// Номер первого события в `events`: вытесненные из журнала события нумерацию не сдвигают.
    first: u64,
    bytes: usize,
}

/// Одна команда: журнал её событий и счётчик записанных.
#[derive(Debug)]
pub(super) struct Job {
    log: Mutex<Log>,
    /// Сколько событий записано всего; читатели ждут его роста.
    written: watch::Sender<u64>,
}

fn size_of(event: &ExecEvent) -> usize {
    match event {
        ExecEvent::Output { data, .. } => data.len(),
        _ => 64,
    }
}

impl Job {
    fn new() -> Self {
        Self {
            log: Mutex::default(),
            written: watch::channel(0).0,
        }
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, event: ExecEvent) {
        let end = {
            let mut log = self.log();
            log.bytes += size_of(&event);
            log.events.push_back(event);
            while log.bytes > RETAIN_BYTES && log.events.len() > 1 {
                if let Some(dropped) = log.events.pop_front() {
                    log.bytes -= size_of(&dropped);
                    log.first += 1;
                }
            }
            log.first + log.events.len() as u64
        };
        self.written.send_replace(end);
    }

    /// Отправляет события, начиная с номера `from`, пока не уйдёт последнее (`Exit` или `Err`) или канал не оборвётся.
    /// Обрыв команду не трогает: она остаётся в журнале.
    pub(super) async fn stream(&self, from: u64, incoming: &Incoming) {
        let mut written = self.written.subscribe();
        let mut next = from;
        loop {
            let end = *written.borrow_and_update();
            if next > end {
                send(
                    incoming,
                    &ExecEvent::Err(Error::Io("сессия получила больше сообщений, чем есть".to_owned())),
                    true,
                )
                .await;
                return;
            }
            while next < end {
                let batch: Result<Vec<ExecEvent>, ()> = {
                    let log = self.log();
                    if next < log.first {
                        Err(())
                    } else {
                        let skip = usize::try_from(next - log.first).unwrap_or(usize::MAX);
                        Ok(log.events.iter().skip(skip).take(SEND_BATCH).cloned().collect())
                    }
                };
                let Ok(batch) = batch else {
                    send(
                        incoming,
                        &ExecEvent::Err(Error::Io("вывод команды вытеснен из журнала runner".to_owned())),
                        true,
                    )
                    .await;
                    return;
                };
                for event in batch {
                    let last = matches!(event, ExecEvent::Exit(_) | ExecEvent::Err(_));
                    if !send(incoming, &event, last).await || last {
                        return;
                    }
                    next += 1;
                }
            }
            if self.is_finished() {
                // Сессия просит остаток потока, который уже получила целиком.
                send(incoming, &ExecEvent::Err(Error::Io("итог команды уже передан".to_owned())), true).await;
                return;
            }
            if written.changed().await.is_err() {
                return;
            }
        }
    }
}

/// `false` — канал оборвался.
async fn send(incoming: &Incoming, event: &ExecEvent, last: bool) -> bool {
    let Ok(body) = protocol::encode(event) else {
        tracing::error!("событие не закодировано");
        return false;
    };
    if let Err(e) = incoming.respond(Bytes::from(body), last).await {
        tracing::warn!(error = %e, "событие не отправлено, команда продолжает работать");
        return false;
    }
    true
}

/// Команды одного назначения по id: итоги и запуски других назначений сюда не попадают.
#[derive(Debug)]
pub(super) struct Jobs {
    assignment: Assignment,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    jobs: HashMap<u64, Arc<Job>>,
    /// Id в порядке запуска: вытесняются самые старые завершённые.
    order: VecDeque<u64>,
}

impl Jobs {
    pub(super) fn new(assignment: Assignment) -> Self {
        Self {
            assignment,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Запускает команду. Если команда с таким id уже есть — выполняющаяся или завершённая, итог которой ещё ждёт
    /// сессию, — это повтор запроса: возвращается она же, а не новый запуск.
    pub(super) fn start(&self, request: ExecRequest, executor: &Arc<Executor>) -> Arc<Job> {
        let job = Arc::new(Job::new());
        {
            let mut state = self.state();
            if let Some(known) = state.jobs.get(&request.id) {
                return Arc::clone(known);
            }
            state.jobs.insert(request.id, Arc::clone(&job));
            state.order.push_back(request.id);
            Self::evict_finished(&mut state);
        }
        let (events, mut received) = mpsc::channel(EVENT_BUFFER);
        let executor = Arc::clone(executor);
        let assignment = self.assignment;
        tokio::spawn(async move { executor.run(assignment, request, events).await });
        let writer = Arc::clone(&job);
        tokio::spawn(async move {
            while let Some(event) = received.recv().await {
                writer.push(event);
            }
        });
        job
    }

    pub(super) fn get(&self, id: u64) -> Option<Arc<Job>> {
        self.state().jobs.get(&id).cloned()
    }

    /// Конец назначения: итоги его команд не нужны. Выполняющиеся команды останавливает исполнитель.
    pub(super) fn clear(&self) {
        let mut state = self.state();
        state.jobs.clear();
        state.order.clear();
    }

    /// Не даёт завершённым командам копиться без счёта: лишние старые забываются.
    fn evict_finished(state: &mut State) {
        while state.jobs.len() > KEEP_FINISHED {
            let finished = state
                .order
                .iter()
                .position(|id| state.jobs.get(id).is_some_and(|job| job.is_finished()));
            let Some(position) = finished else { return };
            if let Some(id) = state.order.remove(position) {
                state.jobs.remove(&id);
            }
        }
    }
}

impl Job {
    fn is_finished(&self) -> bool {
        self.log()
            .events
            .back()
            .is_some_and(|event| matches!(event, ExecEvent::Exit(_) | ExecEvent::Err(_)))
    }
}
