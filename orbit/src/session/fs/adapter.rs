//! Тонкая прослойка между fuser и ядром [`Vfs`]: разбирает аргументы колбэка и отвечает из задачи tokio.
//!
//! Колбэки fuser синхронные, а ждать runner долго. Поэтому колбэк кладёт запрос в задачу tokio и возвращается;
//! `Reply*` — `Send`, и задача отвечает ядру сама. Ожидание одного запроса не задерживает остальные.
//!
//! Время жизни атрибутов и записей для ядра — 0: иначе ядро отвечало бы агенту из своего кэша, не спрашивая ФС, и
//! проверки версий файлов не срабатывали бы. Частые `lookup` и `getattr` отвечает кэш метаданных [`Vfs`].

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
};
use nerve::protocol::{Kind, Root};
use rustix::fs::OFlags;
use tokio::runtime::Handle;

use super::inodes::Owner;
use super::vfs::{Entry, SetAttr, Vfs};

#[cfg(test)]
#[path = "../../../tests/session/fs/adapter.rs"]
mod tests;

const TTL: Duration = Duration::ZERO;

struct Adapter {
    vfs: Arc<Vfs>,
    runtime: Handle,
}

/// Монтирует ФС в `mountpoint` от имени текущего процесса (root) с `allow_other`: ходить в неё будет другой
/// пользователь. Колбэки отвечают из задач текущего runtime tokio. Размонтирование — `drop` результата.
pub(crate) fn mount(vfs: Arc<Vfs>, mountpoint: &Path, root: Root) -> io::Result<BackgroundSession> {
    let mut config = Config::default();
    config.acl = SessionACL::All;
    config.mount_options = mount_options(root);
    let adapter = Adapter {
        vfs,
        runtime: Handle::current(),
    };
    fuser::spawn_mount(adapter, mountpoint, &config)
}

/// Права проектной ФС проверяет ядро по режиму и владельцу файла. В `$HOME` runner — только runner, выполняя
/// операции своим пользователем: `$HOME` обычно `0700` и принадлежит пользователю агента, а session работает от
/// root без `CAP_DAC_OVERRIDE`, и ядро не пустило бы её создать и смонтировать проект внутри `$HOME`. Агент и так
/// видит всё, что видит этот пользователь.
fn mount_options(root: Root) -> Vec<MountOption> {
    let mut options = vec![
        MountOption::FSName("iapetus".to_owned()),
        MountOption::Subtype("orbit".to_owned()),
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    if root == Root::Project {
        options.push(MountOption::DefaultPermissions);
    }
    options
}

fn errno(error: rustix::io::Errno) -> Errno {
    Errno::from_i32(error.raw_os_error())
}

fn file_type(kind: Kind) -> FileType {
    match kind {
        Kind::Dir => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
        Kind::File | Kind::Other => FileType::RegularFile,
    }
}

fn file_attr(entry: &Entry) -> FileAttr {
    let attr = entry.attr;
    let mtime = UNIX_EPOCH + Duration::from_nanos(u64::try_from(attr.mtime_ns).unwrap_or(0));
    // Сокеты и каналы с runner агенту не нужны: без прав открыть их не получится.
    let mode = if attr.kind == Kind::Other { 0 } else { attr.mode };
    FileAttr {
        ino: INodeNo(entry.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: UNIX_EPOCH,
        kind: file_type(attr.kind),
        perm: u16::try_from(mode & 0o7777).unwrap_or(0),
        nlink: if attr.kind == Kind::Dir { 2 } else { 1 },
        uid: entry.owner.uid,
        gid: entry.owner.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn open_flags(raw: i32) -> OFlags {
    OFlags::from_bits_retain(raw.cast_unsigned())
}

fn time_ns(time: TimeOrNow) -> i64 {
    let time = match time {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Имя из колбэка. Имя не UTF-8 в протокол не помещается: такого файла для агента нет.
fn name_of(name: &OsStr) -> Option<String> {
    name.to_str().map(str::to_owned)
}

impl Adapter {
    /// Запускает задачу и заодно запускает очистку кэша, если пора.
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if self.vfs.sweep_due() {
            let vfs = Arc::clone(&self.vfs);
            drop(self.runtime.spawn_blocking(move || {
                let removed = vfs.sweep();
                if removed > 0 {
                    tracing::debug!(removed, "очищен кэш `/work`");
                }
            }));
        }
        drop(self.runtime.spawn(task));
    }

    fn reply_entry(entry: &Entry, reply: ReplyEntry) {
        reply.entry(&TTL, &file_attr(entry), Generation(0));
    }
}

impl Filesystem for Adapter {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // `O_TRUNC` приходит в `open` вместе с флагами: проверка версии на `open` с обрезкой видит и этот случай, а не
        // отдельный `setattr size` перед ним.
        if let Err(missing) = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC) {
            tracing::warn!(?missing, "ядро не поддерживает атомарный O_TRUNC");
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name_of(name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.lookup(parent.0, &name).await {
                Ok(entry) => Self::reply_entry(&entry, reply),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.vfs.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.getattr(ino.0, fh.map(|fh| fh.0)).await {
                Ok(entry) => reply.attr(&TTL, &file_attr(&entry)),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let change = SetAttr {
            mode,
            uid,
            gid,
            size,
            mtime_ns: mtime.map(time_ns),
            atime: atime.is_some(),
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.setattr(ino.0, fh.map(|fh| fh.0), change).await {
                Ok(entry) => reply.attr(&TTL, &file_attr(&entry)),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.readlink(ino.0).await {
                Ok(target) => reply.data(target.as_bytes()),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(name) = name_of(name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let caller = Owner {
            uid: req.uid(),
            gid: req.gid(),
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.mkdir(parent.0, &name, caller).await {
                Ok(entry) => Self::reply_entry(&entry, reply),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name_of(name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.unlink(parent.0, &name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name_of(name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.rmdir(parent.0, &name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(new_name)) = (name_of(name), name_of(new_name)) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.rename(parent.0, &name, new_parent.0, &new_name, flags.bits()).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.open(ino.0, open_flags(flags.0)).await {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, flags: i32, reply: ReplyCreate) {
        let Some(name) = name_of(name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let caller = Owner {
            uid: req.uid(),
            gid: req.gid(),
        };
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.create(parent.0, &name, mode, umask, open_flags(flags), caller).await {
                Ok((entry, fh)) => reply.created(&TTL, &file_attr(&entry), Generation(0), FileHandle(fh), FopenFlags::empty()),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.read(fh.0, offset, size).await {
                Ok(bytes) => reply.data(&bytes),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let vfs = Arc::clone(&self.vfs);
        let data = data.to_vec();
        self.spawn(async move {
            match vfs.write(fh.0, offset, &data).await {
                Ok(written) => reply.written(u32::try_from(written).unwrap_or(u32::MAX)),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.flush(fh.0).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.flush(fh.0).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            vfs.release(fh.0).await;
            reply.ok();
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let vfs = Arc::clone(&self.vfs);
        self.spawn(async move {
            match vfs.open_dir(ino.0).await {
                Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
                Err(error) => reply.error(errno(error)),
            }
        });
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        match self.vfs.read_dir(fh.0, offset) {
            Ok(items) => {
                for (index, item) in items.iter().enumerate() {
                    let next = (offset + index + 1) as u64;
                    if reply.add(INodeNo(item.ino), next, file_type(item.kind), &item.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(error) => reply.error(errno(error)),
        }
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.vfs.release_dir(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Свободного места у ФС нет в смысле диска: агенту нужен ответ, при котором запись не отклоняется заранее.
        reply.statfs(1 << 20, 1 << 19, 1 << 19, 1 << 16, 1 << 15, 4096, 255, 4096);
    }
}
