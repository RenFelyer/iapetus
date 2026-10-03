//! Запуск агента без лишних прав.
//!
//! Сессия работает от root: она монтирует FUSE и размонтирует его при остановке. Агент получает уже урезанный
//! процесс: из bounding set убран `CAP_SYS_ADMIN`, inheritable очищен, `no_new_privs` включён, группы сброшены, uid и
//! gid — обычного пользователя. Такой процесс не может размонтировать, перемонтировать или смонтировать поверх файловую систему.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use pty_process::Pty;
use rustix::process::{Pid, Signal, getegid, geteuid, kill_process_group};
use rustix::thread::{
    CapabilitySet, Gid, Uid, capabilities, remove_capability_from_bounding_set, set_capabilities, set_no_new_privs, set_thread_groups,
    set_thread_res_gid, set_thread_res_uid,
};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::data::Link;
use super::host::Environment;
use super::mcp::Project;
use super::state::{AgentEvent, record_agent_exit, record_agent_relaunch, report_agent_exit};
use super::terminal::Terminal;
use super::update::UpdateRequest;

/// Эффективные uid и gid текущего потока равны владельцу, пока значение живёт: файлы создаются сразу его, без
/// `chown`, которому нужна `CAP_CHOWN`, а её у контейнера нет. Реальный и сохранённый uid остаются прежними, и
/// `Drop` возвращает права; поток не должен уходить на `.await`, пока значение живо.
pub(super) struct AsOwner {
    uid: Uid,
    gid: Gid,
}

impl AsOwner {
    pub(super) fn enter(uid: u32, gid: u32) -> std::io::Result<Self> {
        let before = Self {
            uid: geteuid(),
            gid: getegid(),
        };
        set_thread_res_gid(None, Gid::from_raw(gid), None)?;
        if let Err(error) = set_thread_res_uid(None, Uid::from_raw(uid), None) {
            set_thread_res_gid(None, before.gid, None)?;
            return Err(error.into());
        }
        Ok(before)
    }
}

impl Drop for AsOwner {
    fn drop(&mut self) {
        // Возврат к прежним uid и gid разрешён сохранённым uid. Сбой оставил бы поток runtime с чужими правами и без
        // `CAP_SYS_ADMIN`, который нужен для размонтирования FUSE: процесс останавливается.
        if set_thread_res_uid(None, self.uid, None)
            .and_then(|()| set_thread_res_gid(None, self.gid, None))
            .is_err()
        {
            tracing::error!("uid и gid потока не возвращены после работы от имени пользователя агента");
            std::process::abort();
        }
    }
}

/// `TERM` агента, если `env` его не задаёт: окна зрителей — обычные эмуляторы терминала.
const DEFAULT_TERM: &str = "xterm-256color";

/// Переменные сессии, которые агент наследует: язык (`LANG`, `LC_*`), часовой пояс и поиск программ.
/// Переменные состояния агента задаёт его профиль. Учётных данных агента здесь нет: он входит сам. Остальное
/// окружение сессии (секрет сессии, ключ хоста, адреса server) агенту не нужно и не передаётся.
const INHERITED_ENV: &[&str] = &["PATH", "LANG", "TZ"];

fn is_inherited(name: &str) -> bool {
    INHERITED_ENV.contains(&name) || name.starts_with("LC_")
}

/// Скрытая подкоманда `orbit drop-exec`: понижает права и заменяет процесс командой.
#[derive(Debug, clap::Args)]
pub(crate) struct DropExecArgs {
    #[arg(long)]
    uid: u32,
    #[arg(long)]
    gid: u32,
    /// Значение `HOME` для агента: от root оно было бы `/root`
    #[arg(long)]
    home: PathBuf,
    /// Рабочий каталог агента: точка монтирования FUSE; runner узнаёт отсюда, где запускать его команды
    #[arg(long)]
    cwd: PathBuf,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

/// Пользователь, от имени которого работает агент.
#[derive(Debug, Clone)]
pub(super) struct AgentUser {
    pub uid: u32,
    pub gid: u32,
    /// Корень состояния агентов: их файлы лежат здесь, а `HOME` агента — как у runner.
    pub state_root: PathBuf,
}

/// Окружение агента: наследуемые из `INHERITED_ENV` переменные `inherited`, затем `profile` — они главнее.
pub(super) fn agent_environment(inherited: impl Iterator<Item = (String, String)>, profile: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited.filter(|(name, _)| is_inherited(name)).collect();
    env.retain(|(name, _)| !profile.iter().any(|(own, _)| own == name));
    env.extend(profile.iter().cloned());
    env
}

/// Запускает `command` с окружением `env` (и только им: окружение сессии не наследуется) отдельным процессом
/// (`orbit drop-exec` из файла `orbit`), который понижает права и делает `exec`, — в новом pty: тот становится
/// управляющим терминалом агента. Понижение —
/// в дочернем процессе, а не в потоке сессии: у процесса один поток, поэтому `setresuid` меняет его целиком, а
/// `exec` идёт с того же потока.
pub(super) fn spawn(
    orbit: &Path,
    user: &AgentUser,
    home: &Path,
    cwd: &Path,
    env: &[(String, String)],
    command: &[String],
) -> anyhow::Result<(tokio::process::Child, Pty)> {
    let (pty, pts) = pty_process::open().context("pty не открыт")?;
    let child = pty_process::Command::new(orbit)
        .arg("drop-exec")
        .args(["--uid", &user.uid.to_string(), "--gid", &user.gid.to_string()])
        .arg("--home")
        .arg(home)
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .args(command)
        .env_clear()
        .env("TERM", DEFAULT_TERM)
        .envs(env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
        .kill_on_drop(true)
        .spawn(pts)
        .context("процесс агента не запущен")?;
    Ok((child, pty))
}

/// Убирает у процесса всё, что нужно для монтирования, и заменяет его командой. Возвращается только с ошибкой.
///
/// Порядок важен: пока процесс root, он ещё может менять bounding set и наследуемые права; `setresuid` на
/// обычного пользователя после этого сбрасывает permitted и effective, а `no_new_privs` не даёт получить их
/// обратно через `exec` программы с setuid или файловыми правами.
pub(crate) fn drop_and_exec(args: &DropExecArgs) -> anyhow::Result<Infallible> {
    remove_capability_from_bounding_set(CapabilitySet::SYS_ADMIN).context("CAP_SYS_ADMIN не убран из bounding set")?;
    let mut sets = capabilities(None).context("права процесса не прочитаны")?;
    sets.inheritable = CapabilitySet::empty();
    set_capabilities(None, sets).context("inheritable не очищен")?;
    set_no_new_privs(true).context("no_new_privs не включён")?;

    let (uid, gid) = (Uid::from_raw(args.uid), Gid::from_raw(args.gid));
    set_thread_groups(&[]).context("дополнительные группы не сброшены")?;
    set_thread_res_gid(gid, gid, gid).context("gid не сменён")?;
    set_thread_res_uid(uid, uid, uid).context("uid не сменён")?;

    // Агент запускается, когда runner подключён: каталог файловой системы session уже отвечает. Каталог открывается
    // от имени агента, после понижения прав.
    std::env::set_current_dir(&args.cwd).with_context(|| format!("рабочий каталог {} недоступен агенту", args.cwd.display()))?;
    let (program, arguments) = args.command.split_first().context("команда агента пуста")?;
    let error = std::process::Command::new(program).args(arguments).env("HOME", &args.home).exec();
    Err(anyhow::Error::new(error).context(format!("не удалось запустить {program}")))
}

/// Как долго агент закрывается по SIGTERM, прежде чем его убьют.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Сколько выходов агента без указания server за [`EXIT_WINDOW`] считаются циклом: session перестаёт его перезапускать.
const EXITS_IN_LOOP: usize = 3;

/// Окно, в котором считаются выходы агента.
const EXIT_WINDOW: Duration = Duration::from_secs(60);

/// Моменты последних выходов агента без указания server.
#[derive(Debug, Default)]
struct Exits(VecDeque<Instant>);

impl Exits {
    /// Записывает выход в `now`; `true`, если в окне набралось [`EXITS_IN_LOOP`] выходов.
    fn record(&mut self, now: Instant) -> bool {
        self.0.push_back(now);
        while self.0.front().is_some_and(|first| now.duration_since(*first) > EXIT_WINDOW) {
            self.0.pop_front();
        }
        self.0.len() >= EXITS_IN_LOOP
    }
}

/// Указание тому, кто держит процесс агента.
#[derive(Debug)]
pub(super) enum AgentCommand {
    /// Перезапуск с продолжением разговора после конца основного хода. Повторный `Resume`, пока первый ещё ждёт
    /// конца хода, перезапускает агента сразу и обрывает ход.
    Resume,
    /// Остановить сразу и запустить заново; `resume` — с продолжением разговора. Основной ход не ждётся: при смене
    /// runner его не договорить.
    Restart { resume: bool },
    /// Остановить сразу и не запускать, пока не придёт `Restart`.
    Stop,
    /// Остановить, заменить бинарник загруженным и запустить заново с продолжением разговора. Агент останавливается,
    /// только если нет зрителей и основной ход закончен; иначе указание отклоняется и агент не тронут.
    Update(UpdateRequest),
}

/// Что нужно, чтобы планово перезапустить или остановить агента.
pub(super) struct Restarts<'a> {
    /// Указания от server.
    pub commands: mpsc::Receiver<AgentCommand>,
    /// Основной ход идёт: `Resume` ждёт его конца.
    pub turn: watch::Receiver<bool>,
    /// Сколько worker смотрят терминал агента: `Update` не останавливает агента при зрителях.
    pub viewers: watch::Receiver<usize>,
    /// Поднимается перед остановкой агента: такой выход не `failure`.
    pub planned_exit: &'a AtomicBool,
    /// Сводка проектных MCP-серверов: агент читает список инструментов при старте, поэтому запуск ждёт первую сводку.
    pub project: Project,
    /// Окружение runner: агент запускается в его проекте, поэтому запуск ждёт первое назначение.
    pub environment: watch::Receiver<Option<Environment>>,
}

/// Запускает агента через `start(resumed)`, когда подключён runner и пришло окружение: терминал для зрителей кладётся
/// в слот `terminal`, и только после этого агент объявляется запущенным. Первый запуск идёт с `resumed`, каждый
/// следующий после выхода агента — с продолжением разговора.
///
/// Агент, вышедший сам (например, по `/exit`), запускается заново, зрители входят заново. Если он выходит
/// [`EXITS_IN_LOOP`] раза за [`EXIT_WINDOW`], перезапуск прекращается: выход уходит в состояние как `failure`, и
/// функция не возвращается.
///
/// Указания: `Resume` ждёт конца основного хода (повторный не ждёт), `Restart`, `Stop` и `Update` не ждут. Плановая
/// остановка (SIGTERM группе процессов, затем SIGKILL) не считается выходом для [`EXITS_IN_LOOP`]. После `Restart`
/// агент запускается, когда канал данных открыт и пришла первая сводка проектных MCP-серверов или истёк срок её
/// ожидания; после `Stop` — только по следующему `Restart`, вернувшийся runner его не запускает. Указания, пришедшие
/// во время остановки, сводятся к последнему из `Restart` и `Stop`, `Resume` отбрасываются, `Update`, принятый до
/// остановки, выполняется всегда. Пока агента нет, `Update` заменяет бинарник сразу.
pub(super) async fn run_after_runner(
    link: &mut Link,
    mut start: impl FnMut(bool) -> anyhow::Result<(tokio::process::Child, Pty)>,
    mut resumed: bool,
    terminal: &watch::Sender<Option<Terminal>>,
    running: &watch::Sender<bool>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    mut restarts: Restarts<'_>,
) -> anyhow::Result<Infallible> {
    let mut halted = false;
    let mut exits = Exits::default();
    loop {
        if halted {
            match restarts.commands.recv().await {
                Some(AgentCommand::Restart { resume }) => {
                    resumed = resume;
                    halted = false;
                }
                Some(AgentCommand::Update(request)) => request.install(),
                Some(AgentCommand::Resume | AgentCommand::Stop) => {}
                None => return std::future::pending().await,
            }
            continue;
        }
        // Пока runner не подключён, указания всё равно принимаются: `Stop` отменяет запуск.
        let opened = tokio::select! {
            opened = async {
                let opened = link.open().await;
                drop(restarts.environment.wait_for(Option::is_some).await);
                restarts.project.wait_first_catalog().await;
                opened
            } => opened,
            command = restarts.commands.recv() => {
                match command {
                    Some(AgentCommand::Restart { resume }) => resumed = resume,
                    Some(AgentCommand::Stop) => {
                        events.send(AgentEvent::Halted).unwrap_or_default();
                        halted = true;
                    }
                    Some(AgentCommand::Update(request)) => request.install(),
                    Some(AgentCommand::Resume) => {}
                    None => return std::future::pending().await,
                }
                continue;
            }
        };
        opened.context("канал данных закрыт до подключения runner")?;

        let (mut child, pty) = start(resumed)?;
        terminal.send_replace(Some(Terminal::start(pty)));
        running.send_replace(true);
        events.send(AgentEvent::Started).unwrap_or_default();

        let mut command = tokio::select! {
            result = agent_exit(Some(&mut child)) => {
                terminal.send_replace(None);
                if exits.record(Instant::now()) {
                    tracing::error!(exits = EXITS_IN_LOOP, window = ?EXIT_WINDOW, "агент завершается снова и снова, перезапуск остановлен");
                    return Ok(report_agent_exit(async { result }, events, running, restarts.planned_exit).await);
                }
                record_agent_relaunch(result, events, running);
                resumed = true;
                continue;
            }
            command = next_command(&mut restarts.commands, &mut restarts.turn, &restarts.viewers, events) => command?,
        };
        restarts.planned_exit.store(true, Ordering::SeqCst);
        let stopped = stop(&mut child).await;
        terminal.send_replace(None);
        record_agent_exit(stopped, events, running, restarts.planned_exit);
        if let AgentCommand::Update(request) = command {
            request.install();
            command = AgentCommand::Restart { resume: true };
        }
        while let Ok(extra) = restarts.commands.try_recv() {
            match extra {
                AgentCommand::Resume => {}
                AgentCommand::Update(request) => request.install(),
                AgentCommand::Restart { .. } | AgentCommand::Stop => command = extra,
            }
        }
        match command {
            AgentCommand::Resume | AgentCommand::Update(_) => resumed = true,
            AgentCommand::Restart { resume } => resumed = resume,
            AgentCommand::Stop => {
                events.send(AgentEvent::Halted).unwrap_or_default();
                halted = true;
            }
        }
    }
}

/// Ждёт указания, по которому агента нужно остановить: `Restart` и `Stop` — сразу, `Resume` — когда закончится
/// основной ход (пока идёт ход, об ожидании сообщается `RestartPending`), а повторный `Resume` во время ожидания — сразу,
/// `Update` — сразу, если нет зрителей и хода, иначе отклоняется. Без указаний не возвращается.
async fn next_command(
    commands: &mut mpsc::Receiver<AgentCommand>,
    turn: &mut watch::Receiver<bool>,
    viewers: &watch::Receiver<usize>,
    events: &mpsc::UnboundedSender<AgentEvent>,
) -> anyhow::Result<AgentCommand> {
    let mut resume_wanted = false;
    loop {
        let command = if resume_wanted {
            tokio::select! {
                command = commands.recv() => command,
                waited = turn.wait_for(|open| !*open) => {
                    waited.context("слежение за ходом агента закрыто")?;
                    return Ok(AgentCommand::Resume);
                }
            }
        } else {
            commands.recv().await
        };
        match command {
            Some(AgentCommand::Resume) if resume_wanted => return Ok(AgentCommand::Resume),
            Some(AgentCommand::Resume) => {
                if !*turn.borrow() {
                    return Ok(AgentCommand::Resume);
                }
                resume_wanted = true;
                events.send(AgentEvent::RestartPending).unwrap_or_default();
            }
            Some(AgentCommand::Update(request)) => {
                if *viewers.borrow() == 0 && !*turn.borrow() {
                    return Ok(AgentCommand::Update(request));
                }
                request.refuse();
            }
            Some(command) => return Ok(command),
            None => return std::future::pending().await,
        }
    }
}
/// Останавливает агента: SIGTERM его группе процессов (агент — лидер сессии своего pty), а если за `STOP_GRACE` он не
/// вышел, SIGKILL.
async fn stop(child: &mut tokio::process::Child) -> anyhow::Result<()> {
    let group = child.id().and_then(|id| i32::try_from(id).ok()).and_then(Pid::from_raw);
    if let Some(group) = group {
        kill_process_group(group, Signal::TERM).context("SIGTERM группе процессов агента не отправлен")?;
    }
    if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
        if let Some(group) = group {
            kill_process_group(group, Signal::KILL).context("SIGKILL группе процессов агента не отправлен")?;
        }
        child.wait().await.context("агент не дождался SIGKILL")?;
    }
    Ok(())
}

/// Ждёт завершения агента; без агента не завершается никогда.
pub(super) async fn agent_exit(agent: Option<&mut tokio::process::Child>) -> anyhow::Result<()> {
    let Some(agent) = agent else {
        return std::future::pending().await;
    };
    let status = agent.wait().await.context("не удалось дождаться агента")?;
    if status.success() {
        tracing::info!("агент завершился");
        Ok(())
    } else {
        anyhow::bail!("агент завершился с ошибкой: {status}")
    }
}

#[cfg(test)]
#[path = "../../tests/session/launch.rs"]
mod tests;
