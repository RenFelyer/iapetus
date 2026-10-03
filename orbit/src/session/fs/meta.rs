//! Кэш метаданных: атрибуты путей и списки каталогов, которые сессия узнала у runner.
//!
//! Запись годна, пока эпоха не сменилась: команда на runner или конец хода могли изменить что угодно, а событие
//! watch могло ещё не дойти. Внутри эпохи запись уточняют события watch и собственные изменения сессии.
//! Отрицательная запись — «пути нет» — хранится так же, как положительная.
//!
//! Записям каталога, который не под наблюдением, верить нельзя вовсе: события watch о нём не придут, и он остаётся
//! таким, пока сессия не прочтёт его заново и runner не возьмёт его под наблюдение.

use std::collections::{BTreeSet, HashMap};

use nerve::protocol::{Attr, DirEntry};

use super::inodes::{name_of, parent_of};

/// Что известно о пути.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Known {
    Present(Attr),
    Absent,
}

impl Known {
    pub(super) fn attr(self) -> Option<Attr> {
        match self {
            Self::Present(attr) => Some(attr),
            Self::Absent => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Cached {
    /// `None` — пути нет.
    attr: Option<Attr>,
    epoch: u64,
}

impl Cached {
    fn known(self) -> Known {
        self.attr.map_or(Known::Absent, Known::Present)
    }
}

/// Цель ссылки и атрибуты ссылки, при которых она прочитана.
#[derive(Debug, Clone)]
struct Link {
    attr: Attr,
    target: String,
    epoch: u64,
}

#[derive(Debug)]
struct Listing {
    names: BTreeSet<String>,
    epoch: u64,
}

#[derive(Debug, Default)]
pub(super) struct Meta {
    entries: HashMap<String, Cached>,
    listings: HashMap<String, Listing>,
    links: HashMap<String, Link>,
    /// Каталоги без наблюдения: их записи не кэшируются.
    unwatched: BTreeSet<String>,
}

impl Meta {
    /// Что известно о пути в эпохе `epoch`; `None` — неизвестно.
    pub(super) fn get(&self, path: &str, epoch: u64) -> Option<Known> {
        if let Some(cached) = self.entries.get(path)
            && cached.epoch == epoch
        {
            return Some(cached.known());
        }
        // Каталог прочитан целиком: имени в нём нет — значит, пути нет.
        let listing = self.listings.get(parent_of(path))?;
        (!path.is_empty() && listing.epoch == epoch && !listing.names.contains(name_of(path))).then_some(Known::Absent)
    }

    /// Цель ссылки, прочитанная в эпохе `epoch`, пока запись пути показывает ту же ссылку: заменённая ссылка
    /// меняет атрибуты (время изменения), и событие watch или перечитывание каталога обесценивает прочитанную цель.
    pub(super) fn link(&self, path: &str, epoch: u64) -> Option<&str> {
        let link = self.links.get(path).filter(|link| link.epoch == epoch)?;
        let cached = self.entries.get(path)?;
        (cached.attr == Some(link.attr)).then_some(link.target.as_str())
    }

    /// Запоминает цель ссылки, у которой в момент чтения были атрибуты `attr`. Ссылке в каталоге без наблюдения
    /// не верят: её замену никто не заметит.
    pub(super) fn put_link(&mut self, path: &str, attr: Attr, target: String, epoch: u64) {
        if self.in_unwatched(path) {
            return;
        }
        self.links.insert(path.to_owned(), Link { attr, target, epoch });
    }

    /// Что было известно о пути в любую эпоху: по этому видно, что агент видел раньше.
    pub(super) fn last_known(&self, path: &str) -> Option<Known> {
        self.entries.get(path).map(|cached| cached.known())
    }

    /// Запоминает ответ runner или своё изменение. `epoch` — эпоха на момент запроса: ответ, пришедший после смены
    /// эпохи, так и остаётся устаревшим.
    pub(super) fn put(&mut self, path: &str, attr: Option<Attr>, epoch: u64) {
        if self.in_unwatched(path) {
            return;
        }
        self.entries.insert(path.to_owned(), Cached { attr, epoch });
        if path.is_empty() {
            return;
        }
        let Some(listing) = self.listings.get_mut(parent_of(path)) else {
            return;
        };
        if listing.epoch != epoch {
            return;
        }
        if attr.is_some() {
            listing.names.insert(name_of(path).to_owned());
        } else {
            listing.names.remove(name_of(path));
        }
    }

    /// Каталог пути не под наблюдением: ни записям в нём, ни копиям его файлов верить нельзя.
    pub(super) fn in_unwatched(&self, path: &str) -> bool {
        !path.is_empty() && self.unwatched.contains(parent_of(path))
    }

    /// Runner больше не наблюдает каталог: его список и записи его детей не достоверны.
    pub(super) fn unwatch(&mut self, dir: &str) {
        self.unwatched.insert(dir.to_owned());
        self.listings.remove(dir);
        self.entries.retain(|path, _| path.is_empty() || parent_of(path) != dir);
        self.links.retain(|path, _| path.is_empty() || parent_of(path) != dir);
    }

    /// Запоминает прочитанный каталог. Дети, которых в списке нет, теперь известны как отсутствующие. Если runner
    /// каталог под наблюдение не взял, записи не запоминаются.
    pub(super) fn put_listing(&mut self, dir: &str, entries: &[DirEntry], epoch: u64, watched: bool) {
        if !watched {
            self.unwatch(dir);
            return;
        }
        self.unwatched.remove(dir);
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        if let Some(old) = self.listings.get(dir) {
            for name in &old.names {
                if !entries.iter().any(|entry| &entry.name == name) {
                    self.entries.insert(format!("{prefix}{name}"), Cached { attr: None, epoch });
                }
            }
        }
        for entry in entries {
            self.entries.insert(
                format!("{prefix}{}", entry.name),
                Cached {
                    attr: Some(entry.attr),
                    epoch,
                },
            );
        }
        self.listings.insert(
            dir.to_owned(),
            Listing {
                names: entries.iter().map(|entry| entry.name.clone()).collect(),
                epoch,
            },
        );
    }

    /// Эпоха, в которой прочитан список каталога; `None` — списка нет.
    pub(super) fn listing_epoch(&self, dir: &str) -> Option<u64> {
        self.listings.get(dir).map(|listing| listing.epoch)
    }

    /// Каталог из списка эпохи `epoch`: имена с атрибутами; `None`, если списка нет или он устарел.
    pub(super) fn listing(&self, dir: &str, epoch: u64) -> Option<Vec<DirEntry>> {
        let listing = self.listings.get(dir).filter(|listing| listing.epoch == epoch)?;
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        listing
            .names
            .iter()
            .map(|name| {
                let attr = self.entries.get(&format!("{prefix}{name}"))?.attr?;
                Some(DirEntry { name: name.clone(), attr })
            })
            .collect()
    }

    /// Забывает запись пути и список его каталога: сессия ошиблась в этом пути, значит, списку тоже нельзя верить.
    /// Следующий вопрос пойдёт к runner.
    pub(super) fn invalidate(&mut self, path: &str) {
        self.entries.remove(path);
        self.links.remove(path);
        self.listings.remove(parent_of(path));
    }

    /// Путь и всё под ним исчезли.
    pub(super) fn remove_tree(&mut self, path: &str, epoch: u64) {
        let prefix = format!("{path}/");
        self.entries.retain(|known, _| !known.starts_with(&prefix));
        self.links.retain(|known, _| known != path && !known.starts_with(&prefix));
        self.listings.retain(|known, _| known != path && !known.starts_with(&prefix));
        self.unwatched.retain(|known| known != path && !known.starts_with(&prefix));
        self.put(path, None, epoch);
    }
}
