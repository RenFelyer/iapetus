//! Файловые операции канала данных: сессия спрашивает, runner отвечает.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::Error;

/// Вид записи в каталоге.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// Атрибуты записи. Для символической ссылки — её собственные, не цели.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct Attr {
    pub kind: Kind,
    pub size: u64,
    /// Время изменения, наносекунды от начала эпохи.
    pub mtime_ns: i64,
    /// Права: младшие 12 бит `st_mode`.
    pub mode: u32,
}

/// Содержимое файла. Идёт в сообщении целиком: на кадры его режет мультиплексор.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct Content(pub Vec<u8>);

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Content({} байт)", self.0.len())
    }
}

/// Версия файла, на которую рассчитывает запись.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum Expect {
    /// sha256 содержимого, которое видела сессия.
    Hash([u8; 32]),
    /// Файла быть не должно.
    Absent,
}

/// Запрос сессии → runner. Пути — относительно рабочего каталога, с `/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum FsRequest {
    Stat {
        path: String,
    },
    ReadDir {
        path: String,
    },
    Read {
        path: String,
    },
    /// Цель символической ссылки как записана в ссылке. Цель, которая после разрешения от каталога ссылки лежит
    /// вне проекта и `$HOME` runner, — `Access`: ядро сессии разрешило бы её в своей файловой системе.
    ReadLink {
        path: String,
    },
    Write {
        path: String,
        expect: Expect,
        content: Content,
    },
    /// Создаёт один каталог; родитель должен существовать.
    MakeDir {
        path: String,
    },
    /// Хеш файла без содержимого: сессия сверяет им свою копию и версию, на которую рассчитывает запись.
    Hash {
        path: String,
    },
    /// Удаляет файл, символическую ссылку или пустой каталог; ссылка удаляется сама, а не цель.
    Remove {
        path: String,
    },
    /// Переносит файл. Источник должен быть обычным файлом с хешем `expect_from`, цель — как ждёт `expect_to`:
    /// `Absent` — цели нет (перенос её не затирает), `Hash` — там файл с этим хешем, и он заменяется. Любое
    /// несовпадение — `Conflict`, и ничего не меняется. Каталоги не переносятся.
    Rename {
        from: String,
        to: String,
        expect_from: [u8; 32],
        expect_to: Expect,
    },
}

/// Запись каталога.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub struct DirEntry {
    pub name: String,
    pub attr: Attr,
}

/// Успешный ответ runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(proptest_derive::Arbitrary))]
pub enum FsReply {
    Stat(Attr),
    ReadDir {
        entries: Vec<DirEntry>,
        /// Каталог под наблюдением: об изменениях в нём придут события watch. `false` — подписаться не вышло,
        /// и кэшу записей этого каталога верить нельзя. Событие `Unwatched` об этом же может прийти раньше ответа,
        /// поэтому сессия решает по ответу, а не по порядку сообщений.
        watched: bool,
    },
    Read {
        attr: Attr,
        sha256: [u8; 32],
        content: Content,
    },
    /// Ответ на [`FsRequest::ReadLink`]: цель ссылки.
    Link(String),
    /// Файл записан; хеш и атрибуты — новой версии.
    Written {
        attr: Attr,
        sha256: [u8; 32],
    },
    MadeDir(Attr),
    /// Ответ на [`FsRequest::Hash`]: атрибуты и sha256 текущего содержимого.
    Hashed {
        attr: Attr,
        sha256: [u8; 32],
    },
    Removed,
    /// Файл перенесён; атрибуты — файла на новом месте.
    Renamed(Attr),
}

/// Ответ на [`FsRequest`].
pub type FsResponse = Result<FsReply, Error>;
