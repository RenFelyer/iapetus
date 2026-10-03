//! Запросы server к runner по каналу управления: начало и конец сессии, чужой запрос.

use std::fs;
use std::time::Duration;

use nerve::mux::Mux;
use nerve::protocol::{self, Assignment, Author};
use tokio::io::duplex;

use super::*;
use crate::runner::fs::Workdir;

fn start(assignment: u64, github_token: Option<&str>) -> ToRunner {
    ToRunner::Session(SessionMessage::Start {
        assignment: Assignment(assignment),
        author: Author {
            name: "Ada".to_owned(),
            email: "ada@example.com".to_owned(),
        },
        github_token: github_token.map(str::to_owned),
    })
}

fn end(assignment: u64) -> ToRunner {
    ToRunner::Session(SessionMessage::End {
        assignment: Assignment(assignment),
    })
}

/// Runner обслуживает запросы канала; `server` — сторона server.
struct Harness {
    server: Mux,
    data: Arc<Data>,
    executor: Arc<Executor>,
    dir: tempfile::TempDir,
    _server_requests: Requests,
}

fn serve_requests() -> Harness {
    let (a, b) = duplex(1 << 16);
    let (server, server_requests) = Mux::new(a);
    let (_runner, mut runner_requests) = Mux::new(b);
    let dir = tempfile::tempdir().unwrap();
    let workdir = Arc::new(Workdir::open(&fs::canonicalize(dir.path()).unwrap()).unwrap());
    let executor = Arc::new(Executor::new(workdir, dir.path().join("no-such.env"), Vec::new(), None));
    let data = Arc::new(Data::new(None));
    let (for_executor, for_data) = (Arc::clone(&executor), Arc::clone(&data));
    tokio::spawn(async move {
        answer_requests(&mut runner_requests, &for_executor, &for_data, None, None).await;
    });
    Harness {
        server,
        data,
        executor,
        dir,
        _server_requests: server_requests,
    }
}

impl Harness {
    /// Выполняет команду в рабочем каталоге от имени назначения `assignment` и возвращает её вывод.
    async fn run(&self, assignment: u64, command: &str) -> String {
        let (events, mut received) = tokio::sync::mpsc::channel(64);
        let request = protocol::ExecRequest {
            id: 1,
            command: command.to_owned(),
            cwd: fs::canonicalize(self.dir.path()).unwrap().display().to_string(),
            timeout_ms: None,
        };
        self.executor.run(Assignment(assignment), request, events).await;
        let mut stdout = String::new();
        while let Some(event) = received.recv().await {
            if let protocol::ExecEvent::Output {
                stream: protocol::Stream::Stdout,
                data,
            } = event
            {
                stdout.push_str(&String::from_utf8_lossy(&data));
            }
        }
        stdout
    }

    async fn wait_dead(&self, file: &str) {
        let pid = fs::read_to_string(self.dir.path().join(file))
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            // Зомби не жив: статус забирает не тест.
            while fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next()) != Some("Z"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("процесс {pid} не убит"));
    }
}

async fn ask(server: &Mux, request: &ToRunner) -> Result<(), Error> {
    let body = server.request(protocol::encode(request).unwrap().into()).await.unwrap();
    protocol::decode_body(&body).unwrap()
}

#[tokio::test]
async fn session_start_and_end_from_server_open_and_close_the_session() {
    let harness = serve_requests();
    assert_eq!(harness.data.assignment(), None);

    ask(&harness.server, &start(1, None)).await.unwrap();
    assert_eq!(harness.data.assignment(), Some(Assignment(1)), "после Start сессия не началась");

    ask(&harness.server, &end(1)).await.unwrap();
    assert_eq!(harness.data.assignment(), None, "после End сессия не закончилась");
}

#[tokio::test]
async fn start_of_the_running_assignment_changes_nothing_and_end_of_another_is_ignored() {
    let harness = serve_requests();
    ask(&harness.server, &start(1, Some("ghp_first"))).await.unwrap();

    // Повтор после переподключения канала управления приходит без токена: переменные идущего назначения остаются.
    ask(&harness.server, &start(1, None)).await.unwrap();
    assert_eq!(
        harness.run(1, "echo [${GITHUB_TOKEN:+set}]").await,
        "[set]\n",
        "повторный Start сбросил назначение"
    );

    ask(&harness.server, &end(2)).await.unwrap();
    assert_eq!(
        harness.data.assignment(),
        Some(Assignment(1)),
        "End чужого назначения остановил идущее"
    );
    assert_eq!(harness.run(1, "echo [${GITHUB_TOKEN:+set}]").await, "[set]\n");
}

#[tokio::test]
async fn start_of_a_new_assignment_leaves_no_variables_of_the_previous_one() {
    let harness = serve_requests();
    ask(&harness.server, &start(1, Some("ghp_first"))).await.unwrap();
    ask(&harness.server, &start(2, None)).await.unwrap();

    assert_eq!(harness.data.assignment(), Some(Assignment(2)));
    assert_eq!(
        harness.run(2, "echo [${GITHUB_TOKEN:+set}]").await,
        "[]\n",
        "токен прежнего назначения остался"
    );
    // Команда прежнего назначения, пришедшая с опозданием, не запускается.
    assert_eq!(harness.run(1, "echo ran").await, "", "команда прежнего назначения запущена");
}

#[tokio::test]
async fn background_process_of_the_previous_assignment_is_killed_by_end() {
    let harness = serve_requests();
    ask(&harness.server, &start(1, None)).await.unwrap();
    // Команда кончается, фоновый процесс остаётся: так задумано, пока назначение идёт.
    harness.run(1, "sleep 300 >/dev/null 2>&1 & echo $! > bg.pid").await;
    let pid = fs::read_to_string(harness.dir.path().join("bg.pid")).unwrap();
    assert!(
        fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok(),
        "фоновый процесс не пережил свою команду"
    );

    ask(&harness.server, &end(1)).await.unwrap();
    harness.wait_dead("bg.pid").await;
}

#[tokio::test]
async fn background_process_of_the_previous_assignment_is_killed_by_start_of_the_next() {
    let harness = serve_requests();
    ask(&harness.server, &start(1, None)).await.unwrap();
    harness.run(1, "sleep 300 >/dev/null 2>&1 & echo $! > bg.pid").await;

    ask(&harness.server, &start(2, None)).await.unwrap();
    harness.wait_dead("bg.pid").await;
}

#[tokio::test]
async fn request_outside_the_runner_set_closes_the_channel() {
    let harness = serve_requests();
    // Не вариант `ToRunner`: тег варианта вне набора.
    let reply = harness.server.request(vec![0xff, 0xff, 0xff].into()).await;
    assert!(reply.is_err(), "запрос не из набора runner получил ответ");
    tokio::time::timeout(Duration::from_secs(5), harness.server.closed())
        .await
        .expect("канал не закрыт после запроса не из набора runner");
}

#[tokio::test]
async fn clone_runner_refuses_to_create_clones() {
    let harness = serve_requests();
    let request = ToRunner::Clone(protocol::CloneRequest {
        dir: "/srv/site".to_owned(),
    });
    let error = ask_clone(&harness.server, &request).await.unwrap_err();
    assert!(format!("{error}").contains("клонов"), "клон не отказал в клоне клона: {error}");
}

async fn ask_clone(server: &Mux, request: &ToRunner) -> Result<(), Error> {
    let body = server.request(protocol::encode(request).unwrap().into()).await.unwrap();
    protocol::decode_body::<Result<protocol::CloneReply, Error>>(&body)
        .unwrap()
        .map(|_| ())
}
