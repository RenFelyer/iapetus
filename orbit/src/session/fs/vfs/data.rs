//! Содержимое: открытие, чтение, запись, `flush`.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nerve::protocol::{Attr, Expect, Kind};
use rustix::fs::OFlags;
use rustix::io::Errno;

use super::{Entry, Fh, FsResult, Handle, Known, LocalFile, Open, Pending, Seen, Vfs, now_ns};
use crate::session::fs::inodes::{Ino, Owner, join};

fn writable(flags: OFlags) -> bool {
    flags.intersects(OFlags::WRONLY | OFlags::RDWR)
}

fn slice(bytes: &[u8], offset: u64, size: u32) -> Vec<u8> {
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
    let end = start.saturating_add(size as usize).min(bytes.len());
    bytes[start..end].to_vec()
}

/// Записывает `data` в `buf` с позиции `offset`; промежуток заполняется нулями. Файл не растёт больше `limit`.
fn splice(buf: &mut Vec<u8>, offset: u64, data: &[u8], limit: usize) -> FsResult<()> {
    let start = usize::try_from(offset).map_err(|_| Errno::FBIG)?;
    let end = start.checked_add(data.len()).filter(|end| *end <= limit).ok_or(Errno::FBIG)?;
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[start..end].copy_from_slice(data);
    Ok(())
}

fn read_file(file: &File, offset: u64, size: u32) -> FsResult<Vec<u8>> {
    let mut buf = vec![0; size as usize];
    let mut filled = 0;
    while filled < buf.len() {
        let read = file.read_at(&mut buf[filled..], offset + filled as u64).map_err(|_| Errno::IO)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    Ok(buf)
}

fn read_whole(file: &File) -> FsResult<Vec<u8>> {
    let len = file.metadata().map_err(|_| Errno::IO)?.len();
    read_file(file, 0, u32::try_from(len).map_err(|_| Errno::IO)?)
}

impl Vfs {
    fn register(&self, ino: Ino, open: Open) -> Fh {
        let pending_size = match &open {
            Open::Pending(pending) => Some(AtomicU64::new(pending.buf.len() as u64)),
            _ => None,
        };
        let handle = Handle {
            ino,
            kind: tokio::sync::Mutex::new(open),
            pending_size,
        };
        let mut state = self.lock();
        let fh = state.next_fh;
        state.next_fh += 1;
        state.handles.insert(fh, Arc::new(handle));
        fh
    }

    fn handle(&self, fh: Fh) -> FsResult<Arc<Handle>> {
        self.lock().handles.get(&fh).cloned().ok_or(Errno::BADF)
    }

    pub(crate) async fn open(&self, ino: Ino, flags: OFlags) -> FsResult<Fh> {
        let (path, is_local) = {
            let state = self.lock();
            let node = state.inodes.node(ino).ok_or(Errno::STALE)?;
            (node.path.clone(), node.local.is_some())
        };
        if is_local {
            if writable(flags) && flags.contains(OFlags::TRUNC) {
                let mut state = self.lock();
                if let Some(local) = state.inodes.node_mut(ino).and_then(|node| node.local.as_mut()) {
                    local.content.clear();
                    local.mtime_ns = now_ns();
                }
            }
            return Ok(self.register(ino, Open::Local));
        }
        let path = path.ok_or(Errno::STALE)?;
        if writable(flags) {
            return self.open_for_write(ino, path, flags).await;
        }
        let file = self.ensure_cached(&path).await?;
        Ok(self.register(ino, Open::Cached(file)))
    }

    /// Копия файла в `/work`. Первое обращение после смены эпохи сверяет хеш с runner, остальные идут без проверок,
    /// пока кэш метаданных не узнал другие атрибуты файла.
    pub(super) async fn ensure_cached(&self, path: &str) -> FsResult<Arc<File>> {
        let epoch = self.epoch.current();
        let known = {
            let mut state = self.lock();
            state.cache.touch(path);
            let attr_now = state.meta.get(path, epoch);
            // В каталоге без наблюдения об изменениях никто не скажет: версию сверяем при каждом `open`.
            let watched = !state.meta.in_unwatched(path);
            state.cache.get(path).and_then(|entry| {
                let unchanged = match attr_now {
                    Some(Known::Present(attr)) => entry.attr == Some(attr),
                    Some(Known::Absent) => false,
                    None => true,
                };
                let checked = watched && unchanged && entry.checked.is_some_and(|checked| checked >= epoch);
                Some((entry.seen, entry.file.clone()?, checked))
            })
        };
        if let Some((Seen::Hash(hash), file, checked)) = known {
            if checked {
                return Ok(file);
            }
            let Some((attr, current)) = self.runner_hash(path).await? else {
                return Err(Errno::NOENT);
            };
            if current == hash {
                self.lock().cache.mark_checked(path, epoch, attr);
                return Ok(file);
            }
        }
        let (attr, sha256, bytes) = self.fetch(path).await?;
        let mut state = self.lock();
        if let Err(error) = state.cache.store(path, Seen::Hash(sha256), Some(&bytes), attr, epoch) {
            tracing::error!(path, %error, "кэш-файл не записан");
            return Err(Errno::IO);
        }
        state.cache.get(path).and_then(|entry| entry.file.clone()).ok_or(Errno::IO)
    }

    /// Сверяет версию на runner с той, которую видел агент, перед записью на месте. Возвращает хеш, на который
    /// рассчитывает запись.
    async fn prepare_write(&self, path: &str) -> FsResult<[u8; 32]> {
        let seen = self.lock().cache.get(path).map(|entry| entry.seen);
        let Some((_, current)) = self.runner_hash(path).await? else {
            return Err(Errno::NOENT);
        };
        // «Файла нет» сюда не доходит: чтобы открыть файл, ядро сначала нашло его `lookup`, и это знание агент уже получил.
        if let Some(Seen::Hash(seen)) = seen
            && seen != current
        {
            self.stale(path);
            return Err(Errno::CANCELED);
        }
        Ok(current)
    }

    /// Содержимое версии `hash`: из кэш-файла, а если его нет или он другой — с runner. Если на runner уже другая
    /// версия, правка рассчитана на устаревшее.
    async fn load_content(&self, path: &str, hash: [u8; 32]) -> FsResult<Vec<u8>> {
        let file = {
            let state = self.lock();
            state
                .cache
                .get(path)
                .filter(|entry| entry.seen == Seen::Hash(hash))
                .and_then(|entry| entry.file.clone())
        };
        if let Some(file) = file {
            return read_whole(&file);
        }
        let (_, sha256, bytes) = self.fetch(path).await?;
        if sha256 == hash {
            return Ok(bytes);
        }
        self.stale(path);
        Err(Errno::CANCELED)
    }

    async fn open_for_write(&self, ino: Ino, path: String, flags: OFlags) -> FsResult<Fh> {
        let base = self.prepare_write(&path).await?;
        let truncate = flags.contains(OFlags::TRUNC);
        let buf = if truncate {
            Vec::new()
        } else {
            self.load_content(&path, base).await?
        };
        let pending = Pending {
            path,
            base: Expect::Hash(base),
            buf,
            dirty: truncate,
        };
        Ok(self.register(ino, Open::Pending(pending)))
    }

    /// Создаёт файл. Временный файл профиля `Rename` живёт только в сессии до `rename` поверх цели. Любой другой новый
    /// файл — запись на месте: она уйдёт на runner на `flush`, иначе файл, созданный сразу под своим именем, пропал бы.
    pub(crate) async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        umask: u32,
        flags: OFlags,
        caller: Owner,
    ) -> FsResult<(Entry, Fh)> {
        let path = join(&self.path_of(parent)?, name);
        if self.is_hidden(&path) {
            // Как у `lookup`: спрятанного пути для агента нет. `EACCES` агент счёл бы поводом писать на месте.
            return Err(Errno::NOENT);
        }
        self.reject_own_link(&path)?;
        if self.resolve(&path).await?.is_some() {
            if flags.contains(OFlags::EXCL) {
                // `EEXIST` агент обошёл бы перезаписью цели на месте; `ECANCELED` — «файл изменился, повтори».
                return Err(Errno::CANCELED);
            }
            // Ядро не знало файла и вызвало `create`: для существующего файла это обычный `open`.
            let entry = self.lookup(parent, name).await?;
            let fh = self.open(entry.ino, flags).await?;
            return Ok((entry, fh));
        }
        let mode = mode & !umask & 0o7777;
        let epoch = self.epoch.current();
        let local = self.config.profile == super::Profile::Rename && self.config.temp_name.is_some_and(|is_temp_name| is_temp_name(name));
        let (ino, attr) = {
            let mut state = self.lock();
            let ino = state.inodes.lookup(&path);
            let attr = Attr {
                kind: Kind::File,
                size: 0,
                mtime_ns: now_ns(),
                mode,
            };
            let Some(node) = state.inodes.node_mut(ino) else {
                return Err(Errno::STALE);
            };
            node.owner = Some(caller);
            if local {
                node.local = Some(LocalFile {
                    content: Vec::new(),
                    mode,
                    mtime_ns: attr.mtime_ns,
                });
            } else {
                state.meta.put(&path, Some(attr), epoch);
            }
            (ino, attr)
        };
        let open = if local {
            Open::Local
        } else {
            Open::Pending(Pending {
                path,
                base: Expect::Absent,
                buf: Vec::new(),
                dirty: true,
            })
        };
        let fh = self.register(ino, open);
        let state = self.lock();
        Ok((self.entry(&state, ino, attr), fh))
    }

    pub(crate) async fn read(&self, fh: Fh, offset: u64, size: u32) -> FsResult<Vec<u8>> {
        let handle = self.handle(fh)?;
        let kind = handle.kind.lock().await;
        match &*kind {
            Open::Cached(file) => read_file(file, offset, size),
            Open::Pending(pending) => Ok(slice(&pending.buf, offset, size)),
            Open::Local => {
                let state = self.lock();
                let local = state
                    .inodes
                    .node(handle.ino)
                    .and_then(|node| node.local.as_ref())
                    .ok_or(Errno::BADF)?;
                Ok(slice(&local.content, offset, size))
            }
        }
    }

    pub(crate) async fn write(&self, fh: Fh, offset: u64, data: &[u8]) -> FsResult<usize> {
        let handle = self.handle(fh)?;
        let mut kind = handle.kind.lock().await;
        match &mut *kind {
            Open::Pending(pending) => {
                splice(&mut pending.buf, offset, data, self.config.max_file)?;
                pending.dirty = true;
                if let Some(size) = &handle.pending_size {
                    size.store(pending.buf.len() as u64, Ordering::Relaxed);
                }
            }
            Open::Local => {
                let mut state = self.lock();
                let local = state
                    .inodes
                    .node_mut(handle.ino)
                    .and_then(|node| node.local.as_mut())
                    .ok_or(Errno::BADF)?;
                splice(&mut local.content, offset, data, self.config.max_file)?;
                local.mtime_ns = now_ns();
            }
            Open::Cached(_) => return Err(Errno::BADF),
        }
        Ok(data.len())
    }

    /// Правки на месте уходят на runner. Временный файл профиля `Rename` остаётся в сессии до `rename`.
    pub(crate) async fn flush(&self, fh: Fh) -> FsResult<()> {
        let handle = self.handle(fh)?;
        self.flush_handle(&handle).await
    }

    async fn flush_handle(&self, handle: &Handle) -> FsResult<()> {
        let mut kind = handle.kind.lock().await;
        let Open::Pending(pending) = &mut *kind else { return Ok(()) };
        if !pending.dirty {
            return Ok(());
        }
        let (_, sha256) = self.push(&pending.path, pending.base, &pending.buf).await?;
        pending.base = Expect::Hash(sha256);
        pending.dirty = false;
        Ok(())
    }

    pub(crate) async fn release(&self, fh: Fh) {
        let handle = self.lock().handles.remove(&fh);
        if let Some(handle) = handle
            && let Err(error) = self.flush_handle(&handle).await
        {
            // Закрытие уже не может сообщить ошибку агенту: он получил её на `flush`.
            tracing::warn!(%error, "правки при закрытии файла не ушли на runner");
        }
    }

    /// `truncate` и `ftruncate` файла runner.
    pub(super) async fn set_size(&self, path: &str, fh: Option<Fh>, size: u64) -> FsResult<()> {
        let size = usize::try_from(size)
            .ok()
            .filter(|size| *size <= self.config.max_file)
            .ok_or(Errno::FBIG)?;
        let handle = fh.map(|fh| self.handle(fh)).transpose()?;
        let Some(handle) = handle else {
            // `truncate` по пути: версию сверяем, содержимое меняем и сразу отправляем.
            let base = self.prepare_write(path).await?;
            let mut content = self.load_content(path, base).await?;
            content.resize(size, 0);
            self.push(path, Expect::Hash(base), &content).await?;
            return Ok(());
        };
        let mut kind = handle.kind.lock().await;
        let Open::Pending(pending) = &mut *kind else {
            return Err(Errno::BADF);
        };
        // Запись уже открыта, но за это время файл на runner мог измениться.
        let current = self.runner_hash(&pending.path).await?.map(|(_, hash)| hash);
        match (pending.base, current) {
            (Expect::Hash(base), Some(current)) if base == current => {}
            (Expect::Absent, None) => {}
            _ => {
                let path = pending.path.clone();
                self.stale(&path);
                return Err(Errno::CANCELED);
            }
        }
        pending.buf.resize(size, 0);
        pending.dirty = true;
        if let Some(stored) = &handle.pending_size {
            stored.store(size as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}
