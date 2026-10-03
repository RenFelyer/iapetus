//! Временные runner (клоны): точка входа в указанный каталог той же машины.
//!
//! Каталог клона не совпадает с каталогом родителя и не вложен в него ни в одну сторону; клоны клонов не
//! создаются. Если указанный каталог — корень репозитория git, клон работает в его `git worktree` рядом
//! (`<каталог>-clone-<N>`) на ветке `iapetus/<имя каталога клона>`. Иначе клон работает прямо в указанном каталоге
//! и ничего в нём не меняет, в том числе при остановке.
//!
//! Жизнь клона ведёт [`Lifetime`]: без назначения он живёт [`IDLE_STOP`], конец сессии (отключение или
//! переключение на другой runner) его останавливает. Перед остановкой [`finish`] фиксирует изменения клона в
//! `git worktree` в его ветке и удаляет рабочую копию; ветка с коммитом остаётся. Клон, у которого не было ни одной
//! сессии, рабочую копию не трогает: автора коммитов нет, и с ней разбираются люди.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Author, CloneReply, CloneResponse, Error};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

use super::exec::session_vars;
use super::forbidden;
use super::identity::Identity;
use super::reaper::clone_process;

#[cfg(test)]
#[path = "../../tests/runner/clone.rs"]
mod tests;

/// Сколько клон живёт без назначенной сессии.
const IDLE_STOP: Duration = Duration::from_mins(5);

/// Сколько номеров каталога клона перебирать, прежде чем сдаться.
const MAX_CLONES: u32 = 1000;

/// Сообщение автокоммита.
const AUTOCOMMIT_MESSAGE: &str = "autocommit: изменения клона при отключении сессии";

/// Префикс веток клонов.
const BRANCH_PREFIX: &str = "iapetus/";

/// Созданный клон: то, что нужно для его запуска.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Created {
    /// Рабочий каталог клона: `git worktree` рядом с указанным каталогом или сам указанный каталог.
    dir: PathBuf,
    /// Каталог из запроса.
    source: PathBuf,
    /// Ветка клона; есть только у клона в `git worktree`.
    branch: Option<String>,
    /// Рабочий каталог родителя.
    parent: PathBuf,
}

/// Что родитель знает о себе и server: по этому он запускает клон. Клон такой записи не имеет и клонов не создаёт.
#[derive(Debug)]
pub(super) struct Parent {
    /// Рабочий каталог родителя.
    pub(super) workdir: PathBuf,
    /// Каталог личности родителя: личности клонов лежат в его подкаталоге `clones`.
    pub(super) identity_dir: PathBuf,
    /// Исполняемый файл waldo, которым запускается клон.
    pub(super) exe: PathBuf,
    pub(super) fingerprint: Fingerprint,
    pub(super) orbit_url: String,
    pub(super) orbit_ca: PathBuf,
    /// Ключ доступа к server: клон получает его в каталоге личности, а не в аргументах и окружении процесса.
    pub(super) orbit_key: String,
    /// Клон отдаёт `$HOME`, как родитель.
    pub(super) home: bool,
}

/// Подкаталог каталога личности родителя для личностей его клонов.
const CLONES_DIR: &str = "clones";

/// Создаёт клон и запускает его процесс; регистрацию у server клон проходит сам при первом подключении. При отказе
/// запуска клон убирается, чтобы не осталось каталога без хозяина.
pub(super) async fn start_clone(parent: &Parent, requested: &str) -> CloneResponse {
    let created = create(&parent.workdir, requested).await?;
    match launch(parent, &created) {
        Ok(reply) => Ok(reply),
        Err(error) => {
            if let Err(e) = discard(&created).await {
                tracing::warn!(dir = %created.dir.display(), error = %e, "клон без запуска не убран");
            }
            Err(error)
        }
    }
}

/// Личность клона в подкаталоге родителя и его процесс: `waldo runner --temporary --parent <fp>` в каталоге клона.
/// Окружение команд клона — `.env` указанного каталога: в `git worktree` его нет, он не под git.
fn launch(parent: &Parent, created: &Created) -> Result<CloneReply, Error> {
    let name = created
        .dir
        .file_name()
        .map_or_else(|| "clone".to_owned(), |name| name.to_string_lossy().into_owned());
    let identity_dir = new_identity_dir(&parent.identity_dir.join(CLONES_DIR), &name).map_err(io_error)?;
    let result = spawn(parent, created, &name, &identity_dir);
    if result.is_err()
        && let Err(e) = std::fs::remove_dir_all(&identity_dir)
    {
        tracing::warn!(dir = %identity_dir.display(), error = %e, "личность неудавшегося клона не удалена");
    }
    result
}

fn spawn(parent: &Parent, created: &Created, name: &str, identity_dir: &Path) -> Result<CloneReply, Error> {
    let identity = Identity::load_or_create(identity_dir).map_err(io_error)?;
    identity.store_orbit_key(&parent.orbit_key).map_err(io_error)?;
    let fingerprint = identity.fingerprint().clone();
    let mut command = Command::new(&parent.exe);
    command
        .arg("runner")
        .arg(&created.dir)
        .args(["--temporary", "--parent", parent.fingerprint.as_str(), "--name", name, "--orbit-url", &parent.orbit_url])
        .args(["--home", if parent.home { "on" } else { "off" }])
        .arg("--orbit-ca")
        .arg(&parent.orbit_ca)
        .arg("--identity-dir")
        .arg(identity_dir)
        .arg("--env-file")
        .arg(created.source.join(".env"))
        .current_dir(&created.dir)
        .stdin(Stdio::null())
        // Свой процесс-группа: остановка родителя клона не трогает.
        .process_group(0);
    let mut child = command.spawn().map_err(|e| Error::Io(format!("клон не запущен: {e}")))?;
    let mark = child.id().map(clone_process);
    tokio::spawn(async move {
        let status = child.wait().await;
        tracing::info!(?status, "клон завершился");
        drop(mark);
    });
    Ok(CloneReply {
        fingerprint,
        name: name.to_owned(),
    })
}

/// Новый пустой подкаталог `<prefix>-<N>` в `base`: имя занято ровно один раз.
fn new_identity_dir(base: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(base)?;
    for number in 1..=MAX_CLONES {
        let dir = base.join(format!("{prefix}-{number}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "свободное имя личности клона не найдено за {MAX_CLONES} попыток"
    )))
}

fn io_error(error: impl std::fmt::Display) -> Error {
    Error::Io(format!("{error:#}"))
}

/// Создаёт клон в каталоге `requested`. Каталог должен существовать, быть абсолютным путём, не пересекаться с путями
/// контейнера session и не совпадать с `parent`, не быть вложенным в него или объемлющим его: сравниваются пути после
/// разрешения ссылок и `..`.
async fn create(parent: &Path, requested: &str) -> Result<Created, Error> {
    let parent = std::fs::canonicalize(parent).map_err(|e| Error::Io(format!("каталог родителя {}: {e}", parent.display())))?;
    let source = resolve(&parent, requested)?;
    if !is_repository_root(&source).await? {
        return Ok(Created {
            dir: source.clone(),
            source,
            branch: None,
            parent,
        });
    }
    let (Some(base), Some(siblings)) = (source.file_name(), source.parent()) else {
        return Err(Error::Io("у репозитория нет соседнего каталога для клона".to_owned()));
    };
    let base = base.to_string_lossy();
    for number in 1..=MAX_CLONES {
        let name = format!("{base}-clone-{number}");
        let dir = siblings.join(&name);
        let branch = format!("{BRANCH_PREFIX}{name}");
        if dir.symlink_metadata().is_ok() || branch_exists(&source, &branch).await? {
            continue;
        }
        let dir_arg = dir.to_string_lossy();
        git_ok(&source, &["worktree", "add", "-b", &branch, &dir_arg], None).await?;
        return Ok(Created {
            dir,
            source,
            branch: Some(branch),
            parent,
        });
    }
    Err(Error::Io(format!("свободное имя клона не найдено за {MAX_CLONES} попыток")))
}

/// Каталог из запроса без ссылок и `..`, если он существует и не пересекается ни с путями контейнера session, ни с
/// каталогом родителя.
fn resolve(parent: &Path, requested: &str) -> Result<PathBuf, Error> {
    let requested = Path::new(requested);
    if !requested.is_absolute() {
        return Err(Error::Io(format!("путь {} не абсолютный", requested.display())));
    }
    let dir = std::fs::canonicalize(requested).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound,
        _ => Error::Io(format!("каталог {}: {e}", requested.display())),
    })?;
    if !dir.is_dir() {
        return Err(Error::Io(format!("{} не каталог", dir.display())));
    }
    forbidden::check(&dir).map_err(Error::Io)?;
    if dir.starts_with(parent) {
        return Err(Error::Io(format!(
            "{} совпадает с каталогом родителя или вложен в него",
            dir.display()
        )));
    }
    if parent.starts_with(&dir) {
        return Err(Error::Io(format!("каталог родителя вложен в {}", dir.display())));
    }
    Ok(dir)
}

/// `dir` — корень рабочей копии репозитория: не подкаталог и не каталог вне репозитория.
async fn is_repository_root(dir: &Path) -> Result<bool, Error> {
    let output = git(dir, &["rev-parse", "--show-toplevel"], None).await?;
    if !output.status.success() {
        return Ok(false);
    }
    let top = String::from_utf8_lossy(&output.stdout);
    Ok(std::fs::canonicalize(top.trim()).is_ok_and(|top| top == dir))
}

/// Общий каталог `.git` репозитория, если `dir` — дополнительная рабочая копия на ветке клона. Автокоммит и
/// удаление разрешены только такой копии: основной репозиторий, подкаталоги и любая ветка, кроме созданной клоном
/// (`iapetus/<имя каталога>`), не трогаются.
async fn clone_worktree_origin(dir: &Path) -> Result<Option<PathBuf>, Error> {
    if !is_repository_root(dir).await? {
        return Ok(None);
    }
    let own = git_ok(dir, &["rev-parse", "--path-format=absolute", "--git-dir"], None).await?;
    let common = git_ok(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None).await?;
    if own == common {
        return Ok(None);
    }
    let branch = git_ok(dir, &["branch", "--show-current"], None).await?;
    let Some(name) = dir.file_name() else {
        return Ok(None);
    };
    if branch.trim() != format!("{BRANCH_PREFIX}{}", name.to_string_lossy()) {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(common.trim())))
}

async fn branch_exists(repo: &Path, branch: &str) -> Result<bool, Error> {
    let output = git(repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")], None).await?;
    Ok(output.status.success())
}

/// Убирает клон без изменений: каталог и ветку. Для клона, который не удалось запустить; у клона без git
/// убирать нечего, указанный каталог остаётся нетронутым.
async fn discard(created: &Created) -> Result<(), Error> {
    let Some(branch) = &created.branch else {
        return Ok(());
    };
    let dir = created.dir.to_string_lossy();
    git_ok(&created.source, &["worktree", "remove", "--force", &dir], None).await?;
    git_ok(&created.source, &["branch", "-D", branch], None).await?;
    Ok(())
}

/// Коммитит в ветку клона всё, что изменилось: новые, изменённые и удалённые файлы, кроме игнорируемых
/// `.gitignore`. `true`, если коммит создан. Шаги повторяемы: после обрыва между `add` и `commit` повторный вызов
/// доводит дело до конца, после `commit` — ничего не делает.
async fn autocommit(dir: &Path, author: &Author) -> Result<bool, Error> {
    git_ok(dir, &["add", "-A"], None).await?;
    let staged = git(dir, &["diff", "--cached", "--quiet"], None).await?;
    match staged.status.code() {
        Some(0) => return Ok(false),
        Some(1) => {}
        _ => return Err(failure(&["diff", "--cached", "--quiet"], &staged)),
    }
    // Хуки и подпись пользователя не должны мешать сохранить изменения.
    let commit = [
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--no-verify",
        "--quiet",
        "-m",
        AUTOCOMMIT_MESSAGE,
    ];
    git_ok(dir, &commit, Some(author)).await?;
    Ok(true)
}

/// Гибель клона. Клон в `git worktree`: изменения коммитятся в его ветку, затем рабочая копия удаляется
/// безусловно, ветка остаётся. Если коммит не удался, а изменения есть, они пропадут вместе с копией: пути пишутся в
/// журнал как ошибка и возвращаются. Клон, у которого не было ни одной сессии, не коммитит и рабочую копию не трогает:
/// без сессии нет автора коммитов. Клон без git и любой каталог, не являющийся рабочей копией клона, не меняются.
pub(super) async fn finish(dir: &Path, lifetime: &Lifetime) -> Vec<String> {
    let common = match clone_worktree_origin(dir).await {
        Ok(Some(common)) => common,
        Ok(None) => {
            tracing::info!(dir = %dir.display(), "клон без git: каталог не тронут");
            return Vec::new();
        }
        Err(e) => {
            tracing::error!(dir = %dir.display(), error = %e, "вид клона не определён: каталог не тронут");
            return Vec::new();
        }
    };
    let Some(author) = lifetime.author() else {
        tracing::warn!(dir = %dir.display(), "у клона не было сессии: рабочая копия и ветка оставлены как есть");
        return Vec::new();
    };
    let lost = match autocommit(dir, &author).await {
        Ok(true) => {
            tracing::info!(dir = %dir.display(), "изменения клона закоммичены");
            Vec::new()
        }
        Ok(false) => Vec::new(),
        Err(reason) => {
            let lost = uncommitted_paths(dir).await;
            if !lost.is_empty() {
                tracing::error!(dir = %dir.display(), %reason, ?lost, "изменения клона не закоммичены и пропадают вместе с рабочей копией");
            }
            lost
        }
    };
    let dir_arg = dir.to_string_lossy();
    match git_ok(&common, &["worktree", "remove", "--force", &dir_arg], None).await {
        Ok(_) => tracing::info!(dir = %dir.display(), "рабочая копия клона удалена"),
        Err(e) => tracing::error!(dir = %dir.display(), error = %e, "рабочая копия клона не удалена"),
    }
    lost
}

/// Пути с несохранёнными изменениями: изменённые, удалённые и новые файлы.
async fn uncommitted_paths(dir: &Path) -> Vec<String> {
    match git_ok(dir, &["status", "--porcelain", "--untracked-files=all"], None).await {
        Ok(status) => status.lines().filter_map(|line| line.get(3..)).map(str::to_owned).collect(),
        Err(e) => {
            tracing::error!(dir = %dir.display(), error = %e, "список несохранённых изменений клона не получен");
            Vec::new()
        }
    }
}

async fn git(dir: &Path, args: &[&str], author: Option<&Author>) -> Result<Output, Error> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(author) = author {
        command.envs(session_vars(author.clone(), None));
    }
    command.output().await.map_err(|e| Error::Io(format!("git не запущен: {e}")))
}

async fn git_ok(dir: &Path, args: &[&str], author: Option<&Author>) -> Result<String, Error> {
    let output = git(dir, args, author).await?;
    if !output.status.success() {
        return Err(failure(args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn failure(args: &[&str], output: &Output) -> Error {
    Error::Io(format!(
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Что привело к остановке клона.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// За [`IDLE_STOP`] сессия не назначена.
    Idle,
    /// Назначенная сессия закончилась: отключилась или переключилась на другой runner.
    SessionEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Assigned,
    Ended,
}

/// Жизнь временного runner: ожидание сессии, сессия, конец.
#[derive(Debug)]
pub(super) struct Lifetime {
    phase: watch::Sender<Phase>,
    /// Автор последней начатой сессии: от его имени идёт автокоммит.
    author: Mutex<Option<Author>>,
    /// Личность клона: автор хранится в ней, и перезапущенный клон остаётся с автором прежней сессии.
    identity: Option<Arc<Identity>>,
    idle_deadline: Instant,
}

impl Lifetime {
    /// Клон без личности: автор живёт только в памяти. Отсчёт ожидания сессии идёт с создания.
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_author(None, None)
    }

    /// Клон с личностью: автор прежней сессии, если она была, берётся из каталога личности, а каждая начатая сессия
    /// записывает своего.
    pub(super) fn restored(identity: Arc<Identity>) -> Self {
        let author = identity.author().unwrap_or_else(|e| {
            tracing::error!(error = format!("{e:#}"), "автор клона не прочитан: сохранённая сессия не учтена");
            None
        });
        Self::with_author(author, Some(identity))
    }

    fn with_author(author: Option<Author>, identity: Option<Arc<Identity>>) -> Self {
        Self {
            phase: watch::channel(Phase::Waiting).0,
            author: Mutex::new(author),
            identity,
            idle_deadline: Instant::now() + IDLE_STOP,
        }
    }

    /// Начало назначения отменяет остановку по простою и запоминает автора.
    pub(super) fn start(&self, author: &Author) {
        if let Some(identity) = &self.identity
            && let Err(e) = identity.store_author(author)
        {
            tracing::error!(error = format!("{e:#}"), "автор клона не сохранён: после перезапуска его не будет");
        }
        *self.author.lock().unwrap_or_else(PoisonError::into_inner) = Some(author.clone());
        self.phase.send_modify(|phase| {
            if *phase == Phase::Waiting {
                *phase = Phase::Assigned;
            }
        });
    }

    /// Конец назначенного назначения (отключение или переключение на другой runner) останавливает клон. Конец без
    /// начала игнорируется: это не его назначение.
    pub(super) fn end(&self) {
        self.phase.send_modify(|phase| {
            if *phase == Phase::Assigned {
                *phase = Phase::Ended;
            }
        });
    }

    fn author(&self) -> Option<Author> {
        self.author.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Ждёт, пока клон не должен остановиться.
    async fn stopped(&self) -> Stop {
        let mut phase = self.phase.subscribe();
        loop {
            let current = *phase.borrow_and_update();
            match current {
                Phase::Ended => return Stop::SessionEnded,
                Phase::Assigned => phase.changed().await.unwrap_or_default(),
                Phase::Waiting => {
                    tokio::select! {
                        () = sleep_until(self.idle_deadline) => {
                            // Назначение могло прийти вместе со сроком: оно главнее.
                            if *phase.borrow() == Phase::Waiting {
                                return Stop::Idle;
                            }
                        }
                        changed = phase.changed() => changed.unwrap_or_default(),
                    }
                }
            }
        }
    }
}

/// Ждёт остановки временного runner; обычный runner не останавливается никогда.
pub(super) async fn stopped(lifetime: Option<&Lifetime>) -> Stop {
    match lifetime {
        Some(lifetime) => lifetime.stopped().await,
        None => std::future::pending().await,
    }
}
