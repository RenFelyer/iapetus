//! Документы: файлы `*.md` каталога `docs/`, которые кладёт человек на хосте server. Session подписывается и
//! получает снимок, затем изменения. Server только читает: worker документы не пишет.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nerve::protocol::{DocsEvent, Document};

/// Документ больше этого размера не рассылается.
const MAX_DOCUMENT: u64 = 1 << 20;

/// Как часто подписка сверяет каталог с тем, что отправила.
const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub(in crate::server) struct Docs {
    dir: PathBuf,
}

/// Подписка session: снимок каталога и изменения после него.
#[derive(Debug)]
pub(in crate::server) struct DocsSubscription {
    dir: PathBuf,
    known: BTreeMap<String, String>,
    pending: VecDeque<DocsEvent>,
}

impl Docs {
    pub(in crate::server) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Подписка: `Snapshot` отдаётся первым событием, дальше — `Updated` и `Removed`.
    pub(in crate::server) fn subscribe(&self) -> DocsSubscription {
        let known = read_documents(&self.dir);
        let snapshot = known
            .iter()
            .map(|(name, content)| Document {
                name: name.clone(),
                content: content.clone(),
            })
            .collect();
        DocsSubscription {
            dir: self.dir.clone(),
            known,
            pending: VecDeque::from([DocsEvent::Snapshot(snapshot)]),
        }
    }
}

impl DocsSubscription {
    /// Следующее событие; ждёт, пока каталог не изменится.
    pub(in crate::server) async fn next(&mut self) -> DocsEvent {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return event;
            }
            tokio::time::sleep(POLL).await;
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        let current = read_documents(&self.dir);
        for name in self.known.keys().filter(|name| !current.contains_key(*name)) {
            self.pending.push_back(DocsEvent::Removed { name: name.clone() });
        }
        for (name, content) in &current {
            if self.known.get(name) != Some(content) {
                self.pending.push_back(DocsEvent::Updated(Document {
                    name: name.clone(),
                    content: content.clone(),
                }));
            }
        }
        self.known = current;
    }
}

/// Читаемые документы каталога: обычные файлы `*.md` до 1 мебибайта в `UTF-8`. Остальное пропускается, в журнал
/// попадает только имя. Нет каталога — документов нет.
fn read_documents(dir: &Path) -> BTreeMap<String, String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    let mut documents = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_DOCUMENT => {}
            Ok(meta) if meta.is_file() => {
                tracing::warn!(%name, "документ больше 1 МиБ не рассылается");
                continue;
            }
            _ => continue,
        }
        if let Ok(content) = fs::read_to_string(&path) {
            documents.insert(name, content);
        } else {
            tracing::warn!(%name, "документ не прочитан или не в UTF-8");
        }
    }
    documents
}

#[cfg(test)]
#[path = "../../../tests/server/control/docs.rs"]
mod tests;
