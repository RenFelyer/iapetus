//! Имена: `lookup`, атрибуты, каталоги, `mkdir`, удаление.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use nerve::protocol::{Attr, FsReply, FsRequest, Kind};
use rustix::io::Errno;

use super::{Entry, Fh, FsResult, Intent, Open, State, Vfs, errno, local_attr, now_ns, unexpected, write_errno};
use crate::session::fs::inodes::{Ino, Owner, ROOT, join, parent_of};

/// Запись открытого каталога.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirItem {
    pub(crate) name: String,
    pub(crate) ino: Ino,
    pub(crate) kind: Kind,
}

/// Что меняет `setattr`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SetAttr {
    pub(crate) mode: Option<u32>,
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
    pub(crate) size: Option<u64>,
    /// Новое время изменения, наносекунды от начала эпохи.
    pub(crate) mtime_ns: Option<i64>,
    /// Меняется время доступа. Своего времени доступа у файлов нет, поэтому значение не хранится.
    pub(crate) atime: bool,
}

impl State {
    /// Размер буфера, в который агент пишет на месте: пока он не ушёл на runner, ответ `getattr` должен его показать.
    fn pending_size(&self, ino: Ino) -> Option<u64> {
        self.handles
            .values()
            .filter(|handle| handle.ino == ino)
            .find_map(|handle| handle.pending_size.as_ref().map(|size| size.load(Ordering::Relaxed)))
    }
}

impl Vfs {
    pub(crate) async fn lookup(&self, parent: Ino, name: &str) -> FsResult<Entry> {
        let path = join(&self.path_of(parent)?, name);
        if self.is_hidden(&path) {
            return Err(Errno::NOENT);
        }
        let attr = self.resolve(&path).await?;
        self.observe(&path, attr.as_ref());
        let attr = attr.ok_or(Errno::NOENT)?;
        let mut state = self.lock();
        let ino = state.inodes.lookup(&path);
        Ok(self.entry(&state, ino, attr))
    }

    /// Цель символической ссылки. Цель кэшируется на эпоху: ссылка заменена — атрибуты другие, и прочитанная цель
    /// перестаёт годиться.
    pub(crate) async fn readlink(&self, ino: Ino) -> FsResult<String> {
        let path = self.path_of(ino)?;
        if let Some(target) = self.own_link(&path) {
            return Ok(target.to_owned());
        }
        let epoch = self.epoch.current();
        if let Some(target) = self.lock().meta.link(&path, epoch) {
            return Ok(target.to_owned());
        }
        let Some(attr) = self.resolve(&path).await? else {
            return Err(Errno::NOENT);
        };
        if attr.kind != Kind::Symlink {
            return Err(Errno::INVAL);
        }
        match self.call(FsRequest::ReadLink { path: path.clone() }).await {
            Ok(FsReply::Link(target)) => {
                self.lock().meta.put_link(&path, attr, target.clone(), epoch);
                Ok(target)
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(errno(&error)),
        }
    }

    pub(crate) fn forget(&self, ino: Ino, count: u64) {
        self.lock().inodes.forget(ino, count);
    }

    pub(crate) async fn getattr(&self, ino: Ino, fh: Option<Fh>) -> FsResult<Entry> {
        let path = self.lock().inodes.path(ino).map(str::to_owned);
        let Some(path) = path else {
            return self.attr_of_unlinked(ino, fh);
        };
        let attr = self.resolve(&path).await?;
        self.observe(&path, attr.as_ref());
        let mut attr = attr.ok_or(Errno::NOENT)?;
        let state = self.lock();
        if let Some(size) = state.pending_size(ino) {
            attr.size = size;
        }
        Ok(self.entry(&state, ino, attr))
    }

    /// Атрибуты файла, который удалили или заменили при открытом дескрипторе: открытый файл продолжает работать.
    fn attr_of_unlinked(&self, ino: Ino, fh: Option<Fh>) -> FsResult<Entry> {
        let state = self.lock();
        let node = state.inodes.node(ino).ok_or(Errno::STALE)?;
        let attr = match (&node.local, fh.and_then(|fh| state.handles.get(&fh))) {
            (Some(local), _) => local_attr(local),
            (None, Some(handle)) => {
                let size = match handle.pending_size.as_ref() {
                    Some(size) => size.load(Ordering::Relaxed),
                    None => match handle.kind.try_lock().map_err(|_| Errno::NOENT)?.cached_len() {
                        Some(len) => len,
                        None => return Err(Errno::NOENT),
                    },
                };
                Attr {
                    kind: Kind::File,
                    size,
                    mtime_ns: now_ns(),
                    mode: 0o644,
                }
            }
            (None, None) => return Err(Errno::NOENT),
        };
        Ok(self.entry(&state, ino, attr))
    }

    pub(crate) async fn setattr(&self, ino: Ino, fh: Option<Fh>, change: SetAttr) -> FsResult<Entry> {
        let (path, is_local) = {
            let state = self.lock();
            let node = state.inodes.node(ino).ok_or(Errno::STALE)?;
            (node.path.clone(), node.local.is_some())
        };
        if is_local {
            return self.setattr_local(ino, change);
        }
        let path = path.ok_or(Errno::STALE)?;
        // Права, владельца и время файла runner менять нельзя: ENOTSUP агенты не считают ошибкой записи. Время вместе
        // с размером — это `ftruncate`, который ядро сопровождает временем изменения.
        if change.mode.is_some()
            || change.uid.is_some()
            || change.gid.is_some()
            || (change.size.is_none() && (change.atime || change.mtime_ns.is_some()))
        {
            return Err(Errno::NOTSUP);
        }
        if let Some(size) = change.size {
            self.set_size(&path, fh, size).await?;
        }
        self.getattr(ino, fh).await
    }

    fn setattr_local(&self, ino: Ino, change: SetAttr) -> FsResult<Entry> {
        let mut state = self.lock();
        let node = state.inodes.node_mut(ino).ok_or(Errno::STALE)?;
        if let Some(size) = change.size {
            let size = usize::try_from(size).map_err(|_| Errno::FBIG)?;
            if size > self.config.max_file {
                return Err(Errno::FBIG);
            }
            if let Some(local) = node.local.as_mut() {
                local.content.resize(size, 0);
                local.mtime_ns = now_ns();
            }
        }
        if let Some(local) = node.local.as_mut() {
            if let Some(mode) = change.mode {
                local.mode = mode & 0o7777;
            }
            if let Some(mtime) = change.mtime_ns {
                local.mtime_ns = mtime;
            }
        }
        if change.uid.is_some() || change.gid.is_some() {
            let current = node.owner.unwrap_or(self.config.owner);
            node.owner = Some(Owner {
                uid: change.uid.unwrap_or(current.uid),
                gid: change.gid.unwrap_or(current.gid),
            });
        }
        let attr = node.local.as_ref().map(local_attr).ok_or(Errno::STALE)?;
        Ok(self.entry(&state, ino, attr))
    }

    /// Открывает каталог: список снимается сразу, чтобы чтение по частям не зависело от изменений между вызовами.
    pub(crate) async fn open_dir(&self, ino: Ino) -> FsResult<Fh> {
        let path = self.path_of(ino)?;
        match self.resolve(&path).await? {
            Some(attr) if attr.kind == Kind::Dir => {}
            Some(_) => return Err(Errno::NOTDIR),
            None => return Err(Errno::NOENT),
        }
        let epoch = self.epoch.current();
        let cached = self.lock().meta.listing(&path, epoch);
        let entries = match cached {
            Some(entries) => entries,
            None => match self.call(FsRequest::ReadDir { path: path.clone() }).await {
                Ok(FsReply::ReadDir { entries, watched }) => {
                    self.lock().meta.put_listing(&path, &entries, epoch, watched);
                    entries
                }
                Ok(other) => return Err(unexpected(&other)),
                Err(error) => return Err(errno(&error)),
            },
        };
        let mut state = self.lock();
        // Имена на runner и временные файлы сессии: временный файл с тем же именем заслоняет файл runner.
        let mut names: BTreeMap<String, Kind> = entries.into_iter().map(|entry| (entry.name, entry.attr.kind)).collect();
        for (name, _) in state.inodes.local_children(&path) {
            names.insert(name, Kind::File);
        }
        if path.is_empty() {
            for link in &self.config.links {
                names.insert(link.name.clone(), Kind::Symlink);
            }
        }
        let parent = if path.is_empty() {
            ROOT
        } else {
            state.inodes.ino_for(parent_of(&path))
        };
        let mut items = vec![
            DirItem {
                name: ".".to_owned(),
                ino,
                kind: Kind::Dir,
            },
            DirItem {
                name: "..".to_owned(),
                ino: parent,
                kind: Kind::Dir,
            },
        ];
        names.retain(|name, _| !self.is_hidden(&join(&path, name)));
        for (name, kind) in names {
            let child = state.inodes.ino_for(&join(&path, &name));
            items.push(DirItem { name, ino: child, kind });
        }
        let fh = state.next_fh;
        state.next_fh += 1;
        state.dirs.insert(fh, Arc::from(items));
        Ok(fh)
    }

    /// Записи каталога начиная с `offset`.
    pub(crate) fn read_dir(&self, fh: Fh, offset: usize) -> FsResult<Vec<DirItem>> {
        let state = self.lock();
        let items = state.dirs.get(&fh).ok_or(Errno::BADF)?;
        Ok(items.iter().skip(offset).cloned().collect())
    }

    pub(crate) fn release_dir(&self, fh: Fh) {
        self.lock().dirs.remove(&fh);
    }

    pub(crate) async fn mkdir(&self, parent: Ino, name: &str, caller: Owner) -> FsResult<Entry> {
        let path = join(&self.path_of(parent)?, name);
        self.reject_own_link(&path)?;
        if self.lock().inodes.at(&path).is_some_and(|node| node.local.is_some()) {
            return Err(Errno::EXIST);
        }
        let epoch = self.epoch.current();
        match self.call(FsRequest::MakeDir { path: path.clone() }).await {
            Ok(FsReply::MadeDir(attr)) => {
                let mut state = self.lock();
                state.meta.put(&path, Some(attr), epoch);
                state.cache.forget(&path);
                let ino = state.inodes.lookup(&path);
                if let Some(node) = state.inodes.node_mut(ino) {
                    node.owner = Some(caller);
                }
                Ok(self.entry(&state, ino, attr))
            }
            // Путь занят: для `mkdir` это `EEXIST`, как у любой файловой системы.
            Err(nerve::protocol::Error::Conflict) => Err(Errno::EXIST),
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(write_errno(&error)),
        }
    }

    pub(crate) async fn unlink(&self, parent: Ino, name: &str) -> FsResult<()> {
        let path = join(&self.path_of(parent)?, name);
        self.reject_own_link(&path)?;
        {
            let mut state = self.lock();
            if state.inodes.at(&path).is_some_and(|node| node.local.is_some()) {
                state.inodes.orphan(&path);
                return Ok(());
            }
        }
        self.remove_on_runner(&path).await
    }

    pub(crate) async fn rmdir(&self, parent: Ino, name: &str) -> FsResult<()> {
        let path = join(&self.path_of(parent)?, name);
        self.reject_own_link(&path)?;
        self.remove_on_runner(&path).await
    }

    async fn remove_on_runner(&self, path: &str) -> FsResult<()> {
        let epoch = self.epoch.current();
        let seen = self.lock().cache.get(path).map(|entry| entry.seen);
        let request = || FsRequest::Remove { path: path.to_owned() };
        let removed = self.call_settled(request, Intent::Remove { path, seen }).await;
        let mut state = self.lock();
        match removed {
            Ok(FsReply::Removed) | Err(nerve::protocol::Error::NotFound) => {
                state.meta.remove_tree(path, epoch);
                state.cache.mark_absent(path, epoch);
                state.inodes.orphan(path);
                if removed.is_ok() { Ok(()) } else { Err(Errno::NOENT) }
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(write_errno(&error)),
        }
    }
}

impl Open {
    /// Размер кэш-файла у открытого на чтение дескриптора.
    pub(super) fn cached_len(&self) -> Option<u64> {
        match self {
            Self::Cached(file) => file.metadata().ok().map(|m| m.len()),
            _ => None,
        }
    }
}
