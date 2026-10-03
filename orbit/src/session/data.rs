//! Канал данных на стороне orbit session: состояние соединения назначенного runner и мультиплексор поверх него.
//!
//! Пускать ли runner, решает сессия: принимается один fingerprint — назначенного runner ([`Data::assign`]). Новое
//! соединение того же runner вытесняет старое, смена назначения закрывает текущее. У runner есть одна подсистема
//! с мультиплексором ([`Mux`]), по которой идут файловые запросы, команды и изменения.
//!
//! Обрыв и вытеснение сообщаются событиями ([`Event`]): кэш FUSE, построенный по событиям watch, после потери связи
//! верить нельзя.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use nerve::fingerprint::Fingerprint;
use nerve::mux::{Mux, Requests};
use nerve::protocol::{self, WatchBatch};
use russh::Disconnect;
use russh::server::Handle;
use tokio::sync::{mpsc, watch};

#[cfg(test)]
#[path = "../../tests/session/data.rs"]
mod tests;

/// Что случилось с каналом данных.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    /// runner вошёл, канал открыт. Всё, что сессия знала о runner до этого, нужно перепроверить.
    Connected,
    /// Канал потерян или заменён: события watch за это время не дошли.
    Disconnected,
    Watch(WatchBatch),
}

/// Текущий канал данных; `None`, пока runner не вошёл.
#[derive(Debug, Clone)]
pub(super) struct Link(watch::Receiver<Option<Mux>>);

impl Link {
    pub(super) fn current(&self) -> Option<Mux> {
        self.0.borrow().clone()
    }

    pub(super) fn is_connected(&self) -> bool {
        self.0.borrow().as_ref().is_some_and(|mux| !mux.is_closed())
    }

    /// Ждёт смены канала и сообщает, подключён ли runner теперь; `None` — каналов больше не будет.
    pub(super) async fn changed(&mut self) -> Option<bool> {
        self.0.changed().await.ok()?;
        Some(self.is_connected())
    }

    /// Ждёт открытого канала: текущего, если он ещё не закрыт, иначе следующего. `None` — каналов больше не будет.
    pub(super) async fn open(&mut self) -> Option<Mux> {
        let open = self.0.wait_for(|mux| mux.as_ref().is_some_and(|mux| !mux.is_closed())).await.ok()?;
        open.clone()
    }
}

#[derive(Debug)]
struct Active {
    id: u64,
    runner: Fingerprint,
    handle: Handle,
    /// Мультиплексор открыт: `Connected` отправлен, и значит, нужен `Disconnected`.
    attached: bool,
}

#[derive(Debug, Default)]
struct State {
    /// Назначенный runner; без него не пускают никого.
    runner: Option<Fingerprint>,
    active: Option<Active>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    next_id: AtomicU64,
    link: watch::Sender<Option<Mux>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Убирает активное соединение из состояния; закрывать его и сообщать о потере — дело вызывающего через
    /// [`Shared::drop_connection`].
    fn take_active(&self, state: &mut State) -> Option<Active> {
        let active = state.active.take()?;
        self.link.send_replace(None);
        Some(active)
    }

    fn drop_connection(&self, active: Active, reason: &'static str) {
        if active.attached {
            // Получатель событий мог исчезнуть вместе с сессией.
            self.events.send(Event::Disconnected).unwrap_or_default();
        }
        tokio::spawn(async move {
            active
                .handle
                .disconnect(Disconnect::ByApplication, reason.to_owned(), String::new())
                .await
                .unwrap_or_default();
        });
    }

    fn is_active(&self, id: u64) -> bool {
        self.state().active.as_ref().is_some_and(|active| active.id == id)
    }
}

/// Состояние канала данных сессии: назначенный runner и его соединение.
#[derive(Debug, Clone)]
pub(super) struct Data {
    shared: Arc<Shared>,
}

impl Data {
    /// Возвращает состояние, текущий канал и поток событий.
    pub(super) fn new() -> (Self, Link, mpsc::UnboundedReceiver<Event>) {
        let (link, link_receiver) = watch::channel(None);
        let (events, events_receiver) = mpsc::unbounded_channel();
        let data = Self {
            shared: Arc::new(Shared {
                state: Mutex::default(),
                next_id: AtomicU64::new(1),
                link,
                events,
            }),
        };
        (data, Link(link_receiver), events_receiver)
    }

    /// Назначает runner: пускают только его. Соединение другого runner закрывается; `None` закрывает любое.
    pub(super) fn assign(&self, runner: Option<Fingerprint>) {
        let evicted = {
            let mut state = self.shared.state();
            state.runner = runner;
            if state
                .active
                .as_ref()
                .is_some_and(|active| Some(&active.runner) == state.runner.as_ref())
            {
                None
            } else {
                self.shared.take_active(&mut state)
            }
        };
        if let Some(old) = evicted {
            tracing::info!(runner = %old.runner, "runner сменился, соединение закрыто");
            self.shared.drop_connection(old, "runner сменился");
        }
    }

    /// Номер нового входящего соединения.
    pub(super) fn next_connection_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn is_assigned(&self, runner: &Fingerprint) -> bool {
        self.shared.state().runner.as_ref() == Some(runner)
    }

    /// Вход принят. Старое соединение вытесняется; `false`, если за время входа runner сменился.
    pub(super) fn activate(&self, id: u64, runner: &Fingerprint, handle: Handle) -> bool {
        let evicted = {
            let mut state = self.shared.state();
            if state.runner.as_ref() != Some(runner) {
                return false;
            }
            let evicted = self.shared.take_active(&mut state);
            state.active = Some(Active {
                id,
                runner: runner.clone(),
                handle,
                attached: false,
            });
            evicted
        };
        if let Some(old) = evicted {
            tracing::info!(%runner, "новое соединение runner вытеснило старое");
            self.shared.drop_connection(old, "вытеснено новым соединением");
        }
        true
    }

    /// Мультиплексор соединения `id` готов: канал публикуется, входящие сообщения разбираются в события.
    /// `false`, если соединение уже не активно.
    pub(super) fn attach(&self, id: u64, mux: Mux, mut requests: Requests) -> bool {
        {
            let mut state = self.shared.state();
            let Some(active) = state.active.as_mut().filter(|active| active.id == id) else {
                return false;
            };
            active.attached = true;
            self.shared.link.send_replace(Some(mux.clone()));
            self.shared.events.send(Event::Connected).unwrap_or_default();
        }
        let data = self.clone();
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if !data.shared.is_active(id) {
                    break;
                }
                match protocol::decode_body::<WatchBatch>(request.body()) {
                    Ok(batch) => data.shared.events.send(Event::Watch(batch)).unwrap_or_default(),
                    // Из канала с разобранной версией мусор приходит от неисправного runner: тот же канал уже не
                    // надёжен, runner войдёт заново.
                    Err(error) => {
                        tracing::warn!(%error, "сообщение runner не разобрано, канал закрывается");
                        mux.close();
                        break;
                    }
                }
            }
            data.release(id);
        });
        true
    }

    /// Соединение `id` закончилось.
    pub(super) fn release(&self, id: u64) {
        let active = {
            let mut state = self.shared.state();
            if state.active.as_ref().is_none_or(|active| active.id != id) {
                return;
            }
            self.shared.take_active(&mut state)
        };
        if let Some(active) = active {
            tracing::info!(runner = %active.runner, "канал данных закрыт");
            self.shared.drop_connection(active, "канал данных закрыт");
        }
    }
}
