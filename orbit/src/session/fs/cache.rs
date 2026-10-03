//! Кэш содержимого в каталоге `/work` и версии файлов, которые видел агент.
//!
//! `/work` — плоский список файлов с именами `sha256(<fingerprint runner>:<полный путь>)` и исходным расширением.
//! Версия, которую видел агент, — основа ожидания при записи: запись проходит, только если на runner всё ещё она.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nerve::protocol::Attr;
use sha2::{Digest, Sha256};

use super::inodes::parent_of;
use tokio::time::Instant;

/// Сколько файл может лежать в кэше без обращений.
pub(super) const MAX_IDLE: Duration = Duration::from_hours(2);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Версия файла, которую агент видел последней.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    Hash([u8; 32]),
    /// Агент узнал, что файла нет.
    Absent,
}

#[derive(Debug)]
pub(super) struct Entry {
    pub(super) seen: Seen,
    /// Копия содержимого; нет у [`Seen::Absent`], после очистки и если копию не удалось записать или перенести.
    pub(super) file: Option<Arc<File>>,
    /// Эпоха, в которой версию сверили с runner; `None` — сверить при следующем обращении.
    pub(super) checked: Option<u64>,
    /// Атрибуты runner в момент, когда копия совпадала с файлом: если кэш метаданных знает другие, файл изменился.
    pub(super) attr: Option<Attr>,
    last_used: Instant,
}

#[derive(Debug)]
pub(super) struct ContentCache {
    dir: PathBuf,
    fingerprint: String,
    /// Абсолютный путь точки монтирования: имена кэш-файлов строятся от полного пути, который видит агент.
    root: PathBuf,
    entries: HashMap<String, Entry>,
}

impl ContentCache {
    pub(super) fn new(dir: PathBuf, fingerprint: String, root: PathBuf) -> Self {
        Self {
            dir,
            fingerprint,
            root,
            entries: HashMap::new(),
        }
    }

    /// Имя кэш-файла: хеш от fingerprint и полного пути плюс исходное расширение.
    pub(super) fn file_name(&self, path: &str) -> String {
        file_name(&self.fingerprint, &self.root, path)
    }

    pub(super) fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    pub(super) fn touch(&mut self, path: &str) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.last_used = Instant::now();
        }
    }

    /// Записывает копию в `/work` и запоминает версию. Старый кэш-файл заменяется атомарно: открытые дескрипторы
    /// дочитывают прежнюю версию.
    pub(super) fn store(&mut self, path: &str, seen: Seen, content: Option<&[u8]>, attr: Attr, epoch: u64) -> io::Result<()> {
        let file = content.map(|bytes| self.write_file(path, bytes)).transpose()?;
        self.entries.insert(
            path.to_owned(),
            Entry {
                seen,
                file: file.map(Arc::new),
                checked: Some(epoch),
                attr: Some(attr),
                last_used: Instant::now(),
            },
        );
        Ok(())
    }

    /// Запоминает версию без копии: файла нет, или копию не удалось перенести вместе с файлом.
    pub(super) fn note(&mut self, path: &str, seen: Seen, epoch: u64) {
        self.entries.insert(
            path.to_owned(),
            Entry {
                seen,
                file: None,
                checked: Some(epoch),
                attr: None,
                last_used: Instant::now(),
            },
        );
    }

    fn write_file(&self, path: &str, bytes: &[u8]) -> io::Result<File> {
        let temp = self.dir.join(format!(".tmp-{}", TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let mut file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&temp)?;
        let written = file
            .write_all(bytes)
            .and_then(|()| fs::rename(&temp, self.dir.join(self.file_name(path))));
        if let Err(error) = written {
            // Остался бы файл, который никто не вспомнит.
            fs::remove_file(&temp).unwrap_or_default();
            return Err(error);
        }
        Ok(file)
    }

    /// Файл перенесли на runner: версия `sha256` теперь лежит под путём `to`, а `from` пуст. Копия переезжает вместе с
    /// файлом, поэтому следующее чтение не скачивает его заново.
    pub(super) fn rename(&mut self, from: &str, to: &str, sha256: [u8; 32], attr: Attr, epoch: u64) {
        let copy = self
            .entries
            .get(from)
            .filter(|entry| entry.seen == Seen::Hash(sha256))
            .and_then(|entry| entry.file.clone());
        self.forget(to);
        let moved = copy.is_some_and(|file| {
            let renamed = fs::rename(self.dir.join(self.file_name(from)), self.dir.join(self.file_name(to)));
            if let Err(error) = &renamed {
                tracing::warn!(from, to, %error, "кэш-файл не перенесён");
            }
            renamed.is_ok() && {
                self.entries.insert(
                    to.to_owned(),
                    Entry {
                        seen: Seen::Hash(sha256),
                        file: Some(file),
                        checked: Some(epoch),
                        attr: Some(attr),
                        last_used: Instant::now(),
                    },
                );
                true
            }
        });
        if !moved {
            self.note(to, Seen::Hash(sha256), epoch);
        }
        self.forget(from);
    }

    /// Агент узнал, что файла нет: копия не нужна, а версия — «файла нет».
    pub(super) fn mark_absent(&mut self, path: &str, epoch: u64) {
        self.forget(path);
        self.note(path, Seen::Absent, epoch);
    }

    /// Версию сверили с runner в эпохе `epoch`: она та же, а атрибуты теперь `attr`.
    pub(super) fn mark_checked(&mut self, path: &str, epoch: u64, attr: Attr) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.checked = Some(epoch);
            entry.attr = Some(attr);
        }
    }

    /// Runner сообщил об изменении: при следующем обращении версию нужно сверить, даже в той же эпохе.
    pub(super) fn invalidate_check(&mut self, path: &str) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.checked = None;
        }
    }

    /// Каталог больше не наблюдается: копиям его файлов нельзя верить, пока их версию не сверят.
    pub(super) fn invalidate_children(&mut self, dir: &str) {
        for (path, entry) in &mut self.entries {
            if parent_of(path) == dir && !path.is_empty() {
                entry.checked = None;
            }
        }
    }

    pub(super) fn forget(&mut self, path: &str) {
        if let Some(entry) = self.entries.remove(path)
            && entry.file.is_some()
        {
            fs::remove_file(self.dir.join(self.file_name(path))).unwrap_or_default();
        }
    }

    /// Абсолютный путь точки монтирования.
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// Переходит к другой точке монтирования: имена файлов строятся от неё, поэтому копии и версии прежней
    /// забываются.
    pub(super) fn set_root(&mut self, root: PathBuf) {
        for (path, entry) in &self.entries {
            if entry.file.is_some() {
                fs::remove_file(self.dir.join(file_name(&self.fingerprint, &self.root, path))).unwrap_or_default();
            }
        }
        self.entries.clear();
        self.root = root;
    }

    /// Переходит к другому runner: копии файлов прежнего удаляются, сверка версий начинается заново с новой эпохой.
    /// Версии, которые видел агент, остаются основой записи: запись, рассчитанная на файл прежнего runner, получит
    /// `ECANCELED`, если на новом лежит другой. Открытые файлы читаются до закрытия.
    pub(super) fn switch_runner(&mut self, fingerprint: String) {
        for (path, entry) in &mut self.entries {
            if entry.file.take().is_some() {
                fs::remove_file(self.dir.join(file_name(&self.fingerprint, &self.root, path))).unwrap_or_default();
            }
        }
        self.fingerprint = fingerprint;
    }

    /// Удаляет копии файлов, к которым не обращались дольше [`MAX_IDLE`] и которые никто не держит открытыми.
    /// Возвращает число удалённых. Запись о версии, которую видел агент, остаётся: без неё запись рассчитывала бы на то,
    /// что лежит на runner сейчас, и молча затёрла бы чужую правку. Следующее чтение скачает файл заново.
    pub(super) fn sweep(&mut self) -> usize {
        let mut removed = 0;
        for (path, entry) in &mut self.entries {
            let idle = entry.last_used.elapsed() > MAX_IDLE;
            if idle && entry.file.as_ref().is_some_and(|file| Arc::strong_count(file) == 1) {
                entry.file = None;
                let name = self.dir.join(file_name(&self.fingerprint, &self.root, path));
                fs::remove_file(name).unwrap_or_default();
                removed += 1;
            }
        }
        removed
    }
}

/// Имя кэш-файла пути `path` под точкой монтирования `root` у runner `fingerprint`.
fn file_name(fingerprint: &str, root: &Path, path: &str) -> String {
    let full = root.join(path);
    let digest = Sha256::digest(format!("{fingerprint}:{}", full.display()));
    let mut name: String = digest
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect();
    let extension = Path::new(path).extension().and_then(|e| e.to_str());
    if let Some(extension) = extension.filter(|e| e.bytes().all(|b| b.is_ascii_alphanumeric())) {
        name.push('.');
        name.push_str(extension);
    }
    name
}

#[cfg(test)]
#[path = "../../../tests/session/fs/cache.rs"]
mod tests;
