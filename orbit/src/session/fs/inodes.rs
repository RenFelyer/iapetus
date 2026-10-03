//! Таблица inode: путь ↔ номер. Путь — относительно рабочего каталога, с `/`, у корня пустой.

use std::collections::HashMap;

pub(super) type Ino = u64;

pub(super) const ROOT: Ino = 1;

/// Владелец, от имени которого файл показан агенту.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// Файл, который есть только в сессии: временный файл агента до `rename` поверх цели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalFile {
    pub(super) content: Vec<u8>,
    pub(super) mode: u32,
    pub(super) mtime_ns: i64,
}

#[derive(Debug)]
pub(super) struct Node {
    /// `None` — путь удалён или занят другим файлом; открытые дескрипторы продолжают работать.
    pub(super) path: Option<String>,
    /// Сколько ссылок на inode держит ядро: `lookup` прибавляет, `forget` убавляет.
    pub(super) nlookup: u64,
    /// Кто создал файл через ФС; у остальных — владелец по умолчанию.
    pub(super) owner: Option<Owner>,
    pub(super) local: Option<LocalFile>,
}

#[derive(Debug)]
pub(super) struct Inodes {
    nodes: HashMap<Ino, Node>,
    by_path: HashMap<String, Ino>,
    next: Ino,
}

/// Путь ребёнка каталога `parent`.
pub(super) fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Каталог, в котором лежит `path`; для файла в корне — пустой путь.
pub(super) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Последняя составляющая пути.
pub(super) fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

impl Inodes {
    pub(super) fn new() -> Self {
        let root = Node {
            path: Some(String::new()),
            nlookup: 1,
            owner: None,
            local: None,
        };
        Self {
            nodes: HashMap::from([(ROOT, root)]),
            by_path: HashMap::from([(String::new(), ROOT)]),
            next: ROOT + 1,
        }
    }

    pub(super) fn node(&self, ino: Ino) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    pub(super) fn node_mut(&mut self, ino: Ino) -> Option<&mut Node> {
        self.nodes.get_mut(&ino)
    }

    pub(super) fn path(&self, ino: Ino) -> Option<&str> {
        self.nodes.get(&ino)?.path.as_deref()
    }

    pub(super) fn at(&self, path: &str) -> Option<&Node> {
        self.nodes.get(self.by_path.get(path)?)
    }

    pub(super) fn at_mut(&mut self, path: &str) -> Option<&mut Node> {
        self.nodes.get_mut(self.by_path.get(path)?)
    }

    /// Номер inode пути; заводит запись, если её нет.
    pub(super) fn ino_for(&mut self, path: &str) -> Ino {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.to_owned(), ino);
        self.nodes.insert(
            ino,
            Node {
                path: Some(path.to_owned()),
                nlookup: 0,
                owner: None,
                local: None,
            },
        );
        ino
    }

    /// Номер inode пути для ответа ядру на `lookup`, `create` или `mkdir`: ядро получает ссылку, которую вернёт `forget`.
    pub(super) fn lookup(&mut self, path: &str) -> Ino {
        let ino = self.ino_for(path);
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.nlookup += 1;
        }
        ino
    }

    pub(super) fn forget(&mut self, ino: Ino, count: u64) {
        if ino == ROOT {
            return;
        }
        let Some(node) = self.nodes.get_mut(&ino) else { return };
        node.nlookup = node.nlookup.saturating_sub(count);
        self.drop_if_unused(ino);
    }

    /// Убирает запись, на которую никто не ссылается; файл со своим содержимым и путём живёт, пока его не удалят или не переименуют.
    fn drop_if_unused(&mut self, ino: Ino) {
        let Some(node) = self.nodes.get(&ino) else { return };
        if node.nlookup > 0 || (node.local.is_some() && node.path.is_some()) {
            return;
        }
        if let Some(path) = &node.path {
            self.by_path.remove(path);
        }
        self.nodes.remove(&ino);
    }

    /// Путь больше не принадлежит никому: файл удалён. То же для всего, что лежало под ним.
    pub(super) fn orphan(&mut self, path: &str) {
        let prefix = format!("{path}/");
        let gone: Vec<String> = self
            .by_path
            .keys()
            .filter(|known| known.as_str() == path || known.starts_with(&prefix))
            .cloned()
            .collect();
        for known in gone {
            if let Some(ino) = self.by_path.remove(&known)
                && let Some(node) = self.nodes.get_mut(&ino)
            {
                node.path = None;
                self.drop_if_unused(ino);
            }
        }
    }

    /// `rename` файла поверх цели: запись `from` переезжает на путь `to` и сохраняет номер inode, прежняя запись `to` теряет путь.
    pub(super) fn move_over(&mut self, from: &str, to: &str) {
        let Some(ino) = self.by_path.remove(from) else { return };
        if let Some(old) = self.by_path.insert(to.to_owned(), ino)
            && let Some(node) = self.nodes.get_mut(&old)
        {
            node.path = None;
            self.drop_if_unused(old);
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.path = Some(to.to_owned());
        }
    }

    /// Имена и номера файлов, которые лежат только в сессии, непосредственно в каталоге `dir`.
    pub(super) fn local_children(&self, dir: &str) -> Vec<(String, Ino)> {
        let mut children: Vec<(String, Ino)> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.local.is_some())
            .filter_map(|(ino, node)| {
                let path = node.path.as_deref()?;
                (parent_of(path) == dir).then(|| (name_of(path).to_owned(), *ino))
            })
            .collect();
        children.sort();
        children
    }
}

#[cfg(test)]
#[path = "../../../tests/session/fs/inodes.rs"]
mod tests;
