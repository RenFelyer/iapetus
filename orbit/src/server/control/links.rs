//! Открытые каналы управления в памяти server: не больше одного на fingerprint, новый вытесняет прежний.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use nerve::fingerprint::Fingerprint;
use nerve::mux::Mux;

struct Link<T> {
    id: u64,
    mux: Mux,
    extra: T,
}

/// Каналы управления узлов одной роли; `T` — то, что server знает об узле сверх канала.
pub(super) struct Links<T> {
    open: Mutex<HashMap<Fingerprint, Link<T>>>,
    next_id: AtomicU64,
}

impl<T> std::fmt::Debug for Links<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Links").finish_non_exhaustive()
    }
}

impl<T: Clone> Links<T> {
    pub(super) fn new() -> Self {
        Self {
            open: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    fn open(&self) -> MutexGuard<'_, HashMap<Fingerprint, Link<T>>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Записывает канал узла и закрывает прежний канал того же fingerprint. Узел в сети, пока жив возвращённый
    /// [`Membership`].
    pub(super) fn join(self: &Arc<Self>, fingerprint: Fingerprint, mux: Mux, extra: T) -> Membership<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let displaced = self.open().insert(fingerprint.clone(), Link { id, mux, extra });
        if let Some(old) = displaced {
            tracing::info!(%fingerprint, "канал управления вытеснен новым соединением");
            old.mux.close();
        }
        Membership {
            links: Arc::clone(self),
            fingerprint,
            id,
        }
    }

    /// Канал узла и сведения о нём; `None` — узла нет в сети.
    pub(super) fn get(&self, fingerprint: &Fingerprint) -> Option<(Mux, T)> {
        self.open().get(fingerprint).map(|link| (link.mux.clone(), link.extra.clone()))
    }

    pub(super) fn is_online(&self, fingerprint: &Fingerprint) -> bool {
        self.open().contains_key(fingerprint)
    }
}

/// Запись узла в [`Links`]; исчезает вместе с ней, если её не вытеснил более новый канал.
pub(super) struct Membership<T: Clone> {
    links: Arc<Links<T>>,
    fingerprint: Fingerprint,
    id: u64,
}

impl<T: Clone> std::fmt::Debug for Membership<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Membership")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl<T: Clone> Drop for Membership<T> {
    fn drop(&mut self) {
        let mut open = self.links.open();
        if open.get(&self.fingerprint).is_some_and(|link| link.id == self.id) {
            open.remove(&self.fingerprint);
        }
    }
}
