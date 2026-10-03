//! Модель runner для тестов: дерево в памяти с теми же правилами, что у настоящего runner, и журнал запросов. Отвечает
//! по каналу данных (мультиплексор над `tokio::io::duplex`), как настоящий runner.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use nerve::mux::{Mux, Requests};
use nerve::protocol::{self, Attr, Content, DataRequest, DirEntry, Error, Expect, FsReply, FsRequest, FsResponse, Kind, Root};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

use super::{Config, Owner, Profile};
use crate::session::data::Link;

/// Канал данных от сессии к «runner»: `link` отдаётся ядру ФС, `switch` подключает и отключает канал.
pub(super) struct Wire {
    pub(super) link: Link,
    pub(super) switch: watch::Sender<Option<Mux>>,
    _session: Option<Mux>,
    _session_requests: Option<Requests>,
}

impl Wire {
    /// Runner не подключён: канала нет.
    pub(super) fn disconnected() -> Self {
        let (link, switch) = Link::fixed(None);
        Self {
            link,
            switch,
            _session: None,
            _session_requests: None,
        }
    }

    /// Runner, отвечающий на каждый запрос тем, что вернёт `reply` (его тело — запрос как есть); `None` — молчит.
    pub(super) fn answering(reply: impl Fn(Bytes) -> Option<Vec<u8>> + Send + Sync + 'static) -> Self {
        let (near, far) = tokio::io::duplex(64 * 1024);
        let (session, session_requests) = Mux::new(near);
        let (_runner, mut requests) = Mux::new(far);
        tokio::spawn(async move {
            while let Some(incoming) = requests.recv().await {
                if let Some(body) = reply(incoming.body().clone()) {
                    // Сессия могла уйти: отвечать некому.
                    incoming.respond(Bytes::from(body), true).await.unwrap_or_default();
                }
            }
        });
        Self::connected(session, session_requests)
    }

    /// Собеседник начинает с приветствия другой версии протокола.
    pub(super) fn foreign_version() -> Self {
        let (near, mut far) = tokio::io::duplex(64 * 1024);
        let (session, session_requests) = Mux::new(near);
        let mut hello = vec![0, 0, 0, 2, 0, 0, 0, 0, 0];
        hello.extend_from_slice(&(protocol::VERSION + 1).to_be_bytes());
        tokio::spawn(async move {
            far.write_all(&hello).await.unwrap_or_default();
            std::future::pending::<()>().await;
        });
        Self::connected(session, session_requests)
    }

    fn connected(session: Mux, session_requests: Requests) -> Self {
        let (link, switch) = Link::fixed(Some(session.clone()));
        Self {
            link,
            switch,
            _session: Some(session),
            _session_requests: Some(session_requests),
        }
    }
}

/// Наибольший файл в тестах: больше самого большого файла сценариев, чтобы предел не мешал им.
pub(super) const TEST_MAX_FILE: usize = 1 << 20;

/// Настройки ФС для теста: кэш в каталоге `cache_dir`, владелец файлов с runner — uid 1000.
pub(super) fn test_config(cache_dir: PathBuf, profile: Profile, request_timeout: Duration) -> Config {
    Config {
        profile,
        owner: Owner { uid: 1000, gid: 1000 },
        fingerprint: "runner-test".parse().unwrap(),
        root_kind: Root::Project,
        links: Vec::new(),
        root: "/home/agent/proj".into(),
        cache_dir,
        max_file: TEST_MAX_FILE,
        request_timeout,
        hidden: Vec::new(),
        temp_name: crate::session::agent::Profile(nerve::agent::Agent::ClaudeCode)
            .isolation()
            .temp_name,
    }
}

pub(super) fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Debug, Clone)]
struct File {
    content: Vec<u8>,
    /// Растёт при каждом изменении: по нему меняется `mtime`, как на настоящем диске.
    version: i64,
}

#[derive(Debug, Default)]
struct Tree {
    files: BTreeMap<String, File>,
    dirs: BTreeSet<String>,
    /// Символические ссылки: путь → цель и версия, от которой зависит `mtime`.
    links: BTreeMap<String, (String, i64)>,
    clock: i64,
    log: Vec<FsRequest>,
    /// Каталог runner каждого запроса из `log`, в том же порядке.
    roots: Vec<Root>,
    /// Каталоги, которые runner не берёт под наблюдение.
    unwatchable: BTreeSet<String>,
    /// Изменение мимо протокола перед ближайшим запросом вида `.0`: окно гонки между двумя запросами сессии.
    change_before: Option<(String, String, Vec<u8>)>,
    /// Обрыв канала на ближайшем запросе вида `.0`: ответ на него не доходит.
    break_on: Option<(String, Loss)>,
}

/// Что успевает runner, прежде чем канал оборвётся.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Loss {
    /// Запрос выполнен, потерян только ответ.
    AfterApply,
    /// Запрос до runner не дошёл.
    BeforeApply,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ModelRunner(Arc<Mutex<Tree>>);

fn parent_exists(tree: &Tree, path: &str) -> bool {
    let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    parent.is_empty() || tree.dirs.contains(parent)
}

fn file_attr(file: &File) -> Attr {
    Attr {
        kind: Kind::File,
        size: file.content.len() as u64,
        mtime_ns: file.version * 1000,
        mode: 0o644,
    }
}

fn link_attr(target: &str, version: i64) -> Attr {
    Attr {
        kind: Kind::Symlink,
        size: target.len() as u64,
        mtime_ns: version * 1000,
        mode: 0o777,
    }
}

fn dir_attr() -> Attr {
    Attr {
        kind: Kind::Dir,
        size: 4096,
        mtime_ns: 1,
        mode: 0o755,
    }
}

impl ModelRunner {
    /// Начинает отвечать на файловые запросы по каналу данных.
    pub(super) fn serve(&self) -> Wire {
        let model = self.clone();
        Wire::answering(move |body| match protocol::decode_body::<DataRequest>(&body) {
            Ok(DataRequest::Fs { root, request }) => {
                model.tree().roots.push(root);
                Some(protocol::encode(&model.answer(request)).unwrap())
            }
            _ => None,
        })
    }

    /// Каталоги runner, к которым относились запросы, в порядке прихода.
    pub(super) fn roots(&self) -> Vec<Root> {
        self.tree().roots.clone()
    }

    /// Runner, который переподключается после каждого обрыва из [`ModelRunner::break_on`]: старый канал закрывается,
    /// в `link` появляется новый, как после настоящего переподключения.
    pub(super) fn serve_reconnecting(&self) -> Wire {
        let (link, switch) = Link::fixed(None);
        // Первое соединение готово сразу: ядро ФС не должно застать runner неподключённым.
        let connect = |link: &watch::Sender<Option<Mux>>| {
            let (near, far) = tokio::io::duplex(64 * 1024);
            let (session, session_requests) = Mux::new(near);
            let (runner, requests) = Mux::new(far);
            link.send_replace(Some(session));
            (runner, requests, session_requests)
        };
        let mut connection = connect(&switch);
        let (model, reconnect) = (self.clone(), switch.clone());
        tokio::spawn(async move {
            loop {
                let (runner, mut requests, _session_requests) = connection;
                let mut broken = false;
                while let Some(incoming) = requests.recv().await {
                    let Ok(DataRequest::Fs { request, .. }) = protocol::decode_body::<DataRequest>(incoming.body()) else {
                        continue;
                    };
                    let loss = model.take_break(&request);
                    if loss == Some(Loss::AfterApply) {
                        // Запрос выполнен, а ответ теряется вместе с каналом.
                        drop(model.answer(request));
                    } else if loss.is_none() {
                        let body = protocol::encode(&model.answer(request)).unwrap();
                        incoming.respond(Bytes::from(body), true).await.unwrap_or_default();
                        continue;
                    }
                    broken = true;
                    break;
                }
                runner.close();
                if !broken {
                    return;
                }
                connection = connect(&reconnect);
            }
        });
        Wire {
            link,
            switch,
            _session: None,
            _session_requests: None,
        }
    }

    /// Обрыв, назначенный на этот запрос; снимается, сработав.
    fn take_break(&self, request: &FsRequest) -> Option<Loss> {
        let mut tree = self.tree();
        let kind = format!("{request:?}");
        let due = tree
            .break_on
            .as_ref()
            .is_some_and(|(wanted, _)| kind.starts_with(&format!("{wanted} ")));
        if due { tree.break_on.take().map(|(_, loss)| loss) } else { None }
    }

    /// На ближайшем запросе вида `kind` канал обрывается: `loss` — успел ли runner его выполнить. Работает с
    /// [`ModelRunner::serve_reconnecting`].
    pub(super) fn break_on(&self, kind: &str, loss: Loss) {
        self.tree().break_on = Some((kind.to_owned(), loss));
    }

    /// Выполняет запрос по правилам runner; перед ним — изменение «мимо протокола», если оно назначено на этот запрос.
    fn answer(&self, request: FsRequest) -> FsResponse {
        let mut tree = self.tree();
        let kind = format!("{request:?}");
        let due = tree
            .change_before
            .as_ref()
            .is_some_and(|(wanted, ..)| kind.starts_with(&format!("{wanted} ")));
        if due && let Some((_, path, content)) = tree.change_before.take() {
            Self::store(&mut tree, &path, content);
        }
        tree.log.push(request.clone());
        Self::handle(&mut tree, request)
    }

    fn tree(&self) -> MutexGuard<'_, Tree> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn handle(tree: &mut Tree, request: FsRequest) -> FsResponse {
        match request {
            FsRequest::Stat { path } => {
                if path.is_empty() || tree.dirs.contains(&path) {
                    return Ok(FsReply::Stat(dir_attr()));
                }
                if let Some((target, version)) = tree.links.get(&path) {
                    return Ok(FsReply::Stat(link_attr(target, *version)));
                }
                tree.files
                    .get(&path)
                    .map(|file| FsReply::Stat(file_attr(file)))
                    .ok_or(Error::NotFound)
            }
            FsRequest::ReadLink { path } => match tree.links.get(&path) {
                // Цель вне проекта runner не отдаёт.
                Some((target, _)) if target.starts_with('/') && !target.starts_with("/home/agent/proj") => Err(Error::Access),
                Some((target, _)) => Ok(FsReply::Link(target.clone())),
                None if tree.files.contains_key(&path) || tree.dirs.contains(&path) || path.is_empty() => {
                    Err(Error::Io("Invalid argument".to_owned()))
                }
                None => Err(Error::NotFound),
            },
            FsRequest::ReadDir { path } => Self::read_dir(tree, &path),
            FsRequest::Read { path } => {
                let file = Self::regular(tree, &path)?;
                Ok(FsReply::Read {
                    attr: file_attr(file),
                    sha256: sha(&file.content),
                    content: Content(file.content.clone()),
                })
            }
            FsRequest::Hash { path } => {
                let file = Self::regular(tree, &path)?;
                Ok(FsReply::Hashed {
                    attr: file_attr(file),
                    sha256: sha(&file.content),
                })
            }
            FsRequest::Write { path, expect, content } => Self::write(tree, &path, expect, content),
            FsRequest::MakeDir { path } => {
                if path.is_empty() || tree.dirs.contains(&path) || tree.files.contains_key(&path) {
                    return Err(Error::Conflict);
                }
                if !parent_exists(tree, &path) {
                    return Err(Error::NotFound);
                }
                tree.dirs.insert(path);
                Ok(FsReply::MadeDir(dir_attr()))
            }
            FsRequest::Remove { path } => Self::remove(tree, &path),
            FsRequest::Rename {
                from,
                to,
                expect_from,
                expect_to,
            } => Self::rename(tree, &from, &to, expect_from, expect_to),
        }
    }

    fn read_dir(tree: &Tree, path: &str) -> FsResponse {
        if !path.is_empty() && !tree.dirs.contains(path) {
            return Err(if tree.files.contains_key(path) {
                Error::Io("не каталог".to_owned())
            } else {
                Error::NotFound
            });
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let child = |full: &str| full.strip_prefix(&prefix).filter(|rest| !rest.contains('/')).map(str::to_owned);
        let mut entries: Vec<DirEntry> = tree
            .dirs
            .iter()
            .filter_map(|dir| child(dir).map(|name| DirEntry { name, attr: dir_attr() }))
            .chain(tree.links.iter().filter_map(|(link, (target, version))| {
                child(link).map(|name| DirEntry {
                    name,
                    attr: link_attr(target, *version),
                })
            }))
            .chain(tree.files.iter().filter_map(|(file, data)| {
                child(file).map(|name| DirEntry {
                    name,
                    attr: file_attr(data),
                })
            }))
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(FsReply::ReadDir {
            entries,
            watched: !tree.unwatchable.contains(path),
        })
    }

    fn write(tree: &mut Tree, path: &str, expect: Expect, content: Content) -> FsResponse {
        let Content(bytes) = content;
        if !parent_exists(tree, path) {
            return Err(Error::NotFound);
        }
        if tree.dirs.contains(path) {
            return Err(match expect {
                Expect::Absent => Error::Conflict,
                Expect::Hash(_) => Error::Io("не обычный файл".to_owned()),
            });
        }
        let matches = match (expect, tree.files.get(path)) {
            (Expect::Absent, None) => true,
            (Expect::Hash(wanted), Some(file)) => sha(&file.content) == wanted,
            (Expect::Absent, Some(_)) | (Expect::Hash(_), None) => false,
        };
        if !matches {
            return Err(Error::Conflict);
        }
        let file = Self::store(tree, path, bytes);
        Ok(FsReply::Written {
            attr: file_attr(&file),
            sha256: sha(&file.content),
        })
    }

    fn remove(tree: &mut Tree, path: &str) -> FsResponse {
        if path.is_empty() {
            return Err(Error::Io("рабочий каталог не удаляется".to_owned()));
        }
        if tree.files.remove(path).is_some() {
            return Ok(FsReply::Removed);
        }
        if tree.dirs.contains(path) {
            let prefix = format!("{path}/");
            if tree.files.keys().chain(tree.dirs.iter()).any(|known| known.starts_with(&prefix)) {
                return Err(Error::NotEmpty);
            }
            tree.dirs.remove(path);
            return Ok(FsReply::Removed);
        }
        Err(Error::NotFound)
    }

    fn rename(tree: &mut Tree, from: &str, to: &str, expect_from: [u8; 32], expect_to: Expect) -> FsResponse {
        if from.is_empty() || to.is_empty() {
            return Err(Error::Io("рабочий каталог не переносится".to_owned()));
        }
        if !parent_exists(tree, from) || !parent_exists(tree, to) {
            return Err(Error::NotFound);
        }
        if from == to {
            return Err(Error::Io("источник и цель — один и тот же путь".to_owned()));
        }
        let source = Self::regular(tree, from)?;
        if sha(&source.content) != expect_from {
            return Err(Error::Conflict);
        }
        let target_matches = match expect_to {
            Expect::Absent => !tree.files.contains_key(to) && !tree.dirs.contains(to),
            Expect::Hash(wanted) => Self::regular(tree, to).is_ok_and(|target| sha(&target.content) == wanted),
        };
        if !target_matches {
            return Err(Error::Conflict);
        }
        let file = tree.files.remove(from).ok_or(Error::NotFound)?;
        tree.files.insert(to.to_owned(), file.clone());
        Ok(FsReply::Renamed(file_attr(&file)))
    }

    fn regular<'a>(tree: &'a Tree, path: &str) -> Result<&'a File, Error> {
        if tree.dirs.contains(path) || path.is_empty() {
            return Err(Error::Io("не обычный файл".to_owned()));
        }
        tree.files.get(path).ok_or(Error::NotFound)
    }

    fn store(tree: &mut Tree, path: &str, content: Vec<u8>) -> File {
        tree.clock += 1;
        let file = File {
            content,
            version: tree.clock,
        };
        tree.files.insert(path.to_owned(), file.clone());
        file
    }

    /// Изменение мимо протокола: кто-то работает на машине runner.
    pub(super) fn external_write(&self, path: &str, content: &[u8]) {
        Self::store(&mut self.tree(), path, content.to_vec());
    }

    pub(super) fn external_remove(&self, path: &str) {
        self.tree().files.remove(path);
    }

    pub(super) fn external_remove_dir(&self, path: &str) {
        self.tree().dirs.remove(path);
    }

    pub(super) fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.tree()
            .files
            .iter()
            .map(|(path, file)| (path.clone(), file.content.clone()))
            .collect()
    }

    pub(super) fn dirs(&self) -> BTreeSet<String> {
        self.tree().dirs.clone()
    }

    /// Добавляет каталог без запроса: подготовка состояния.
    pub(super) fn put_dir(&self, path: &str) {
        self.tree().dirs.insert(path.to_owned());
    }

    /// Ссылка с целью `target`; повторный вызов заменяет цель, и ссылка меняет время изменения.
    pub(super) fn put_link(&self, path: &str, target: &str) {
        let mut tree = self.tree();
        tree.clock += 1;
        let version = tree.clock;
        tree.links.insert(path.to_owned(), (target.to_owned(), version));
    }

    pub(super) fn link_attr_of(&self, path: &str) -> Option<Attr> {
        self.tree().links.get(path).map(|(target, version)| link_attr(target, *version))
    }

    pub(super) fn put_file(&self, path: &str, content: &[u8]) {
        Self::store(&mut self.tree(), path, content.to_vec());
    }

    pub(super) fn attr_of(&self, path: &str) -> Option<Attr> {
        let tree = self.tree();
        tree.files.get(path).map(file_attr)
    }

    /// Перед ближайшим запросом вида `kind` (`Read`, `Hash`, `Write`, ...) файл `path` получает `content` мимо протокола.
    pub(super) fn change_before(&self, kind: &str, path: &str, content: &[u8]) {
        self.tree().change_before = Some((kind.to_owned(), path.to_owned(), content.to_vec()));
    }

    /// Runner не берёт каталог `path` под наблюдение.
    pub(super) fn set_unwatchable(&self, path: &str) {
        self.tree().unwatchable.insert(path.to_owned());
    }

    /// Запросы с начала работы или с последнего сброса.
    pub(super) fn log(&self) -> Vec<FsRequest> {
        self.tree().log.clone()
    }

    pub(super) fn clear_log(&self) {
        self.tree().log.clear();
    }

    /// Сколько запросов вида `kind` ушло на runner: `Stat`, `ReadDir`, `Read`, `Write`, `MakeDir`, `Hash`, `Remove`, `Rename`, `ReadLink`.
    pub(super) fn count(&self, kind: &str) -> usize {
        self.tree()
            .log
            .iter()
            .filter(|request| format!("{request:?}").starts_with(&format!("{kind} ")))
            .count()
    }
}
