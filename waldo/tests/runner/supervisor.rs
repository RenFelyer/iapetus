//! Супервизор через настоящие процессы: тест запускает свой бинарник как супервизор, а тот — свой же бинарник как
//! «runner». subreaper и PDEATHSIG действуют на весь процесс, в процессе тестов они задели бы соседние тесты.
//!
//! «Runner» ([`runner_process`]) делает то же, что настоящий: ставит защиту от смерти супервизора, становится
//! subreaper, запускает фоновый процесс команды и клон, ждёт SIGTERM и останавливает потомков.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Child;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::sleep;

use super::*;
use crate::runner::reaper::clone_process;

/// Каталог сценария; без переменной процессы [`supervisor_process`] и [`runner_process`] ничего не делают.
const SCENARIO: &str = "WALDO_SUPERVISOR_SCENARIO";

/// pid супервизора для защиты «runner» от его смерти.
const SUPERVISOR_PID: &str = "WALDO_SUPERVISOR_PID";

/// Сколько настоящего времени ждать события, прежде чем счесть тест зависшим.
const HANG_LIMIT: Duration = Duration::from_secs(20);

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG_LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("завис дольше {HANG_LIMIT:?}: {what}"))
}

fn test_process(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", name, "--ignored", "--nocapture"]);
    command
}

/// Процесс супервизора для тестов: запускают его сами тесты.
#[tokio::test]
#[ignore = "процесс супервизора, который запускают тесты супервизора"]
async fn supervisor_process() {
    if std::env::var_os(SCENARIO).is_none() {
        return;
    }
    let mut runner = test_process("runner::supervisor::tests::runner_process");
    runner.env(SUPERVISOR_PID, std::process::id().to_string());
    // Исход виден тесту по процессам и коду выхода, а не по значению.
    supervise(runner).await.unwrap_or_default();
}

/// Процесс «runner» под супервизором.
#[tokio::test]
#[ignore = "процесс runner, который запускает супервизор в тестах"]
async fn runner_process() {
    let (Some(dir), Some(supervisor)) = (std::env::var_os(SCENARIO), std::env::var(SUPERVISOR_PID).ok()) else {
        return;
    };
    let dir = PathBuf::from(dir);
    // Обработчики ставятся до `ready`: SIGTERM, пришедший раньше, убил бы процесс по умолчанию.
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    guard(supervisor.parse().unwrap()).unwrap();
    let reaper = Reaper::start().unwrap();
    fs::write(dir.join("runner.pid"), std::process::id().to_string()).unwrap();
    // Команда оставила фоновый процесс: он усыновлён runner.
    let background = Command::new("sh")
        .args(["-c", "sleep 300 & echo $! > background.pid"])
        .current_dir(&dir)
        .status();
    assert!(background.await.unwrap().success(), "команда с фоновым процессом не отработала");
    // Клон: лидер своей группы с `--temporary`.
    let clone = Command::new("sh")
        .args(["-c", "echo $$ > clone.pid; sleep 301; :", "--temporary"])
        .current_dir(&dir)
        .process_group(0)
        .spawn()
        .unwrap();
    let _mark = clone_process(clone.id().unwrap());
    fs::write(dir.join("ready"), "").unwrap();
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    reaper.stop().await;
}

fn pid_from(dir: &Path, file: &str) -> Option<i32> {
    fs::read_to_string(dir.join(file)).ok()?.trim().parse().ok()
}

fn alive(pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')').is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
}

fn kill(pid: i32) {
    kill_process(Pid::from_raw(pid).unwrap(), Signal::KILL).unwrap_or_default();
}

/// Супервизор с «runner» под ним; уничтожение убивает всё, что пережило тест.
struct Supervised {
    dir: TempDir,
    supervisor: Child,
}

impl Supervised {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = test_process("runner::supervisor::tests::supervisor_process")
            .env(SCENARIO, dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Self { dir, supervisor };
        within("готовность runner", async {
            while !started.dir.path().join("ready").exists() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        started
    }

    fn pid(&self, file: &str) -> i32 {
        pid_from(self.dir.path(), file).unwrap_or_else(|| panic!("нет pid в {file}"))
    }

    async fn wait_dead(&self, what: &str, pid: i32) {
        within(what, async {
            while alive(pid) {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        for file in ["clone.pid", "background.pid", "runner.pid"] {
            if let Some(pid) = pid_from(self.dir.path(), file) {
                kill(pid);
            }
        }
        self.supervisor.start_kill().unwrap_or_default();
    }
}

#[tokio::test]
async fn killed_runner_leaves_no_command_processes_but_its_clone_survives() {
    let mut test = Supervised::start().await;
    let (runner, background, clone) = (test.pid("runner.pid"), test.pid("background.pid"), test.pid("clone.pid"));
    assert!(alive(background), "фоновый процесс команды не запущен");
    kill(runner);
    test.wait_dead("конец фонового процесса команды", background).await;
    within("выход супервизора", test.supervisor.wait()).await.unwrap();
    assert!(alive(clone), "клон завершён вместе с командами убитого runner");
}

#[tokio::test]
async fn killed_supervisor_stops_the_runner_gracefully() {
    let test = Supervised::start().await;
    let (runner, background) = (test.pid("runner.pid"), test.pid("background.pid"));
    kill(i32::try_from(test.supervisor.id().unwrap()).unwrap());
    test.wait_dead("остановка runner", runner).await;
    // Штатная остановка runner завершает и его потомков.
    test.wait_dead("конец фонового процесса команды", background).await;
}
