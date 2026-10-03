//! Наблюдение за каталогами, которые сессия прочитала через `ReadDir`, и пачки изменений в канал данных.
//!
//! Под наблюдением только прочитанные каталоги, а не всё дерево: на `node_modules` и `target` рекурсивный watch
//! упирается в лимит `max_user_watches`. Каталог встаёт под наблюдение до чтения его записей, поэтому изменение
//! между чтением и подпиской не теряется. Каталог, на который подписаться не вышло, и каталог, события которого
//! могли потеряться (переполнение очереди), получают `Unwatched`: кэш сессии ему больше не верит.
//!
//! События inotify только отмечают пути. Атрибуты берутся `stat` в момент отправки, после окна склейки, поэтому
//! серия записей одного файла даёт одно `Updated`, а последнее событие отражает настоящее состояние.
//!
//! Атрибуты каталога, который сам не наблюдается, на размер и mtime не обновляются: их меняют записи внутри него,
//! а событий о них никто не получает.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use nerve::mux::Mux;
use nerve::protocol::{self, Change, ChangeKind, Error, Root, WatchBatch};
use rustix::fs::inotify::{self, CreateFlags, Event, ReadFlags, WatchFlags};
use rustix::fs::readlink;
use rustix::io::Errno;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, watch};

use super::{Workdir, relock};

#[cfg(test)]
#[path = "../../../tests/runner/fs/watch.rs"]
mod tests;

/// Окно склейки: события за это время после первого уходят одной пачкой.
const WINDOW: Duration = Duration::from_millis(30);

/// Грубая верхняя граница размера пачки: большая пачка занимала бы канал данных дольше, чем нужно одному обновлению.
const BATCH_BYTES: usize = 256 * 1024;

/// Что отслеживается в каталоге: появление, удаление, переименование, содержимое и атрибуты записей.
const MASK: WatchFlags = WatchFlags::ATTRIB
    .union(WatchFlags::CREATE)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::CLOSE_WRITE)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);

/// Наблюдатель за каталогами одного рабочего каталога.
#[derive(Debug)]
pub(super) struct Watcher {
    inotify: OwnedFd,
    /// Настоящий путь рабочего каталога: по нему отличают каталог, достигнутый через ссылку.
    root: PathBuf,
    state: Mutex<State>,
    wake: Notify,
}

#[derive(Debug, Default)]
struct State {
    by_wd: HashMap<i32, String>,
    by_path: HashMap<String, i32>,
    /// Пути, у которых что-то произошло: атрибуты берутся при отправке.
    touched: BTreeSet<String>,
    /// События, которые нельзя вывести из `stat`: удаление каталога и потеря наблюдения.
    notices: Vec<Change>,
}

impl State {
    /// Забывает подписку `wd`.
    fn forget(&mut self, wd: i32) {
        let Some(path) = self.by_wd.remove(&wd) else {
            return;
        };
        if self.by_path.get(&path) == Some(&wd) {
            self.by_path.remove(&path);
        }
    }

    /// Повтор не добавляется: каталог о своём удалении сообщает и сам, и через родителя.
    fn notice(&mut self, path: String, kind: ChangeKind) {
        let change = Change { path, kind };
        if !self.notices.contains(&change) {
            self.notices.push(change);
        }
    }
}

impl Watcher {
    /// `root` — открытый рабочий каталог.
    pub(super) fn new(root: &OwnedFd) -> io::Result<Self> {
        Ok(Self {
            inotify: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?,
            root: fd_path(root)?,
            state: Mutex::default(),
            wake: Notify::new(),
        })
    }

    /// Ставит каталог `path` (нормализованный, без `..`) под наблюдение. `dir` — его дескриптор, открытый
    /// строго внутри рабочего каталога. Неудача — `Unwatched`, а не ошибка чтения каталога; возвращает, удалось ли.
    pub(super) fn watch(&self, path: &str, dir: &OwnedFd) -> bool {
        let mut state = relock(&self.state);
        let Err(reason) = self.add(&mut state, path, dir) else {
            return true;
        };
        tracing::warn!(path, reason, "каталог без наблюдения");
        state.notice(path.to_owned(), ChangeKind::Unwatched);
        drop(state);
        self.wake.notify_one();
        false
    }

    fn add(&self, state: &mut State, path: &str, dir: &OwnedFd) -> Result<(), String> {
        // inotify подписывается по пути, а не по дескриптору: путь через /proc/self/fd указывает ровно на этот
        // каталог, без гонки с подменой имени.
        let proc_path = format!("/proc/self/fd/{}", dir.as_raw_fd());
        let real = fd_path(dir).map_err(|e| format!("путь каталога не определён: {e}"))?;
        if real != self.root.join(path) {
            return Err("каталог достигнут через символическую ссылку или уже удалён".to_owned());
        }
        let wd = inotify::add_watch(&self.inotify, &proc_path, MASK).map_err(|e| format!("inotify_add_watch: {e}"))?;
        if let Some(previous) = state.by_wd.insert(wd, path.to_owned())
            && previous != path
            && state.by_path.get(&previous) == Some(&wd)
        {
            state.by_path.remove(&previous);
        }
        if let Some(stale) = state.by_path.insert(path.to_owned(), wd)
            && stale != wd
        {
            state.by_wd.remove(&stale);
        }
        Ok(())
    }

    /// Конец сессии: снимает все подписки и отбрасывает неотправленное.
    pub(super) fn clear(&self) {
        let mut state = relock(&self.state);
        for wd in state.by_wd.keys() {
            self.unsubscribe(*wd);
        }
        *state = State::default();
    }

    /// Канал данных открылся заново: каждый наблюдаемый каталог получает `Unwatched` и снимается с наблюдения,
    /// накопленные события отбрасываются. Сессия не видела того, что случилось за время обрыва.
    pub(super) fn resync(&self) {
        let mut state = relock(&self.state);
        let mut paths: Vec<String> = state.by_wd.values().cloned().collect();
        paths.sort();
        self.drop_watches(&mut state, |_| true);
        state.touched.clear();
        state.notices.clear();
        for path in paths {
            state.notice(path, ChangeKind::Unwatched);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Читает всё, что накопилось в очереди inotify, и отмечает затронутые пути.
    fn drain(&self) -> io::Result<()> {
        let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
        let mut reader = inotify::Reader::new(&self.inotify, &mut buffer);
        let mut state = relock(&self.state);
        loop {
            match reader.next() {
                Ok(event) => self.apply(&mut state, &event),
                Err(Errno::AGAIN) => return Ok(()),
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn apply(&self, state: &mut State, event: &Event<'_>) {
        let flags = event.events();
        if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
            // Какие события потеряны, неизвестно: ни одному каталогу верить нельзя.
            let mut paths: Vec<String> = state.by_wd.values().cloned().collect();
            paths.sort();
            self.drop_watches(state, |_| true);
            for path in paths {
                state.notice(path, ChangeKind::Unwatched);
            }
            return;
        }
        let Some(dir) = state.by_wd.get(&event.wd()).cloned() else {
            return;
        };
        if flags.contains(ReadFlags::IGNORED) {
            // Ядро сняло подписку само, например при размонтировании; при удалении каталога её уже забыли.
            state.forget(event.wd());
            state.notice(dir, ChangeKind::Unwatched);
            return;
        }
        if flags.intersects(ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF) {
            // Подписки каталога и всего под ним привязаны к старым путям; новое место прочтут заново.
            self.drop_watches(state, |path| is_within(path, &dir));
            state.notice(dir, ChangeKind::Removed);
            return;
        }
        state.touched.insert(dir.clone());
        if let Some(name) = event.file_name().and_then(|n| n.to_str().ok()) {
            let path = join(&dir, name);
            if flags.contains(ReadFlags::ISDIR) && flags.intersects(ReadFlags::DELETE | ReadFlags::MOVED_FROM | ReadFlags::MOVED_TO) {
                // Каталог пропал или на его место встал другой. Сообщение самого каталога (`DELETE_SELF`) ядро может
                // отложить на сколько угодно, поэтому полагаемся на событие родителя.
                self.drop_watches(state, |watched| is_within(watched, &path));
                state.notice(path.clone(), ChangeKind::Removed);
            }
            state.touched.insert(path);
        }
        self.wake.notify_one();
    }

    fn unsubscribe(&self, wd: i32) {
        // INVAL: подписка уже исчезла вместе с каталогом.
        if let Err(e) = inotify::remove_watch(&self.inotify, wd)
            && e != Errno::INVAL
        {
            tracing::warn!(wd, error = %e, "подписка не снята");
        }
    }

    /// Снимает подписки на каталоги, чей путь подходит под `select`.
    fn drop_watches(&self, state: &mut State, select: impl Fn(&str) -> bool) {
        let doomed: Vec<i32> = state.by_wd.iter().filter(|(_, path)| select(path)).map(|(wd, _)| *wd).collect();
        for wd in doomed {
            self.unsubscribe(wd);
            state.forget(wd);
        }
    }

    fn has_pending(&self) -> bool {
        let state = relock(&self.state);
        !state.touched.is_empty() || !state.notices.is_empty()
    }

    fn take_pending(&self) -> (Vec<Change>, BTreeSet<String>) {
        let mut state = relock(&self.state);
        (std::mem::take(&mut state.notices), std::mem::take(&mut state.touched))
    }
}

/// Настоящий путь, на который указывает дескриптор каталога.
pub(super) fn fd_path(fd: &OwnedFd) -> io::Result<PathBuf> {
    let link = readlink(format!("/proc/self/fd/{}", fd.as_raw_fd()), Vec::new())?;
    Ok(PathBuf::from(OsStr::from_bytes(link.to_bytes())))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

/// `path` — это `dir` или лежит под ним. Рабочий каталог — пустой путь.
fn is_within(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Отправляет изменения в текущий канал данных `link`, пока не случится ошибка чтения очереди inotify. Пока канала
/// нет, изменения отбрасываются: при открытии нового канала `Workdir::resync_watch` сообщает о потере наблюдения.
pub(in crate::runner) async fn serve(link: watch::Receiver<Option<Mux>>, workdir: Arc<Workdir>) -> anyhow::Result<()> {
    let watcher = &workdir.watcher;
    let queue = AsyncFd::with_interest(
        watcher.inotify.try_clone().context("очередь inotify не продублирована")?,
        Interest::READABLE,
    )
    .context("очередь inotify не подключена к циклу событий")?;
    tracing::info!("runner отправляет изменения каталогов");
    loop {
        if !watcher.has_pending() {
            tokio::select! {
                ready = queue.readable() => {
                    let mut ready = ready.context("ожидание очереди inotify")?;
                    watcher.drain().context("чтение очереди inotify")?;
                    ready.clear_ready();
                }
                () = watcher.wake.notified() => {}
            }
        }
        if !watcher.has_pending() {
            continue;
        }
        // Пока окно открыто, очередь читается дальше: она не должна переполниться из-за ожидания.
        let window = tokio::time::sleep(WINDOW);
        tokio::pin!(window);
        loop {
            tokio::select! {
                ready = queue.readable() => {
                    let mut ready = ready.context("ожидание очереди inotify")?;
                    watcher.drain().context("чтение очереди inotify")?;
                    ready.clear_ready();
                }
                () = &mut window => break,
            }
        }
        let (notices, touched) = watcher.take_pending();
        let shared = Arc::clone(&workdir);
        let changes = tokio::task::spawn_blocking(move || collect(&shared, notices, &touched))
            .await
            .context("сбор атрибутов упал")?;
        let mux = link.borrow().clone();
        if let Some(mux) = mux {
            publish(&mux, workdir.root_kind(), changes).await;
        } else {
            tracing::debug!(changes = changes.len(), "канала данных нет, изменения отброшены");
        }
    }
}

/// События потери наблюдения идут первыми, затем атрибуты на этот момент.
fn collect(workdir: &Workdir, notices: Vec<Change>, touched: &BTreeSet<String>) -> Vec<Change> {
    let mut changes = notices;
    for path in touched {
        if workdir.hidden(path) {
            continue;
        }
        let kind = match workdir.stat(path) {
            Ok(attr) => ChangeKind::Updated(attr),
            Err(Error::NotFound) => ChangeKind::Removed,
            Err(e) => {
                // Что с путём, неизвестно, значит, нельзя верить и записям его каталога.
                tracing::warn!(path, error = %e, "атрибуты не получены");
                changes.push(Change {
                    path: parent(path).to_owned(),
                    kind: ChangeKind::Unwatched,
                });
                continue;
            }
        };
        changes.push(Change { path: path.clone(), kind });
    }
    changes
}

async fn publish(mux: &Mux, root: Root, changes: Vec<Change>) {
    let mut batch = Vec::new();
    let mut bytes = 0;
    for change in changes {
        bytes += change.path.len() + 64;
        batch.push(change);
        if bytes >= BATCH_BYTES {
            send(mux, root, std::mem::take(&mut batch)).await;
            bytes = 0;
        }
    }
    if !batch.is_empty() {
        send(mux, root, batch).await;
    }
}

async fn send(mux: &Mux, root: Root, changes: Vec<Change>) {
    let body = match protocol::encode(&WatchBatch { root, changes }) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!(error = %e, "пачка изменений не закодирована");
            return;
        }
    };
    if let Err(e) = mux.notify(Bytes::from(body)).await {
        tracing::warn!(error = %e, "пачка изменений не отправлена");
    }
}
