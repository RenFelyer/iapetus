//! Окружение runner в контейнере session: точки монтирования проекта и `$HOME`, hostname, `/etc/os-release` и `$HOME`
//! агента.
//!
//! Всё это session ставит по назначению runner до запуска агента. Состояние агента лежит вне `$HOME` runner, в
//! каталоге состояния. `$HOME` runner, если runner его отдаёт, монтируется второй ФС по каноническому пути; проект,
//! лежащий в нём, — поверх неё.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context;
use fuser::BackgroundSession;
use nerve::protocol::{MAX_OS_RELEASE, Root, RunnerHost, check_project};
use rustix::mount::{UnmountFlags, unmount};

use super::fs::{self, Vfs};

/// Наибольшая длина имени машины.
const MAX_HOSTNAME: usize = 64;

/// Что агент видит как runner: каталог проекта и `$HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Environment {
    pub project: PathBuf,
    pub home: PathBuf,
    /// `$HOME` — файловая система `$HOME` runner; иначе обычный каталог контейнера или корень состояния.
    pub home_fs: bool,
    /// Исходная запись `$HOME` runner, если она ссылка на `home`: в контейнере её место занимает такая же ссылка.
    pub home_alias: Option<PathBuf>,
}

impl Environment {
    /// Сменились ли каталоги, по которым монтируются файловые системы: агенту нужен новый запуск.
    pub(super) fn moved_to(&self, other: &Self) -> bool {
        self.project != other.project || self.home != other.home || self.home_fs != other.home_fs
    }

    /// Что и в каком порядке монтируется: `$HOME` раньше проекта, потому что проект внутри него.
    fn mount_plan(&self) -> Vec<MountPoint> {
        let home = self.home_fs.then(|| MountPoint {
            path: self.home.clone(),
            root: Root::Home,
        });
        let project = MountPoint {
            path: self.project.clone(),
            root: Root::Project,
        };
        home.into_iter().chain([project]).collect()
    }
}

/// Куда монтируется файловая система каталога runner.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountPoint {
    path: PathBuf,
    root: Root,
}

/// Файловые системы проекта и `$HOME` и окружение контейнера, которое меняет назначение runner.
pub(super) struct Workspace {
    vfs: Arc<Vfs>,
    home_vfs: Arc<Vfs>,
    /// Корень состояния агентов: проект и `$HOME` с ним не пересекаются.
    state_root: PathBuf,
    etc: PathBuf,
    agent_uid: u32,
    /// Имена в `$HOME`, которые ведут в каталог состояния: агент без переменной состояния ищет его там.
    home_links: Vec<String>,
    mounted: Mutex<Option<Layout>>,
}

/// Смонтированная ФС.
struct Mounted {
    path: PathBuf,
    session: Option<BackgroundSession>,
    detached: bool,
}

impl Mounted {
    /// Отсоединяет ФС от дерева сразу, даже если в ней открыты файлы агента: обычное размонтирование занятой ФС
    /// отказывает, а ожидание агента остановило бы смену runner.
    fn detach(&mut self) {
        if self.detached {
            return;
        }
        self.detached = true;
        if let Err(error) = unmount(&self.path, UnmountFlags::DETACH) {
            tracing::warn!(%error, path = %self.path.display(), "ФС не отсоединена");
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.detach();
        drop(self.session.take());
    }
}

/// Смонтированные ФС окружения в порядке монтирования.
struct Layout {
    plan: Vec<MountPoint>,
    mounted: Vec<Mounted>,
}

impl Layout {
    /// Отсоединяет в порядке, обратном монтированию: вложенная ФС раньше той, в которой лежит её точка.
    fn detach(&mut self) {
        for mounted in self.mounted.iter_mut().rev() {
            mounted.detach();
        }
    }
}

impl Drop for Layout {
    fn drop(&mut self) {
        self.detach();
        while let Some(mounted) = self.mounted.pop() {
            drop(mounted);
        }
    }
}

impl Workspace {
    pub(super) fn new(
        vfs: Arc<Vfs>,
        home_vfs: Arc<Vfs>,
        state_root: PathBuf,
        etc: PathBuf,
        agent_uid: u32,
        home_links: Vec<String>,
    ) -> Self {
        Self {
            vfs,
            home_vfs,
            state_root,
            etc,
            agent_uid,
            home_links,
            mounted: Mutex::new(None),
        }
    }

    /// Окружение агента по сведениям runner; см. [`resolve`].
    pub(super) fn environment(&self, host: &RunnerHost) -> Result<Environment, String> {
        resolve(&self.state_root, self.agent_uid, host)
    }

    /// Файлы `$HOME` берутся с другого runner.
    pub(super) fn switch_runner(&self, runner: &nerve::fingerprint::Fingerprint) {
        self.home_vfs.switch_runner(runner);
    }

    /// Ставит окружение runner, кроме ФС проекта: `$HOME`, hostname и `/etc/os-release`. Без прав на hostname и
    /// `os-release` агент работает с тем, что есть в контейнере: отказ только пишется в журнал.
    pub(super) fn apply(&self, host: &RunnerHost, environment: &Environment) -> anyhow::Result<()> {
        self.prepare_home(environment).context("HOME агента не подготовлен")?;
        if let Err(error) = self.set_hostname(&host.hostname) {
            tracing::warn!(error = %format!("{error:#}"), "hostname runner не установлен");
        }
        if let Err(error) = write_os_release(&self.etc, &host.os_release) {
            tracing::warn!(error = %format!("{error:#}"), "/etc/os-release runner не записан");
        }
        Ok(())
    }

    /// Монтирует ФС окружения (`$HOME` раньше проекта), если они ещё не там; прежние, по другим путям, отсоединяет в
    /// обратном порядке. Предки точек монтирования проекта — пустые каталоги контейнера или, внутри `$HOME`, каталоги
    /// runner. Вызывается, когда агента нет: рабочий каталог остановленного агента исчезает вместе с прежней ФС.
    pub(super) fn mount(&self, environment: &Environment) -> anyhow::Result<()> {
        let plan = environment.mount_plan();
        let mut layout = self.mounted.lock().unwrap_or_else(PoisonError::into_inner);
        if layout.as_ref().is_some_and(|current| current.plan == plan) {
            return Ok(());
        }
        if let Some(mut old) = layout.take() {
            old.detach();
            // Поток FUSE прежней ФС заканчивается, когда её отпустит последний процесс: ждать его здесь нельзя.
            tokio::task::spawn_blocking(move || drop(old));
        }
        self.home_vfs.deactivate();
        let mut mounted = Layout {
            plan: plan.clone(),
            mounted: Vec::new(),
        };
        for point in &plan {
            let vfs = match point.root {
                Root::Project => &self.vfs,
                Root::Home => &self.home_vfs,
            };
            vfs.set_root(&point.path);
            if !point.path.is_dir() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(&point.path)
                    .with_context(|| format!("точка монтирования {} не создана", point.path.display()))?;
            }
            let session = fs::mount(Arc::clone(vfs), &point.path, point.root)
                .with_context(|| format!("FUSE не смонтирован в {}", point.path.display()))?;
            mounted.mounted.push(Mounted {
                path: point.path.clone(),
                session: Some(session),
                detached: false,
            });
        }
        *layout = Some(mounted);
        Ok(())
    }

    /// Исходная запись `$HOME` runner становится ссылкой на него. `$HOME` без второй ФС создаётся, если его нет,
    /// открытым на запись всем (владелец — root: у session нет `CAP_CHOWN`), со ссылками в каталог состояния; при
    /// `$HOME`-ФС ссылки отдаёт она сама.
    fn prepare_home(&self, environment: &Environment) -> anyhow::Result<()> {
        if let Some(alias) = &environment.home_alias {
            link_alias(alias, &environment.home).with_context(|| format!("ссылка {} не создана", alias.display()))?;
        }
        if environment.home_fs {
            return Ok(());
        }
        let home = environment.home.as_path();
        if !home.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o755).create(home)?;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o1777))?;
        }
        if home == self.state_root {
            return Ok(());
        }
        for name in &self.home_links {
            let link = home.join(name);
            match std::fs::remove_file(&link) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("{} не заменена", link.display())),
            }
            std::os::unix::fs::symlink(self.state_root.join(name), &link)
                .with_context(|| format!("ссылка {} не создана", link.display()))?;
        }
        Ok(())
    }

    /// Имя машины контейнера и его запись в `/etc/hosts`, чтобы имя разрешалось.
    fn set_hostname(&self, name: &str) -> anyhow::Result<()> {
        if name.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(valid_hostname(name), "имя {name:?} не подходит как hostname");
        rustix::system::sethostname(name.as_bytes()).context("sethostname отказал")?;
        add_hosts_entry(&self.etc, name)
    }
}

/// Окружение агента по сведениям runner. Путь проекта, который нельзя смонтировать, — отказ. `$HOME` runner, который
/// нельзя завести, заменяется корнем состояния агентов.
pub(super) fn resolve(state_root: &Path, agent_uid: u32, host: &RunnerHost) -> Result<Environment, String> {
    let project = PathBuf::from(&host.project);
    check_project(&project, state_root)?;
    let home = usable_home(state_root, agent_uid, &host.home, &project, host.home_served);
    let from_runner = home == Path::new(&host.home);
    let home_alias = if from_runner {
        home_alias(state_root, &host.home_origin, &home, &project)
    } else {
        None
    };
    Ok(Environment {
        project,
        home,
        home_fs: host.home_served && from_runner,
        home_alias,
    })
}

/// Исходная запись `$HOME` runner, на месте которой в контейнере встаёт ссылка на `home`: только вне системных
/// каталогов, корня состояния и проекта.
fn home_alias(state_root: &Path, origin: &str, home: &Path, project: &Path) -> Option<PathBuf> {
    if origin.is_empty() || Path::new(origin) == home {
        return None;
    }
    let alias = PathBuf::from(origin);
    if let Err(reason) = check_project(&alias, state_root) {
        tracing::warn!(origin, %reason, "исходный путь HOME runner не подходит ссылке, она не создаётся");
        return None;
    }
    if alias.starts_with(project) || project.starts_with(&alias) {
        tracing::warn!(origin, "исходный путь HOME runner пересекается с проектом, ссылка не создаётся");
        return None;
    }
    Some(alias)
}

/// Ссылка `alias` → `home`; прежняя ссылка заменяется, настоящий каталог или файл на её месте остаётся.
fn link_alias(alias: &Path, home: &Path) -> anyhow::Result<()> {
    if let Some(parent) = alias.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o755).create(parent)?;
    }
    match std::fs::symlink_metadata(alias) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(alias)?,
        Ok(_) => anyhow::bail!("на месте ссылки уже есть файл или каталог"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::os::unix::fs::symlink(home, alias)?;
    Ok(())
}

/// `$HOME` runner, если агент сможет им пользоваться: путь не пересекается с системными каталогами и корнем
/// состояния, не лежит внутри проекта (его файлы ушли бы на runner), а существующий каталог принадлежит агенту
/// или открыт всем на запись; `$HOME`-ФС поверх каталога его права не смотрит. Иначе — корень состояния.
fn usable_home(state_root: &Path, agent_uid: u32, home: &str, project: &Path, served: bool) -> PathBuf {
    let fallback = state_root.to_owned();
    if home.is_empty() {
        return fallback;
    }
    let path = Path::new(home);
    if let Err(reason) = check_project(path, state_root) {
        tracing::warn!(home, %reason, "HOME runner не подходит агенту, остаётся каталог состояния");
        return fallback;
    }
    if path.starts_with(project) {
        tracing::warn!(home, "HOME runner лежит внутри проекта, остаётся каталог состояния");
        return fallback;
    }
    if served {
        return path.to_owned();
    }
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_dir() || (meta.uid() != agent_uid && meta.mode() & 0o002 == 0) => {
            tracing::warn!(home, "каталог HOME runner в контейнере закрыт агенту, остаётся каталог состояния");
            fallback
        }
        _ => path.to_owned(),
    }
}

fn valid_hostname(name: &str) -> bool {
    name.len() <= MAX_HOSTNAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

/// Строка `127.0.0.1 <имя>` в hosts; есть уже — не повторяется.
fn add_hosts_entry(etc: &Path, name: &str) -> anyhow::Result<()> {
    let path = etc.join("hosts");
    let line = format!("127.0.0.1\t{name}");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|existing| existing == line) {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(&path)?;
    let separator = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    writeln!(file, "{separator}{line}")?;
    Ok(())
}

/// Заменяет `/etc/os-release` (в образе это ссылка) файлом с содержимым runner. Пустое содержимое и слишком большое
/// файл не меняют.
fn write_os_release(etc: &Path, content: &str) -> anyhow::Result<()> {
    if content.is_empty() || content.len() > MAX_OS_RELEASE {
        return Ok(());
    }
    let path = etc.join("os-release");
    if std::fs::read_to_string(&path).is_ok_and(|current| current == content) {
        return Ok(());
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/session/host.rs"]
mod tests;
