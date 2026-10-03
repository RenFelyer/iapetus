//! Выполнение команд runner: `bash -c` в каталоге внутри рабочего, в своей группе процессов.
//!
//! Модуль не знает о server: команда приходит как [`ExecRequest`], а вывод, heartbeat и завершение уходят в канал
//! `mpsc` потоком [`ExecEvent`]; слой окружения server ему отдаёт `control::env`. Команды приходят только по каналу
//! данных сессии ([`crate::runner::data`]).
//!
//! Окружение команды собирает `env`, значения секретов в выводе заменяются на `***` (`mask`).

mod env;
mod mask;
mod pump;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use nerve::protocol::{Assignment, Author, Error, ExecEvent, ExecExit, ExecRequest, ExitStatus};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, sleep, sleep_until};

use super::control::env::ServerEnv;
use super::fs::Workdir;
use super::reaper;
use env::Layers;
pub(in crate::runner) use env::Resolved;
pub(in crate::runner) use mask::{Masker, Secrets};
use pump::pump;

#[cfg(test)]
#[path = "../../tests/runner/exec.rs"]
mod tests;

/// Сколько команда ждёт свежего снимка окружения server, прежде чем отказаться запускаться: переподключение
/// обычно укладывается в этот срок.
const SERVER_ENV_WAIT: Duration = Duration::from_secs(10);

/// Сколько ждать после SIGTERM, прежде чем убить группу процессов SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Скрипт запуска: команда — первый аргумент, файл для cwd после неё — второй.
///
/// cwd пишет ловушка на `EXIT`: только она срабатывает при любом выходе — конце команды, `exit`, `set -e`,
/// `set -u`. Файл открывается на отдельный дескриптор до команды, а позиционные параметры очищаются: `set --` в
/// команде не перенаправит запись в чужой файл. Собственная ловушка команды на `EXIT` заменила бы ловушку runner,
/// поэтому `trap` подменён функцией: ловушку `EXIT` главной оболочки она запоминает, и ловушка runner вызывает её
/// после записи cwd с тем же `$?` и позиционными параметрами; `trap -p` показывает ловушку команды, а не runner.
/// Остальные сигналы и подоболочки идут прямо во встроенный `trap`. Обойти подмену можно только нарочно
/// (`builtin trap`, `command trap`, `unset -f trap`) — тогда cwd не придёт, и ответ несёт cwd запроса.
/// Ловушка не меняет код выхода, а `exit` в ловушке команды задаёт его, как без runner.
const SCRIPT: &str = r#"__iapetus_command=$1
exec {__iapetus_cwd}>"$2" || exit 125
set --
__iapetus_main=$BASHPID
__iapetus_status() { return "$1"; }
__iapetus_on_exit() {
    local status=$?
    builtin pwd -P 2>/dev/null >&"$__iapetus_cwd" || :
    if [[ -v __iapetus_exit ]]; then
        __iapetus_status "$status"
        builtin eval "$__iapetus_exit"
    fi
}
trap() {
    [[ ${1-} == -- ]] && shift
    local action reset= mode= sig others=()
    if [[ $# -eq 0 ]]; then mode=-p
    elif [[ $1 == -p || $1 == -P ]]; then mode=$1; shift
    elif [[ $1 == -* && $1 != - ]]; then builtin trap "$@"; return
    fi
    if [[ -n $mode ]]; then
        local out ours='__iapetus_on_exit "$@"' theirs=
        [[ $mode == -p ]] && ours="trap -- ${ours@Q} EXIT"
        out=$(builtin trap "$mode" "$@") || return
        if [[ -v __iapetus_exit && $mode == -P ]]; then theirs=$__iapetus_exit
        elif [[ -v __iapetus_exit ]]; then theirs="trap -- ${__iapetus_exit@Q} EXIT"
        fi
        out=${out/"$ours"/"$theirs"}
        out=${out//$'\n\n'/$'\n'}
        out=${out#$'\n'}
        out=${out%$'\n'}
        [[ -n $out ]] && printf '%s\n' "$out"
        return 0
    fi
    if [[ $BASHPID != "$__iapetus_main" ]]; then builtin trap "$@"; return; fi
    if [[ $1 == - ]]; then reset=1; shift
    elif [[ $# -eq 1 ]]; then reset=1
    else action=$1; shift
    fi
    for sig; do
        case $sig in
        0 | [Ee][Xx][Ii][Tt])
            if [[ -n $reset ]]; then unset __iapetus_exit; else __iapetus_exit=$action; fi ;;
        *) others+=("$sig") ;;
        esac
    done
    if [[ ${#others[@]} -eq 0 ]]; then return 0
    elif [[ -n $reset ]]; then builtin trap - "${others[@]}"
    else builtin trap -- "$action" "${others[@]}"
    fi
}
builtin trap '__iapetus_on_exit "$@"' EXIT
builtin eval "$__iapetus_command"
"#;

/// Переменные автора коммитов, которыми сессия перекрывает окружение.
const GIT_AUTHOR_NAME: &str = "GIT_AUTHOR_NAME";
const GIT_AUTHOR_EMAIL: &str = "GIT_AUTHOR_EMAIL";
const GIT_COMMITTER_NAME: &str = "GIT_COMMITTER_NAME";
const GIT_COMMITTER_EMAIL: &str = "GIT_COMMITTER_EMAIL";
const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

/// Исполнитель команд runner.
#[derive(Debug)]
pub(super) struct Executor {
    workdir: Arc<Workdir>,
    /// Путь к `.env`; файл читается при каждой команде, поэтому правки видны сразу.
    env_file: PathBuf,
    /// Переменные системы: снимок при запуске runner.
    system: Vec<(OsString, OsString)>,
    /// `None` — runner без server: команды идут без его слоя.
    server_env: Option<Arc<ServerEnv>>,
    state: Mutex<State>,
    /// Будится, когда выполняющаяся команда уходит из `running`.
    left: Notify,
}

#[derive(Debug, Default)]
struct State {
    /// Назначение, которому служит исполнитель; `None` — назначения нет и команды не запускаются.
    assignment: Option<Assignment>,
    /// Переменные сессии: живут только в памяти, привязаны к назначению и сбрасываются в его конце.
    session: BTreeMap<String, String>,
    /// Выполняющиеся команды: по `Notify` команду останавливают.
    running: HashMap<u64, Arc<Notify>>,
    /// Группы процессов всех команд назначения, в том числе уже завершившихся: фоновые процессы живут дольше команды, а
    /// при конце назначения убиваются.
    groups: Vec<Pid>,
}

impl Executor {
    pub(super) fn new(
        workdir: Arc<Workdir>,
        env_file: PathBuf,
        system: Vec<(OsString, OsString)>,
        server_env: Option<Arc<ServerEnv>>,
    ) -> Self {
        Self {
            workdir,
            env_file,
            system,
            server_env,
            state: Mutex::default(),
            left: Notify::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Начало назначения: команды получают его переменные сессии. Прежнее назначение к этому времени остановлено (`stop`).
    pub(super) fn start(&self, assignment: Assignment, session: BTreeMap<String, String>) {
        let mut state = self.state();
        state.assignment = Some(assignment);
        state.session = session;
        tracing::info!(?assignment, "сессия началась");
    }

    /// Конец назначения: сбрасывает переменные сессии, отменяет его команды (SIGTERM, через `KILL_GRACE` SIGKILL), дожидается
    /// их, затем убивает группы процессов всех команд назначения, в том числе фоновые, и их потомков, ушедших из группы, и снимает наблюдение за каталогами.
    pub(super) async fn stop(&self) {
        let (assignment, running): (Option<Assignment>, Vec<Arc<Notify>>) = {
            let mut state = self.state();
            let assignment = state.assignment.take();
            state.session.clear();
            (assignment, state.running.values().cloned().collect())
        };
        tracing::info!(?assignment, running = running.len(), "сессия закончилась, команды останавливаются");
        for command in running {
            command.notify_one();
        }
        loop {
            let left = self.left.notified();
            if self.state().running.is_empty() {
                break;
            }
            left.await;
        }
        let groups = std::mem::take(&mut self.state().groups);
        for group in groups {
            kill_process_group(group, Signal::KILL).unwrap_or_default();
        }
        // Ушедшие из группы (`setsid`, двойной `fork`) держат токен сессии в окружении: группой их не достать.
        reaper::stop_descendants().await;
        self.workdir.end_session();
        tracing::info!(?assignment, "команды сессии остановлены");
    }

    /// Сессия не вернулась к runner: группы процессов всех команд назначения — и выполняющихся, и закончившихся, — получают
    /// SIGTERM, через `KILL_GRACE` SIGKILL, как при отмене; так же — процессы команд, ушедшие из группы. Назначение и
    /// переменные сессии остаются: итоги убитых команд сессия заберёт, когда вернётся.
    pub(super) async fn abandon(&self) {
        let groups = self.state().groups.clone();
        for group in groups {
            terminate(Some(group));
        }
        reaper::stop_commands().await;
    }

    /// Останавливает команду по id; `NotFound`, если такой не выполняется.
    pub(super) fn cancel(&self, id: u64) -> Result<(), Error> {
        let command = self.state().running.get(&id).cloned().ok_or(Error::NotFound)?;
        command.notify_one();
        Ok(())
    }

    /// Окружение процесса назначения вне команды (проектный MCP-сервер): слои те же, что у команд, `extra` поверх всех.
    pub(in crate::runner) async fn process_env(&self, assignment: Assignment, extra: BTreeMap<String, String>) -> Result<Resolved, Error> {
        let mut layers = self.layers(assignment).await?;
        layers.session.extend(extra);
        Ok(layers.resolve())
    }

    /// Рабочий каталог runner.
    pub(in crate::runner) fn project_dir(&self) -> Result<PathBuf, Error> {
        self.workdir.resolve_cwd(".")
    }

    /// Выполняет команду и шлёт события в `events`; последним уходит `Exit` или `Err`. Если получатель событий
    /// исчез, команда останавливается.
    pub(super) async fn run(&self, assignment: Assignment, request: ExecRequest, events: mpsc::Sender<ExecEvent>) {
        let id = request.id;
        let stop = Arc::new(Notify::new());
        if let Err(error) = self.register(assignment, id, &stop) {
            // Запись выполняющейся команды остаётся нетронутой.
            events.send(ExecEvent::Err(error)).await.unwrap_or_default();
            return;
        }
        let last = match self.run_inner(assignment, request, &stop, &events).await {
            Ok(exit) => ExecEvent::Exit(exit),
            Err(error) => ExecEvent::Err(error),
        };
        self.state().running.remove(&id);
        self.left.notify_waiters();
        // Получателя могло уже не быть: тогда слать некому.
        events.send(last).await.unwrap_or_default();
    }

    /// Заносит команду в список выполняющихся. Ошибка, если `assignment` уже не текущее назначение или команда с таким id
    /// уже выполняется.
    fn register(&self, assignment: Assignment, id: u64, stop: &Arc<Notify>) -> Result<(), Error> {
        let mut state = self.state();
        if state.assignment != Some(assignment) {
            return Err(session_ended());
        }
        // Группы, в которых никого не осталось, больше не нужны, а их номера могут достаться чужим процессам.
        state.groups.retain(|group| test_kill_process_group(*group).is_ok());
        match state.running.entry(id) {
            Entry::Occupied(_) => Err(Error::Conflict),
            Entry::Vacant(slot) => {
                slot.insert(Arc::clone(stop));
                Ok(())
            }
        }
    }

    async fn run_inner(
        &self,
        assignment: Assignment,
        request: ExecRequest,
        stop: &Notify,
        events: &mpsc::Sender<ExecEvent>,
    ) -> Result<ExecExit, Error> {
        let cwd = self.workdir.resolve_cwd(&request.cwd)?;
        // Конец назначения не ждёт снимка окружения.
        let layers = tokio::select! {
            layers = self.layers(assignment) => layers?,
            () = stop.notified() => return Err(session_ended()),
        };
        let Resolved { mut vars, secrets } = layers.resolve();
        // Метка находит процессы команды, ушедшие из её группы.
        vars.insert(reaper::COMMAND_VAR.into(), "1".into());
        let cwd_file = tempfile::NamedTempFile::new().map_err(|e| Error::Io(format!("файл cwd не создан: {e}")))?;
        let mut child = Command::new("bash")
            .args(["-c", SCRIPT, "bash", &request.command])
            .arg(cwd_file.path())
            .current_dir(&cwd)
            .env_clear()
            .envs(&vars)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Своя группа: отмена убивает всё дерево, а не только bash.
            .process_group(0)
            .spawn()
            .map_err(|e| Error::Io(format!("bash не запущен: {e}")))?;
        // bash ждёт сам runner: его зомби не забирается вместе с усыновлёнными.
        let _own = child.id().map(reaper::own_child);
        let group = child.id().and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?));
        if let Some(group) = group {
            self.state().groups.push(group);
        }
        let mut guard = GroupGuard(group);
        let (stdout, stderr) = (
            child.stdout.take().ok_or_else(|| Error::Io("у bash нет stdout".to_owned()))?,
            child.stderr.take().ok_or_else(|| Error::Io("у bash нет stderr".to_owned()))?,
        );
        let (exited_tx, exited_rx) = oneshot::channel();
        let sink = events.clone();
        let output = tokio::spawn(async move { pump(stdout, stderr, secrets, &sink, exited_rx).await });

        let status = wait(&mut child, group, stop, events, request.timeout_ms).await?;
        // Вывод дочитывается, пока труба открыта, но не дольше `DRAIN_GRACE`. Получатель мог исчезнуть: тогда слать
        // уже некому, и это не ошибка команды.
        exited_tx.send(()).unwrap_or_default();
        output
            .await
            .map_err(|e| Error::Io(format!("поток вывода упал: {e}")))?
            .unwrap_or_default();
        // Фоновые процессы, пережившие команду, остаются до конца назначения или потери сессии.
        guard.0 = None;

        let after = tokio::fs::read_to_string(cwd_file.path()).await.unwrap_or_default();
        let after = after.trim_end_matches('\n');
        Ok(ExecExit {
            status,
            cwd: if after.is_empty() {
                cwd.display().to_string()
            } else {
                after.to_owned()
            },
        })
    }

    /// Слои окружения для новой команды. Без свежего снимка окружения server команда не запускается: со старыми значениями или
    /// без переменных server она молча ушла бы не туда. Если за ожиданием снимка назначение кончилось, команда не
    /// запускается тоже: переменные сессии принадлежат назначению.
    async fn layers(&self, assignment: Assignment) -> Result<Layers, Error> {
        let dotenv = match tokio::fs::read_to_string(&self.env_file).await {
            Ok(text) => nerve::dotenv::parse_dotenv(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(file = %self.env_file.display(), error = %e, ".env не прочитан");
                BTreeMap::new()
            }
        };
        let server = match &self.server_env {
            Some(env) => env
                .vars(SERVER_ENV_WAIT)
                .await
                .map_err(|reason| Error::Io(format!("{reason}, команда не запущена")))?,
            None => BTreeMap::new(),
        };
        let state = self.state();
        if state.assignment != Some(assignment) {
            return Err(session_ended());
        }
        Ok(Layers {
            system: self.system.clone(),
            dotenv,
            server,
            session: state.session.clone(),
        })
    }
}

fn session_ended() -> Error {
    Error::Io("сессия закончилась, команда не запущена".to_owned())
}

/// Переменные автора коммитов и `GITHUB_TOKEN` сессии.
pub(super) fn session_vars(author: Author, github_token: Option<String>) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::from([
        (GIT_AUTHOR_NAME.to_owned(), author.name.clone()),
        (GIT_AUTHOR_EMAIL.to_owned(), author.email.clone()),
        (GIT_COMMITTER_NAME.to_owned(), author.name),
        (GIT_COMMITTER_EMAIL.to_owned(), author.email),
    ]);
    if let Some(token) = github_token {
        vars.insert(GITHUB_TOKEN.to_owned(), token);
    }
    vars
}

/// Ждёт завершения bash; по отмене, таймауту или исчезновению получателя останавливает дерево процессов.
async fn wait(
    child: &mut Child,
    group: Option<Pid>,
    stop: &Notify,
    events: &mpsc::Sender<ExecEvent>,
    timeout_ms: Option<u64>,
) -> Result<ExitStatus, Error> {
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let timeout = async {
        match deadline {
            Some(deadline) => sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timeout);
    let mut stopping = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            () = stop.notified(), if !stopping => {
                stopping = true;
                terminate(group);
            }
            () = &mut timeout, if !stopping => {
                stopping = true;
                terminate(group);
            }
            () = events.closed(), if !stopping => {
                stopping = true;
                terminate(group);
            }
        }
    };
    let status = status.map_err(|e| Error::Io(format!("ожидание bash не удалось: {e}")))?;
    exit_status(status)
}

fn exit_status(status: std::process::ExitStatus) -> Result<ExitStatus, Error> {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => Ok(ExitStatus::Code(code)),
        (None, Some(signal)) => Ok(ExitStatus::Signal(signal)),
        (None, None) => Err(Error::Io("bash завершился без кода и сигнала".to_owned())),
    }
}

/// SIGTERM всей группе; если после `KILL_GRACE` в ней кто-то остался — SIGKILL. Bash мог выйти сразу, а внук,
/// который игнорирует SIGTERM, — остаться, поэтому ждать выхода bash для эскалации нельзя.
fn terminate(group: Option<Pid>) {
    let Some(group) = group else { return };
    kill_process_group(group, Signal::TERM).unwrap_or_default();
    tokio::spawn(async move {
        sleep(KILL_GRACE).await;
        if test_kill_process_group(group).is_ok() {
            kill_process_group(group, Signal::KILL).unwrap_or_default();
        }
    });
}

/// Убивает группу процессов, если команда прервана посреди работы: задача отменена, например при остановке runner.
#[derive(Debug)]
struct GroupGuard(Option<Pid>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            kill_process_group(group, Signal::KILL).unwrap_or_default();
        }
    }
}
