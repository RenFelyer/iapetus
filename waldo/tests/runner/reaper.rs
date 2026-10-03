//! Остановка runner через настоящие процессы: тест запускает свой же бинарник как отдельный процесс runner —
//! subreaper действует на весь процесс, и в процессе тестов он задел бы соседние тесты.
//!
//! Процесс runner ([`runner_process`]) делает то же, что `runner::run`: становится subreaper, выполняет команды
//! исполнителем runner, ждёт SIGTERM или SIGINT и останавливает потомков.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use nerve::protocol::{Assignment, ExecEvent, ExecRequest};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::*;

const ASSIGNMENT: Assignment = Assignment(1);
use crate::runner::exec::Executor;
use crate::runner::fs::Workdir;

/// Каталог сценария для процесса runner; без переменной [`runner_process`] ничего не делает.
const SCENARIO: &str = "WALDO_REAPER_SCENARIO";

/// Сколько настоящего времени ждать события, прежде чем счесть тест зависшим.
const HANG_LIMIT: Duration = Duration::from_secs(20);

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG_LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("завис дольше {HANG_LIMIT:?}: {what}"))
}

/// Шаг сценария процесса runner.
#[derive(Debug, Clone, Copy)]
enum Step<'a> {
    /// Команда runner, которая успевает закончиться.
    Run(&'a str),
    /// Команда runner, которая ещё выполняется, когда runner останавливают.
    Start(&'a str),
    /// Клон: отдельный процесс в своей сессии, отмеченный через [`clone_process`]; скрипт — для `sh -c`.
    Clone(&'a str),
    /// Конец назначения исполнителя; runner продолжает работать.
    End,
    /// Свой процесс runner (как MCP-сервер), не команда: runner его ждёт, скрипт — для `sh -c`.
    Spawn(&'a str),
}

impl Step<'_> {
    fn line(self) -> String {
        match self {
            Step::Run(command) => format!("run {command}"),
            Step::Start(command) => format!("start {command}"),
            Step::Clone(script) => format!("clone {script}"),
            Step::End => "end".to_owned(),
            Step::Spawn(script) => format!("spawn {script}"),
        }
    }
}

/// Процесс runner для тестов остановки: запускают его сами тесты с каталогом сценария в [`SCENARIO`].
#[tokio::test]
#[ignore = "процесс runner, который запускают тесты остановки"]
async fn runner_process() {
    let Some(dir) = std::env::var_os(SCENARIO).map(PathBuf::from) else {
        return;
    };
    // Обработчики ставятся до `ready`: SIGTERM, пришедший раньше, убил бы процесс по умолчанию.
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    // SIGUSR1: сессия не вернулась, runner продолжает работать.
    let mut sigusr1 = signal(SignalKind::user_defined1()).unwrap();
    let reaper = Reaper::start().unwrap();
    let work = dir.join("work");
    let executor = Arc::new(Executor::new(
        Arc::new(Workdir::open(&work).unwrap()),
        dir.join(".env"),
        std::env::vars_os().collect(),
        None,
    ));
    executor.start(ASSIGNMENT, BTreeMap::new());
    let mut running = Vec::new();
    let mut clones = Vec::new();
    let mut spawned = Vec::new();
    for (id, line) in (1..).zip(fs::read_to_string(dir.join("scenario")).unwrap().lines()) {
        if line == "end" {
            executor.stop().await;
            continue;
        }
        let (step, text) = line.split_once(' ').unwrap();
        if step == "spawn" {
            let child = Command::new("sh").args(["-c", text]).current_dir(&work).spawn().unwrap();
            let mark = own_child(child.id().unwrap());
            spawned.push((child, mark));
            continue;
        }
        if step == "clone" {
            let clone = Command::new("setsid").args(["sh", "-c", text]).current_dir(&work).spawn().unwrap();
            let mark = clone_process(clone.id().unwrap());
            clones.push((clone, mark));
            continue;
        }
        let request = ExecRequest {
            id,
            command: text.to_owned(),
            cwd: work.display().to_string(),
            timeout_ms: None,
        };
        let (events, mut received) = mpsc::channel(1024);
        let executor = Arc::clone(&executor);
        let task = tokio::spawn(async move { executor.run(ASSIGNMENT, request, events).await });
        if step == "run" {
            let mut last = None;
            while let Some(event) = received.recv().await {
                last = Some(event);
            }
            task.await.unwrap();
            // Статус bash забирает исполнитель: забери его runner вместе с усыновлёнными, ответом была бы ошибка.
            assert!(
                matches!(last, Some(ExecEvent::Exit(_))),
                "команда {text:?} закончилась без Exit: {last:?}"
            );
        } else {
            running.push((task, received));
        }
    }
    fs::write(dir.join("ready"), "").unwrap();
    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sigusr1.recv() => executor.abandon().await,
        }
    }
    reaper.stop().await;
    for (task, mut received) in running {
        let mut last = None;
        while let Some(event) = received.recv().await {
            last = Some(event);
        }
        task.await.unwrap();
        assert!(
            matches!(last, Some(ExecEvent::Exit(_))),
            "выполнявшаяся команда не завершилась при остановке: {last:?}"
        );
    }
}

/// Запущенный тестом процесс runner.
struct RunnerProcess {
    dir: TempDir,
    child: Child,
}

impl RunnerProcess {
    async fn start(steps: &[Step<'_>]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        let scenario: Vec<String> = steps.iter().map(|step| step.line()).collect();
        fs::write(dir.path().join("scenario"), scenario.join("\n")).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runner::reaper::tests::runner_process", "--ignored", "--nocapture"])
            .env(SCENARIO, dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(dir.path().join("stderr")).unwrap())
            .spawn()
            .unwrap();
        let mut runner = Self { dir, child };
        let ready = runner.dir.path().join("ready");
        within("готовность процесса runner", async {
            while !ready.exists() {
                if let Some(status) = runner.child.try_wait().unwrap() {
                    panic!("процесс runner вышел до готовности ({status}): {}", runner.stderr());
                }
                sleep(POLL).await;
            }
        })
        .await;
        runner
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.dir.path().join("stderr")).unwrap_or_default()
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(i32::try_from(self.child.id().unwrap()).unwrap()).unwrap()
    }

    fn signal(&self, signal: Signal) {
        kill_process(self.pid(), signal).unwrap();
    }

    /// Ждёт выхода runner.
    async fn wait(mut self) {
        let status = within("выход процесса runner", self.child.wait()).await.unwrap();
        assert!(status.success(), "процесс runner упал при остановке: {}", self.stderr());
    }

    /// Останавливает runner сигналом и ждёт его выхода.
    async fn stop(self, signal: Signal) {
        self.signal(signal);
        self.wait().await;
    }
}

/// Процессы из файлов с pid; уничтожение убивает выживших, чтобы упавший тест не оставил их жить.
struct Pids(Vec<(&'static str, Process)>);

impl Pids {
    /// Ждёт, пока все файлы с pid в `dir` будут записаны, и запоминает процессы.
    async fn wait(dir: &Path, files: &[&'static str]) -> Self {
        within("файлы с pid", async {
            loop {
                let pids: Option<Vec<_>> = files
                    .iter()
                    .map(|&file| {
                        let text = fs::read_to_string(dir.join(file)).ok()?;
                        let pid = text.strip_suffix('\n')?.parse().ok()?;
                        Some((file, read_process(pid)?))
                    })
                    .collect();
                if let Some(pids) = pids {
                    return Self(pids);
                }
                sleep(POLL).await;
            }
        })
        .await
    }

    /// Процессы, которые ещё живы: тот же pid с тем же временем запуска и не зомби.
    fn alive(&self) -> Vec<&'static str> {
        self.0
            .iter()
            .filter(|(_, process)| read_process(process.pid).is_some_and(|now| now.start == process.start && !now.zombie))
            .map(|(file, _)| *file)
            .collect()
    }

    #[track_caller]
    fn assert_all_alive(&self, what: &str) {
        let alive = self.alive();
        let dead: Vec<&str> = self.0.iter().map(|(file, _)| *file).filter(|file| !alive.contains(file)).collect();
        assert!(dead.is_empty(), "{what}: мертвы {dead:?}");
    }

    #[track_caller]
    fn assert_all_dead(&self, what: &str) {
        let alive = self.alive();
        assert!(alive.is_empty(), "{what}: живы {alive:?}");
    }
}

impl Drop for Pids {
    fn drop(&mut self) {
        for (_, process) in &self.0 {
            send(process, Signal::KILL);
        }
    }
}

#[tokio::test]
async fn processes_left_by_commands_die_with_the_runner() {
    let runner = RunnerProcess::start(&[
        // Фоновый процесс закончившейся команды.
        Step::Run("sleep 300 & echo $! > background.pid"),
        // Своя сессия: группой процессов команды его не достать.
        Step::Run("setsid -f sh -c 'echo $$ > setsid.pid; exec sleep 301'"),
        // Двойной fork: промежуточный процесс вышел, внук осиротел.
        Step::Run("(sh -c 'echo $$ > double-fork.pid; exec sleep 302' &)"),
        // Команда ещё выполняется, и у неё есть свой ребёнок — внук runner.
        Step::Start("sleep 303 & echo $! > running-child.pid; echo $$ > running.pid; wait"),
    ])
    .await;
    let pids = Pids::wait(
        &runner.work(),
        &[
            "background.pid",
            "setsid.pid",
            "double-fork.pid",
            "running-child.pid",
            "running.pid",
        ],
    )
    .await;
    pids.assert_all_alive("конец команды убил процессы, которые она оставила");
    runner.stop(Signal::TERM).await;
    // Все откликаются на SIGTERM: SIGKILL через `KILL_GRACE` им не нужен, и выход runner их уже не застаёт.
    pids.assert_all_dead("потомки команд пережили остановку runner");
}

#[tokio::test]
async fn descendant_ignoring_sigterm_is_killed_after_the_grace_and_its_child_is_not_kept_waiting() {
    // Игнорирование SIGTERM наследуется через exec и fork, поэтому ребёнок запущен до `trap`. Он остаётся ребёнком
    // упрямого процесса — внуком runner, до которого не дойти по одним детям runner.
    let runner = RunnerProcess::start(&[Step::Run(
        r#"setsid -f sh -c 'sleep 308 & echo $! > child.pid; trap "" TERM; echo $$ > stubborn.pid; exec sleep 309'"#,
    )])
    .await;
    let child = Pids::wait(&runner.work(), &["child.pid"]).await;
    let stubborn = Pids::wait(&runner.work(), &["stubborn.pid"]).await;
    let stopping = Instant::now();
    runner.signal(Signal::TERM);
    within("SIGTERM внуку", async {
        while !child.alive().is_empty() {
            sleep(POLL).await;
        }
    })
    .await;
    stubborn.assert_all_alive("упрямый процесс умер раньше, чем истёк срок SIGTERM, и тест не проверяет обход потомков");
    runner.wait().await;
    assert!(
        stopping.elapsed() >= KILL_GRACE,
        "runner вышел раньше, чем истёк срок SIGTERM: упрямый процесс убит не по эскалации"
    );
    stubborn.assert_all_dead("процесс, игнорирующий SIGTERM, пережил остановку runner");
}

#[tokio::test]
async fn clone_and_its_descendants_survive_the_runner_stop() {
    let runner = RunnerProcess::start(&[
        Step::Clone("sleep 306 & echo $! > clone-child.pid; echo $$ > clone.pid; wait"),
        Step::Run("setsid -f sh -c 'echo $$ > command.pid; exec sleep 307'"),
    ])
    .await;
    let clone = Pids::wait(&runner.work(), &["clone.pid", "clone-child.pid"]).await;
    let command = Pids::wait(&runner.work(), &["command.pid"]).await;
    runner.stop(Signal::TERM).await;
    command.assert_all_dead("потомок команды пережил остановку runner");
    clone.assert_all_alive("остановка runner задела клон или его потомков");
}

#[tokio::test]
async fn session_end_kills_processes_that_left_the_group_but_not_clone_descendants() {
    let runner = RunnerProcess::start(&[
        Step::Clone("sleep 310 & echo $! > clone-child.pid; echo $$ > clone.pid; wait"),
        Step::Run("setsid -f sh -c 'echo $$ > setsid.pid; exec sleep 311'"),
        Step::Run("(sh -c 'echo $$ > double-fork.pid; exec sleep 312' &)"),
        Step::End,
    ])
    .await;
    // Сценарий дошёл до готовности только после конца назначения, а runner ещё жив.
    let left = Pids::wait(&runner.work(), &["setsid.pid", "double-fork.pid"]).await;
    let clone = Pids::wait(&runner.work(), &["clone.pid", "clone-child.pid"]).await;
    left.assert_all_dead("конец сессии оставил жить процессы, ушедшие из группы команды");
    clone.assert_all_alive("конец сессии задел клон или его потомков");
    runner.stop(Signal::TERM).await;
}

#[tokio::test]
async fn abandoned_session_loses_command_processes_but_not_runner_own_ones() {
    let runner = RunnerProcess::start(&[
        Step::Spawn("echo $$ > service.pid; exec sleep 320"),
        Step::Clone("sleep 321 & echo $! > clone-child.pid; echo $$ > clone.pid; wait"),
        Step::Run("sleep 322 & echo $! > background.pid"),
        Step::Run("setsid -f sh -c 'echo $$ > setsid.pid; exec sleep 323'"),
        Step::Run("(sh -c 'echo $$ > double-fork.pid; exec sleep 324' &)"),
        Step::Start("sleep 325 & echo $! > running-child.pid; echo $$ > running.pid; wait"),
        Step::Run("until [ -s setsid.pid ] && [ -s double-fork.pid ] && [ -s running.pid ]; do sleep 0.05; done"),
    ])
    .await;
    let commands = Pids::wait(
        &runner.work(),
        &[
            "background.pid",
            "setsid.pid",
            "double-fork.pid",
            "running-child.pid",
            "running.pid",
        ],
    )
    .await;
    let others = Pids::wait(&runner.work(), &["service.pid", "clone.pid", "clone-child.pid"]).await;
    commands.assert_all_alive("команды сессии убиты до того, как она потерялась");
    runner.signal(Signal::USR1);
    within("завершение процессов команд", async {
        while !commands.alive().is_empty() {
            sleep(POLL).await;
        }
    })
    .await;
    commands.assert_all_dead("потерянная сессия оставила жить процессы своих команд");
    others.assert_all_alive("потерянная сессия задела процесс runner или клон");
    runner.stop(Signal::TERM).await;
}
