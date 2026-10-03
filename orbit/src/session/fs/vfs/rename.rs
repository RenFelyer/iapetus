//! `rename`: запись временного файла поверх цели.

use nerve::protocol::{Error, Expect, FsReply, FsRequest, Kind};
use rustix::io::Errno;

use super::{FsResult, Intent, Known, Seen, Vfs, unexpected, write_errno};
use crate::session::fs::inodes::{Ino, join};

impl Vfs {
    /// Переименовывает временный файл сессии. Поверх файла runner это запись: runner получает содержимое с ожиданием
    /// версии, которую видел агент. Если там другая версия, ответ — `ECANCELED`, временный файл остаётся на месте, а
    /// файл runner не меняется.
    ///
    /// Файл runner (не временный) переносится операцией `Rename`; каталоги и всё, кроме обычных файлов, — `ENOTSUP`.
    pub(crate) async fn rename(&self, parent: Ino, name: &str, new_parent: Ino, new_name: &str, flags: u32) -> FsResult<()> {
        if flags != 0 {
            return Err(Errno::NOTSUP);
        }
        let from = join(&self.path_of(parent)?, name);
        let to = join(&self.path_of(new_parent)?, new_name);
        if from == to {
            return Ok(());
        }
        if self.is_hidden(&from) || self.is_hidden(&to) {
            return Err(Errno::NOENT);
        }
        self.reject_own_link(&from)?;
        self.reject_own_link(&to)?;
        let (from_is_local, to_is_local) = {
            let state = self.lock();
            let is_local = |path: &str| state.inodes.at(path).is_some_and(|node| node.local.is_some());
            (is_local(&from), is_local(&to))
        };
        if !from_is_local {
            if to_is_local {
                return Err(Errno::NOTSUP);
            }
            return self.rename_on_runner(&from, &to).await;
        }
        if to_is_local {
            self.lock().inodes.move_over(&from, &to);
            return Ok(());
        }
        if self
            .lock()
            .meta
            .last_known(&to)
            .and_then(Known::attr)
            .is_some_and(|attr| attr.kind == Kind::Dir)
        {
            return Err(Errno::ISDIR);
        }
        let base = self.write_base(&to).await?;
        let content = {
            let state = self.lock();
            let local = state.inodes.at(&from).and_then(|node| node.local.as_ref());
            local.map(|local| local.content.clone()).ok_or(Errno::NOENT)?
        };
        self.push(&to, base, &content).await?;
        let mut state = self.lock();
        state.inodes.move_over(&from, &to);
        if let Some(node) = state.inodes.at_mut(&to) {
            node.local = None;
        }
        Ok(())
    }

    /// Переносит файл runner. Runner сверяет хеш источника и цели: источник — версия, которую видел агент (или та, что
    /// сейчас на runner, если агент файла не читал), цель — как при записи. Любое расхождение — `ECANCELED`, и ничего
    /// не меняется. Ни `EPERM`, ни `EBUSY`, ни `EEXIST`, ни `EXDEV` наружу не уходят.
    async fn rename_on_runner(&self, from: &str, to: &str) -> FsResult<()> {
        let source = self.resolve(from).await?.ok_or(Errno::NOENT)?;
        if source.kind != Kind::File {
            return Err(Errno::NOTSUP);
        }
        if self.resolve(to).await?.is_some_and(|attr| attr.kind == Kind::Dir) {
            return Err(Errno::ISDIR);
        }
        let seen = self.lock().cache.get(from).map(|entry| entry.seen);
        let expect_from = match seen {
            Some(Seen::Hash(hash)) => hash,
            Some(Seen::Absent) => {
                // Агент видел, что файла нет, а он появился: версии, которую можно переносить, у агента нет.
                self.stale(from);
                return Err(Errno::CANCELED);
            }
            None => self.runner_hash(from).await?.ok_or(Errno::NOENT)?.1,
        };
        let expect_to = self.write_base(to).await?;
        let epoch = self.epoch.current();
        let request = || FsRequest::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
            expect_from,
            expect_to,
        };
        let intent = Intent::Rename {
            from,
            to,
            from_hash: expect_from,
            expect_to,
        };
        let response = self.call_settled(request, intent).await;
        match response {
            Ok(FsReply::Renamed(attr)) => {
                let mut state = self.lock();
                state.meta.put(to, Some(attr), epoch);
                state.meta.put(from, None, epoch);
                state.cache.rename(from, to, expect_from, attr, epoch);
                state.inodes.move_over(from, to);
                Ok(())
            }
            Ok(other) => Err(unexpected(&other)),
            Err(error) => {
                if matches!(error, Error::Conflict | Error::NotFound) {
                    self.stale(from);
                    self.stale(to);
                }
                Err(write_errno(&error))
            }
        }
    }

    /// Версия цели, на которую рассчитывает запись: та, что видел агент. Если агент цель не видел, берётся то, что на
    /// runner сейчас: перезаписывать нечего, кроме того, о чём агент не знал.
    async fn write_base(&self, path: &str) -> FsResult<Expect> {
        let seen = self.lock().cache.get(path).map(|entry| entry.seen);
        match seen {
            Some(Seen::Hash(hash)) => Ok(Expect::Hash(hash)),
            Some(Seen::Absent) => Ok(Expect::Absent),
            None => Ok(self.runner_hash(path).await?.map_or(Expect::Absent, |(_, hash)| Expect::Hash(hash))),
        }
    }
}
