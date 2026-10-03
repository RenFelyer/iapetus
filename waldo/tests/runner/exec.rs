//! Выполнение команд настоящим bash во временном рабочем каталоге.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use nerve::protocol::Stream;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use super::*;

/// Сколько настоящего времени ждать события, прежде чем счесть тест зависшим.
const HANG_LIMIT: Duration = Duration::from_secs(20);
const ASSIGNMENT: Assignment = Assignment(1);

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG_LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("завис дольше {HANG_LIMIT:?}: {what}"))
}

struct Fixture {
    work: TempDir,
    /// `.env` лежит вне рабочего каталога: файл в нём попал бы в наблюдение и в кэш сессии.
    config: TempDir,
    executor: Arc<Executor>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_system(&[])
    }

    fn with_system(system: &[(&str, &str)]) -> Self {
        let work = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let workdir = Arc::new(Workdir::open(&fs::canonicalize(work.path()).unwrap()).unwrap());
        let system = system
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect();
        let executor = Arc::new(Executor::new(workdir, config.path().join(".env"), system, None));
        executor.start(ASSIGNMENT, BTreeMap::new());
        Self { work, config, executor }
    }

    fn path(&self) -> PathBuf {
        fs::canonicalize(self.work.path()).unwrap()
    }

    fn write_env(&self, text: &str) {
        fs::write(self.config.path().join(".env"), text).unwrap();
    }

    fn request(id: u64, command: &str, cwd: &str) -> ExecRequest {
        ExecRequest {
            id,
            command: command.to_owned(),
            cwd: cwd.to_owned(),
            timeout_ms: None,
        }
    }

    /// Выполняет команду в каталоге `cwd` и собирает все события до последнего.
    async fn run_in(&self, command: &str, cwd: &str) -> Vec<ExecEvent> {
        self.run_request(Self::request(1, command, cwd)).await
    }

    async fn run(&self, command: &str) -> Vec<ExecEvent> {
        self.run_in(command, &self.path().display().to_string()).await
    }

    async fn run_request(&self, request: ExecRequest) -> Vec<ExecEvent> {
        let (events, mut received) = mpsc::channel(1024);
        let executor = Arc::clone(&self.executor);
        let task = tokio::spawn(async move { executor.run(ASSIGNMENT, request, events).await });
        let mut all = Vec::new();
        within("поток событий команды", async {
            while let Some(event) = received.recv().await {
                all.push(event);
            }
        })
        .await;
        task.await.unwrap();
        all
    }

    /// Запускает команду в фоне; события читает тест.
    fn start(&self, id: u64, command: &str) -> Running {
        let (events, received) = mpsc::channel(1024);
        let executor = Arc::clone(&self.executor);
        let request = Self::request(id, command, &self.path().display().to_string());
        let task = tokio::spawn(async move { executor.run(ASSIGNMENT, request, events).await });
        Running { received, task }
    }

    fn session(&self, github_token: Option<&str>) {
        let author = Author {
            name: "Ren Felyer".to_owned(),
            email: "renfelyer@gmail.com".to_owned(),
        };
        self.executor
            .start(ASSIGNMENT, session_vars(author, github_token.map(str::to_owned)));
    }
}

struct Running {
    received: mpsc::Receiver<ExecEvent>,
    task: JoinHandle<()>,
}

impl Running {
    async fn finish(mut self) -> Vec<ExecEvent> {
        let mut all = Vec::new();
        within("завершение команды", async {
            while let Some(event) = self.received.recv().await {
                all.push(event);
            }
        })
        .await;
        self.task.await.unwrap();
        all
    }
}

fn text(events: &[ExecEvent], wanted: Stream) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            ExecEvent::Output { stream, data } if *stream == wanted => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8(bytes).unwrap()
}

fn stdout(events: &[ExecEvent]) -> String {
    text(events, Stream::Stdout)
}

fn stderr(events: &[ExecEvent]) -> String {
    text(events, Stream::Stderr)
}

#[track_caller]
fn exit_of(events: &[ExecEvent]) -> &ExecExit {
    match events.last() {
        Some(ExecEvent::Exit(exit)) => exit,
        other => panic!("последнее событие — не Exit: {other:?}; все события: {events:?}"),
    }
}

#[track_caller]
fn error_of(events: &[ExecEvent]) -> &Error {
    match events {
        [ExecEvent::Err(error)] => error,
        other => panic!("ожидалась одна ошибка, пришло {other:?}"),
    }
}

/// Процесс жив: есть в `/proc` и не зомби (осиротевший зомби ждёт init, который в контейнере может его не забрать).
fn is_alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(stat) => stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next()) != Some("Z"),
    }
}

async fn read_pid(file: &Path) -> u32 {
    within("файл с pid", async {
        loop {
            if let Ok(text) = fs::read_to_string(file)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

#[tokio::test]
async fn cwd_after_the_command_is_reported() {
    let fixture = Fixture::new();
    let events = fixture.run("mkdir -p sub && cd sub").await;
    assert_eq!(
        exit_of(&events).cwd,
        fixture.path().join("sub").display().to_string(),
        "cwd после `cd sub` не передан"
    );
}

#[tokio::test]
async fn cwd_is_reported_even_when_the_command_calls_exit() {
    let fixture = Fixture::new();
    let events = fixture.run("mkdir -p sub && cd sub; exit 3").await;
    let exit = exit_of(&events);
    assert_eq!(exit.status, ExitStatus::Code(3), "код `exit` искажён ловушкой cwd");
    assert_eq!(
        exit.cwd,
        fixture.path().join("sub").display().to_string(),
        "cwd при `exit` внутри команды потерян"
    );
}

#[tokio::test]
async fn command_exit_trap_does_not_take_the_cwd_away() {
    let fixture = Fixture::new();
    let events = fixture
        .run("trap 'echo cleanup status=$?' EXIT; mkdir -p sub && cd sub; exit 4")
        .await;
    let exit = exit_of(&events);
    assert_eq!(
        stdout(&events),
        "cleanup status=4\n",
        "ловушка команды на EXIT не сработала или получила чужой `$?`"
    );
    assert_eq!(exit.status, ExitStatus::Code(4), "код выхода искажён ловушкой");
    assert_eq!(
        exit.cwd,
        fixture.path().join("sub").display().to_string(),
        "собственная ловушка команды на EXIT отняла cwd"
    );
}

#[tokio::test]
async fn positional_parameters_of_the_command_do_not_redirect_the_cwd_file() {
    let fixture = Fixture::new();
    let events = fixture.run("set -- a victim; mkdir -p sub && cd sub; exit 3").await;
    let exit = exit_of(&events);
    assert_eq!(exit.status, ExitStatus::Code(3), "код `exit` искажён");
    assert_eq!(
        exit.cwd,
        fixture.path().join("sub").display().to_string(),
        "cwd после `set --` в команде потерян"
    );
    for victim in [fixture.path().join("victim"), fixture.path().join("sub/victim")] {
        assert!(!victim.exists(), "runner записал cwd в файл пользователя {}", victim.display());
    }
}

#[tokio::test]
async fn cwd_outside_the_workdir_by_absolute_path_is_rejected() {
    let fixture = Fixture::new();
    let events = fixture.run_in("touch ran", "/tmp").await;
    assert_eq!(error_of(&events), &Error::OutsideWorkdir, "абсолютный путь вне каталога принят");
}

#[tokio::test]
async fn cwd_outside_the_workdir_by_parent_reference_is_rejected() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.path().join("sub")).unwrap();
    for cwd in ["..", "sub/../..", &format!("{}/..", fixture.path().display())] {
        let events = fixture.run_in("touch ran", cwd).await;
        assert_eq!(error_of(&events), &Error::OutsideWorkdir, "`..` в пути {cwd} принят");
    }
}

#[tokio::test]
async fn cwd_with_parent_reference_that_stays_inside_is_rejected_too() {
    // Ядро такой путь пропустило бы: `..` не выходит из каталога. Но сессия присылает прямые пути, и `..` в них — ошибка.
    let fixture = Fixture::new();
    fs::create_dir(fixture.path().join("sub")).unwrap();
    let events = fixture.run_in("touch ran", "sub/..").await;
    assert_eq!(error_of(&events), &Error::OutsideWorkdir, "`..` внутри рабочего каталога принят");
    assert!(!fixture.path().join("ran").exists(), "команда выполнилась при отклонённом cwd");
}

#[tokio::test]
async fn cwd_through_a_symlink_leaving_the_workdir_is_rejected() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), fixture.path().join("escape")).unwrap();
    let events = fixture.run_in("touch ran", "escape").await;
    assert!(
        matches!(
            error_of(&events),
            Error::OutsideWorkdir | Error::NotFound | Error::Access | Error::Io(_)
        ),
        "ссылка наружу принята: {events:?}"
    );
    assert!(
        !outside.path().join("ran").exists(),
        "команда выполнилась за пределами рабочего каталога"
    );
}

#[tokio::test]
async fn session_author_applies_to_git_and_ends_with_the_session() {
    let fixture = Fixture::new();
    fixture.session(Some("ghp_session_token"));
    let events = fixture
        .run("echo $GIT_AUTHOR_NAME $GIT_AUTHOR_EMAIL $GIT_COMMITTER_NAME $GIT_COMMITTER_EMAIL")
        .await;
    assert_eq!(
        stdout(&events),
        "Ren Felyer renfelyer@gmail.com Ren Felyer renfelyer@gmail.com\n",
        "автор коммитов не передан командам"
    );
    fixture.executor.stop().await;
    assert!(
        fixture.run("echo ran").await.iter().any(|event| matches!(event, ExecEvent::Err(_))),
        "команда запущена после конца назначения"
    );
    fixture.executor.start(ASSIGNMENT, BTreeMap::new());
    let events = fixture.run("echo [$GIT_AUTHOR_NAME][$GIT_COMMITTER_EMAIL][$GITHUB_TOKEN]").await;
    assert_eq!(stdout(&events), "[][][]\n", "переменные сессии пережили её конец");
}

#[tokio::test]
async fn new_session_replaces_the_previous_one() {
    let fixture = Fixture::new();
    fixture.session(Some("ghp_first_token"));
    fixture.session(None);
    let events = fixture.run("echo [$GITHUB_TOKEN]").await;
    assert_eq!(stdout(&events), "[]\n", "токен прежней сессии остался после начала новой");
}

#[tokio::test]
async fn github_token_is_masked_in_both_streams() {
    let fixture = Fixture::new();
    fixture.session(Some("ghp_session_token"));
    let events = fixture.run("echo token=$GITHUB_TOKEN; echo err=$GITHUB_TOKEN >&2").await;
    assert_eq!(stdout(&events), "token=***\n", "GITHUB_TOKEN в stdout не замаскирован");
    assert_eq!(stderr(&events), "err=***\n", "GITHUB_TOKEN в stderr не замаскирован");
}

#[tokio::test]
async fn secret_printed_in_pieces_with_pauses_is_masked() {
    let fixture = Fixture::new();
    fixture.session(Some("ghp_session_token"));
    // Куски приходят разными чтениями: пауза между ними заставляет runner отправить первый до прихода второго.
    let events = fixture
        .run("printf ghp_; sleep 0.3; printf session; sleep 0.3; printf _token; echo")
        .await;
    let output = stdout(&events);
    assert_eq!(output, "***\n", "секрет, напечатанный кусками, прошёл открыто: {output:?}");
}

#[tokio::test]
async fn overridden_dotenv_secret_is_masked_in_both_streams_even_in_pieces() {
    let fixture = Fixture::new();
    fixture.write_env("GITHUB_TOKEN=dotenv-github-token\n");
    fixture.session(Some("ghp_session_token"));
    let env = fixture.config.path().join(".env");
    // Сессия перебила значение `.env`, и в окружение команды оно не попало, но файл команда прочитать может.
    // Второй вывод — кусками с паузой: значение разорвано границей кусков.
    let command = format!(
        "cat '{env}'; cat '{env}' >&2; value=$(cut -d= -f2 '{env}'); printf %s \"${{value:0:9}}\"; sleep 0.3; echo \"${{value:9}}\"; \
         printf %s \"${{value:0:5}}\" >&2; sleep 0.3; echo \"${{value:5}}\" >&2",
        env = env.display()
    );
    let events = fixture.run(&command).await;
    assert_eq!(
        stdout(&events),
        "GITHUB_TOKEN=***\n***\n",
        "перебитое значение секрета из `.env` прошло открыто в stdout"
    );
    assert_eq!(
        stderr(&events),
        "GITHUB_TOKEN=***\n***\n",
        "перебитое значение секрета из `.env` прошло открыто в stderr"
    );
}

#[tokio::test]
async fn end_of_session_stops_running_commands() {
    let fixture = Fixture::new();
    fixture.session(None);
    let running = fixture.start(3, "echo $$ > pid; sleep 300");
    let pid = read_pid(&fixture.path().join("pid")).await;
    fixture.executor.stop().await;
    running.finish().await;
    within("остановка команды по концу сессии", async {
        while is_alive(pid) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn new_session_stops_commands_of_the_previous_one() {
    let fixture = Fixture::new();
    fixture.session(None);
    let running = fixture.start(4, "echo $$ > pid; sleep 300");
    let pid = read_pid(&fixture.path().join("pid")).await;
    fixture.executor.stop().await;
    fixture.session(None);
    running.finish().await;
    within("остановка команды прежней сессии", async {
        while is_alive(pid) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn end_of_assignment_kills_background_processes_of_finished_commands() {
    let fixture = Fixture::new();
    fixture.run("sleep 300 >/dev/null 2>&1 & echo $! > bg.pid").await;
    let pid = read_pid(&fixture.path().join("bg.pid")).await;
    assert!(is_alive(pid), "фоновый процесс не пережил команду");
    fixture.executor.stop().await;
    within("убийство фонового процесса", async {
        while is_alive(pid) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}
