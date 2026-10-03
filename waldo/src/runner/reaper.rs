//! Процессы runner завершаются вместе с ним.
//!
//! runner — subreaper (`PR_SET_CHILD_SUBREAPER`): потомок, чей родитель умер, — фоновый процесс закончившейся
//! команды, процесс после `setsid` или двойного `fork` — переходит не к init, а к runner и остаётся его потомком.
//! [`Reaper::stop`] при остановке runner шлёт SIGTERM всем потомкам, через [`KILL_GRACE`] — SIGKILL оставшимся.
//! Конец команды потомков не трогает; конец сессии убирает их через [`stop_descendants`], пока runner работает дальше.
//! Потерянную сессию — когда она не вернулась к runner — [`stop_commands`] избавляет только от процессов команд: каждый
//! запущен с переменной [`COMMAND_VAR`], её наследуют и ушедшие в свою группу или сессию потомки, а остальные процессы
//! runner (проектные MCP-серверы, служебные вызовы) метки не имеют и остаются. Процесс, который сбросил переменную
//! (`env -i`, `unset`), остаётся жить, пока его не достанет группа команды, конец сессии или остановка runner.
//!
//! Клон — отдельный runner, а не процесс команды: его и его потомков остановка не трогает. Клон отличается по pid:
//! запустивший его отмечает процесс через [`clone_process`]. Когда runner убит и клоны остались без родителя,
//! их находит [`mark_orphan_clones`] по виду процесса: лидер своей группы с `--temporary` в аргументах.
//!
//! Усыновлённый потомок, выходя, становится зомби runner, и забрать его некому, кроме runner: иначе зомби копились бы,
//! пока runner жив. Своих детей — bash команд, git, клонов — runner ждёт сам через tokio, и забрать их статус
//! первым значило бы сломать это ожидание. Поэтому зомби забирается, только если он не отмечен как свой
//! ([`own_child`], [`clone_process`]) и остаётся зомби дольше [`ZOMBIE_GRACE`]: свой ребёнок, которого runner ждёт,
//! забирается сразу после выхода, а неотмеченный (git) за это время успевает.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Context;
use rustix::io::Errno;
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, child_subreaper, getpid, kill_process, pidfd_open, pidfd_send_signal, set_child_subreaper,
    waitpid,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

/// Переменная окружения, которой помечены процессы команд, и её значение.
pub(super) const COMMAND_VAR: &str = "IAPETUS_COMMAND";
const COMMAND_MARK: &[u8] = b"IAPETUS_COMMAND=1";

/// Сколько ждать после SIGTERM потомкам, прежде чем убить оставшихся SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Сколько ещё пытаться убить потомков SIGKILL: процесс в непрерываемом ожидании умирает не сразу, а процесс
/// другого пользователя (setuid) убить нельзя вовсе — остановка не должна висеть из-за него.
const KILL_LIMIT: Duration = Duration::from_secs(2);

/// Как часто пересматривать потомков при остановке.
const POLL: Duration = Duration::from_millis(20);

/// Сколько процесс должен пробыть зомби, прежде чем runner заберёт его сам.
const ZOMBIE_GRACE: Duration = Duration::from_secs(1);

/// Процессы, которые runner запустил сам.
#[derive(Debug, Default)]
struct Marked {
    /// Свои дети, которых runner ждёт сам: их зомби не забирается.
    own: BTreeSet<i32>,
    /// Клоны: их и их потомков остановка не трогает.
    clones: BTreeSet<i32>,
}

/// Отметки на весь процесс: subreaper и дети — свойства процесса, а не объекта.
static MARKED: Mutex<Marked> = Mutex::new(Marked {
    own: BTreeSet::new(),
    clones: BTreeSet::new(),
});

fn marked() -> MutexGuard<'static, Marked> {
    MARKED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Отметка процесса; снимается, когда отметка уничтожена, — после того как процесс дождались: иначе его pid,
/// занятый потом чужим процессом, остался бы отмеченным.
#[derive(Debug)]
#[must_use = "отметка снимается, когда значение уничтожено"]
pub(super) struct Mark {
    pid: i32,
    clone: bool,
}

impl Drop for Mark {
    fn drop(&mut self) {
        let mut marked = marked();
        let set = if self.clone { &mut marked.clones } else { &mut marked.own };
        set.remove(&self.pid);
    }
}

/// Отмечает своего ребёнка, которого runner ждёт сам: его зомби не забирается.
pub(super) fn own_child(pid: u32) -> Mark {
    let pid = i32::try_from(pid).unwrap_or_default();
    marked().own.insert(pid);
    Mark { pid, clone: false }
}

/// Отмечает процесс клона: остановка runner не трогает ни его, ни его потомков, а его зомби runner не забирает —
/// клон ждёт тот, кто его запустил.
pub(super) fn clone_process(pid: u32) -> Mark {
    let pid = i32::try_from(pid).unwrap_or_default();
    marked().clones.insert(pid);
    Mark { pid, clone: true }
}

/// Процесс по `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    zombie: bool,
    /// Время запуска в тиках с загрузки: вместе с pid отличает процесс от нового с тем же pid.
    start: u64,
}

fn read_process(pid: i32) -> Option<Process> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Имя процесса в скобках может содержать пробелы и скобки: поля идут после последней `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(Process {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        zombie: *fields.first()? == "Z",
        start: fields.get(19)?.parse().ok()?,
    })
}

/// Все процессы, которые видны в `/proc`.
fn processes() -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter_map(read_process)
        .collect()
}

/// Отмечает клонами живых детей процесса, которых запустил умерший runner: каждый — лидер своей группы процессов
/// (так клоны запускаются) с `--temporary` в аргументах. Остановка их не трогает.
pub(super) fn mark_orphan_clones() -> Vec<Mark> {
    let me = getpid().as_raw_nonzero().get();
    processes()
        .into_iter()
        .filter(|process| process.parent == me && !process.zombie && process.group == process.pid)
        .filter(|process| has_temporary_flag(process.pid))
        .filter_map(|process| u32::try_from(process.pid).ok())
        .map(clone_process)
        .collect()
}

fn has_temporary_flag(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| cmdline.split(|&byte| byte == 0).any(|arg| arg == b"--temporary"))
}

/// Потомки runner, кроме клонов и их потомков; зомби — тоже: через них идёт дерево.
fn descendants() -> Vec<Process> {
    let me = getpid().as_raw_nonzero().get();
    let clones = marked().clones.clone();
    let mut children: BTreeMap<i32, Vec<Process>> = BTreeMap::new();
    for process in processes() {
        children.entry(process.parent).or_default().push(process);
    }
    let mut found = Vec::new();
    let mut queue = vec![me];
    while let Some(parent) = queue.pop() {
        for child in children.remove(&parent).unwrap_or_default() {
            if !clones.contains(&child.pid) {
                queue.push(child.pid);
                found.push(child);
            }
        }
    }
    found
}

/// Шлёт сигнал именно этому процессу: через pidfd с проверкой времени запуска, чтобы не задеть новый процесс,
/// получивший тот же pid после смерти прежнего.
fn send(process: &Process, signal: Signal) {
    let Some(pid) = Pid::from_raw(process.pid) else { return };
    match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) => {
            if read_process(process.pid).is_some_and(|now| now.start == process.start) {
                pidfd_send_signal(&pidfd, signal).unwrap_or_default();
            }
        }
        // Ядро без pidfd: остаётся сигнал по pid.
        Err(Errno::NOSYS) => kill_process(pid, signal).unwrap_or_default(),
        Err(_) => {}
    }
}

/// Процесс несёт метку команды в окружении; чужое окружение (другой пользователь) прочитать нельзя — метки нет.
fn is_command(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| environ.split(|&byte| byte == 0).any(|var| var == COMMAND_MARK))
}

/// Живые потомки, которые остановка должна завершить; `only_commands` — только помеченные как команды.
fn alive(only_commands: bool) -> Vec<Process> {
    descendants()
        .into_iter()
        .filter(|process| !process.zombie && (!only_commands || is_command(process.pid)))
        .collect()
}

/// Ждёт, пока живых потомков не останется, не дольше `limit`; каждого нового живого — `signal`, а отправленные
/// повторяет `repeat`. Пустой обход — ещё не конец: процесс, которого переподчиняют runner посреди обхода, может в
/// нём не попасться, поэтому конец — два пустых обхода подряд.
async fn drive(signal: Signal, repeat: bool, limit: Duration, only_commands: bool) -> bool {
    let deadline = Instant::now() + limit;
    let mut sent: HashSet<(i32, u64)> = HashSet::new();
    let mut empty = 0;
    loop {
        let alive = alive(only_commands);
        if alive.is_empty() {
            empty += 1;
            if empty == 2 {
                return true;
            }
        } else {
            empty = 0;
        }
        for process in &alive {
            if sent.insert((process.pid, process.start)) || repeat {
                send(process, signal);
                if signal == Signal::TERM {
                    // Остановленный процесс получит SIGTERM, только когда продолжит работу.
                    send(process, Signal::CONT);
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(POLL).await;
    }
}

/// Subreaper runner: держит потомков при себе, забирает их зомби и завершает их при остановке.
#[derive(Debug)]
pub(super) struct Reaper {
    sweeper: JoinHandle<()>,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        self.sweeper.abort();
    }
}

impl Reaper {
    /// Делает процесс subreaper и начинает забирать зомби усыновлённых потомков.
    pub(super) fn start() -> anyhow::Result<Self> {
        set_child_subreaper(Some(getpid()))
            .context("runner не стал subreaper: потомки команд ушли бы из-под него и пережили его остановку")?;
        let mut exited = signal(SignalKind::child()).context("SIGCHLD не перехвачен")?;
        let sweeper = tokio::spawn(async move {
            // Зомби → когда runner впервые его увидел.
            let mut zombies: BTreeMap<(i32, u64), Instant> = BTreeMap::new();
            loop {
                let next = zombies.values().min().map(|seen| *seen + ZOMBIE_GRACE);
                tokio::select! {
                    received = exited.recv() => if received.is_none() { return },
                    () = sleep_until_or_pending(next) => {}
                }
                sweep(&mut zombies);
            }
        });
        Ok(Self { sweeper })
    }

    /// То же, что [`stop_descendants`], при остановке самого runner.
    pub(super) async fn stop(&self) {
        stop_descendants().await;
    }
}

/// SIGTERM всем потомкам, кроме клонов и их потомков; через [`KILL_GRACE`] — SIGKILL оставшимся. Возвращается,
/// когда живых потомков не осталось или их не удалось убить за [`KILL_LIMIT`]. Процесс, не ставший subreaper, не
/// трогает ничего: его потомки — не только процессы команд, а детей ушедших из группы он не усыновляет.
pub(super) async fn stop_descendants() {
    stop(false).await;
}

/// То же, но только процессам команд (с [`COMMAND_VAR`]): остальные потомки runner продолжают работу.
pub(super) async fn stop_commands() {
    stop(true).await;
}

async fn stop(only_commands: bool) {
    if !matches!(child_subreaper(), Ok(Some(_))) {
        return;
    }
    if drive(Signal::TERM, false, KILL_GRACE, only_commands).await {
        return;
    }
    if !drive(Signal::KILL, true, KILL_LIMIT, only_commands).await {
        let left: Vec<i32> = alive(only_commands).iter().map(|process| process.pid).collect();
        tracing::warn!(?left, "потомки runner пережили SIGKILL");
    }
}

async fn sleep_until_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Забирает зомби-детей runner, которые не отмечены как свои и пробыли зомби дольше [`ZOMBIE_GRACE`].
fn sweep(zombies: &mut BTreeMap<(i32, u64), Instant>) {
    let me = getpid().as_raw_nonzero().get();
    let now = Instant::now();
    let current: BTreeSet<(i32, u64)> = {
        let marked = marked();
        processes()
            .into_iter()
            .filter(|process| process.parent == me && process.zombie)
            .filter(|process| !marked.own.contains(&process.pid) && !marked.clones.contains(&process.pid))
            .map(|process| (process.pid, process.start))
            .collect()
    };
    zombies.retain(|key, _| current.contains(key));
    for key in current {
        let seen = *zombies.entry(key).or_insert(now);
        if now.duration_since(seen) >= ZOMBIE_GRACE
            && let Some(pid) = Pid::from_raw(key.0)
        {
            // Зомби мог уже забрать tokio: тогда `ECHILD`, и забирать нечего.
            waitpid(Some(pid), WaitOptions::NOHANG).unwrap_or_default();
            zombies.remove(&key);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/runner/reaper.rs"]
mod tests;
