//! Обслуживание запросов сессии через мультиплексор над `tokio::io::duplex` и настоящий рабочий каталог.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nerve::mux::{Mux, Responses};
use nerve::protocol::{self, Assignment, Content, ExecExit, ExitStatus, Expect, FsReply, FsResponse, Stream};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

const HANG_LIMIT: Duration = Duration::from_secs(20);
const ASSIGNMENT: Assignment = Assignment(1);

fn project(request: FsRequest) -> DataRequest {
    DataRequest::Fs {
        root: Root::Project,
        request,
    }
}

struct Runner {
    /// Сторона сессии: мультиплексор и её входящие запросы, которые держатся вместе с ним.
    client: (Mux, Requests),
    dir: TempDir,
    workdir: Arc<Workdir>,
    executor: Arc<Executor>,
    jobs: Arc<Jobs>,
}

/// Канал сессии к уже работающему runner: `serve` на другом конце.
fn link(workdir: &Arc<Workdir>, executor: &Arc<Executor>, jobs: &Arc<Jobs>) -> (Mux, Requests) {
    link_with_home(workdir, None, executor, jobs)
}

fn link_with_home(workdir: &Arc<Workdir>, home: Option<Arc<Workdir>>, executor: &Arc<Executor>, jobs: &Arc<Jobs>) -> (Mux, Requests) {
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (client, client_requests) = Mux::new(near);
    let (_runner, requests) = Mux::new(far);
    let permits = Arc::new(Semaphore::new(usize::from(MAX_IN_FLIGHT)));
    tokio::spawn(serve(
        requests,
        Arc::clone(workdir),
        home,
        Arc::clone(executor),
        Arc::clone(jobs),
        permits,
        Arc::default(),
    ));
    (client, client_requests)
}

fn start() -> Runner {
    let dir = tempfile::tempdir().unwrap();
    let workdir = Arc::new(Workdir::open(&fs::canonicalize(dir.path()).unwrap()).unwrap());
    let executor = Arc::new(Executor::new(
        Arc::clone(&workdir),
        dir.path().join("no-such.env"),
        Vec::new(),
        None,
    ));
    executor.start(ASSIGNMENT, BTreeMap::new());
    let jobs = Arc::new(Jobs::new(ASSIGNMENT));
    Runner {
        client: link(&workdir, &executor, &jobs),
        dir,
        workdir,
        executor,
        jobs,
    }
}

impl Runner {
    /// Сессия вернулась: старый канал оборван, новый ведёт к тому же runner с теми же командами.
    fn reconnect(&mut self) {
        self.client.0.close();
        self.client = link(&self.workdir, &self.executor, &self.jobs);
    }

    async fn attach(&self, id: u64, received: u64) -> Responses {
        let body = protocol::encode(&DataRequest::Attach(AttachRequest { id, received })).unwrap();
        self.raw(body).await
    }

    fn path(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap()
    }

    async fn raw(&self, body: Vec<u8>) -> Responses {
        self.client.0.call(Bytes::from(body)).await.unwrap()
    }

    async fn call(&self, request: FsRequest) -> FsResponse {
        self.call_data(project(request)).await
    }

    async fn call_data(&self, request: DataRequest) -> FsResponse {
        let mut replies = self.raw(protocol::encode(&request).unwrap()).await;
        let reply = tokio::time::timeout(HANG_LIMIT, replies.next()).await.unwrap().unwrap().unwrap();
        protocol::decode_body(&reply).unwrap()
    }

    async fn exec(&self, id: u64, command: &str) -> Responses {
        let request = ExecRequest {
            id,
            command: command.to_owned(),
            cwd: self.path().display().to_string(),
            timeout_ms: None,
        };
        self.raw(protocol::encode(&DataRequest::Exec(request)).unwrap()).await
    }
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn write(path: &str, expect: Expect, bytes: &[u8]) -> FsRequest {
    FsRequest::Write {
        path: path.into(),
        expect,
        content: Content(bytes.to_vec()),
    }
}

/// Читает поток ответов на команду до последнего сообщения.
async fn collect(mut replies: Responses) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    tokio::time::timeout(HANG_LIMIT, async {
        while let Some(body) = replies.next().await.unwrap() {
            events.push(protocol::decode_body::<ExecEvent>(&body).unwrap());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("поток ответов не закончился за {HANG_LIMIT:?}: {events:?}"));
    events
}

fn stdout(events: &[ExecEvent]) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            ExecEvent::Output {
                stream: Stream::Stdout,
                data,
            } => Some(data.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn write_read_and_conflict_work_through_the_channel() {
    let runner = start();
    let path = runner.dir.path().join("file.txt");

    let written = runner.call(write("file.txt", Expect::Absent, b"one")).await;
    assert!(
        matches!(&written, Ok(FsReply::Written { sha256, .. }) if *sha256 == sha(b"one")),
        "создание файла: {written:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"one");

    let read = runner.call(FsRequest::Read { path: "file.txt".into() }).await;
    assert!(
        matches!(&read, Ok(FsReply::Read { content: Content(bytes), sha256, .. }) if bytes == b"one" && *sha256 == sha(b"one")),
        "чтение: {read:?}"
    );

    let conflict = runner.call(write("file.txt", Expect::Hash(sha(b"stale")), b"two")).await;
    assert_eq!(conflict, Err(Error::Conflict));
    assert_eq!(fs::read(&path).unwrap(), b"one", "конфликт изменил файл");

    let updated = runner.call(write("file.txt", Expect::Hash(sha(b"one")), b"two")).await;
    assert!(updated.is_ok(), "запись с верным хешем: {updated:?}");
    assert_eq!(fs::read(&path).unwrap(), b"two");
}

#[tokio::test]
async fn file_larger_than_a_frame_and_a_megabyte_is_written_and_read_back_intact() {
    let runner = start();
    // Сообщение в полмебибайта, не кратное размеру кадра: файл идёт кадрами.
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i: u32| (i % 251) as u8).collect();

    let written = runner.call(write("big", Expect::Absent, &big)).await;
    assert!(
        matches!(&written, Ok(FsReply::Written { sha256, .. }) if *sha256 == sha(&big)),
        "запись: {written:?}"
    );
    assert_eq!(fs::read(runner.dir.path().join("big")).unwrap(), big, "на диске не тот файл");

    let read = runner.call(FsRequest::Read { path: "big".into() }).await;
    assert!(
        matches!(&read, Ok(FsReply::Read { content: Content(bytes), sha256, .. }) if *bytes == big && *sha256 == sha(&big)),
        "чтение большого файла вернуло другое содержимое"
    );
}

/// Канал сессии к runner через ретранслятор: он считает байты запросов и после `limit` байт рвёт поток.
fn link_through_relay(runner: &Runner, limit: u64) -> (Mux, Requests, Arc<AtomicU64>) {
    let (near, relay_near) = tokio::io::duplex(64 * 1024);
    let (relay_far, far) = tokio::io::duplex(64 * 1024);
    let (client, client_requests) = Mux::new(near);
    let (_runner_mux, requests) = Mux::new(far);
    tokio::spawn(serve(
        requests,
        Arc::clone(&runner.workdir),
        None,
        Arc::clone(&runner.executor),
        Arc::clone(&runner.jobs),
        Arc::new(Semaphore::new(usize::from(MAX_IN_FLIGHT))),
        Arc::default(),
    ));
    let sent = Arc::new(AtomicU64::new(0));
    let (mut near_read, mut near_write) = tokio::io::split(relay_near);
    let (mut far_read, mut far_write) = tokio::io::split(relay_far);
    let counter = Arc::clone(&sent);
    tokio::spawn(async move {
        let mut chunk = vec![0; 4096];
        while let Ok(n) = near_read.read(&mut chunk).await {
            let total = counter.fetch_add(n as u64, Ordering::SeqCst) + n as u64;
            if n == 0 || total > limit || far_write.write_all(&chunk[..n]).await.is_err() {
                break;
            }
        }
        // Конец ретрансляции закрывает обе стороны: канал оборван посреди сообщения.
        far_write.shutdown().await.unwrap_or_default();
    });
    tokio::spawn(async move {
        tokio::io::copy(&mut far_read, &mut near_write).await.unwrap_or_default();
    });
    (client, client_requests, sent)
}

/// Мелкий запрос не ждёт большую запись: кадры чередуются, и `Stat` отвечен, пока запись ещё в пути.
#[tokio::test]
async fn small_request_is_answered_while_a_large_write_is_in_flight() {
    let runner = start();
    let big = vec![5u8; 16 * 1024 * 1024];
    let (client, _requests, sent) = link_through_relay(&runner, u64::MAX);
    let body = protocol::encode(&project(write("big", Expect::Absent, &big))).unwrap();
    let mut writing = client.call(Bytes::from(body)).await.unwrap();

    let body = protocol::encode(&project(FsRequest::Stat { path: String::new() })).unwrap();
    let stat = tokio::time::timeout(HANG_LIMIT, client.request(Bytes::from(body)))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(protocol::decode_body::<FsResponse>(&stat).unwrap(), Ok(FsReply::Stat(_))));
    let relayed = sent.load(Ordering::SeqCst);
    assert!(
        relayed < big.len() as u64 / 2,
        "мелкий запрос дождался почти всей записи ({relayed} из {} байт): кадры не чередуются",
        big.len()
    );
    let reply = tokio::time::timeout(HANG_LIMIT, writing.next()).await.unwrap().unwrap().unwrap();
    assert!(
        protocol::decode_body::<FsResponse>(&reply).unwrap().is_ok(),
        "большая запись не удалась"
    );
}

/// Обрыв канала посреди большой записи не трогает файл и не оставляет временных файлов; повтор после
/// переподключения записывает файл целиком.
#[tokio::test]
async fn write_cut_in_the_middle_leaves_the_file_intact_and_a_repeat_succeeds() {
    let mut runner = start();
    fs::write(runner.dir.path().join("f"), "old").unwrap();
    let big = vec![9u8; 8 * 1024 * 1024];
    let (client, _requests, _sent) = link_through_relay(&runner, 2 * 1024 * 1024);

    let body = protocol::encode(&project(write("f", Expect::Hash(sha(b"old")), &big))).unwrap();
    let mut replies = client.call(Bytes::from(body)).await.unwrap();
    let outcome = tokio::time::timeout(HANG_LIMIT, replies.next()).await.expect("обрыв не замечен");
    assert!(outcome.is_err(), "ответ на оборванную запись: {outcome:?}");
    assert_eq!(fs::read(runner.dir.path().join("f")).unwrap(), b"old", "обрыв изменил файл");
    let names: Vec<_> = fs::read_dir(runner.dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["f"], "после обрыва остались временные файлы");

    runner.reconnect();
    let repeated = runner.call(write("f", Expect::Hash(sha(b"old")), &big)).await;
    assert!(
        matches!(&repeated, Ok(FsReply::Written { sha256, .. }) if *sha256 == sha(&big)),
        "повтор записи: {repeated:?}"
    );
    assert_eq!(fs::read(runner.dir.path().join("f")).unwrap(), big);
}

#[tokio::test]
async fn rename_moves_a_file_and_conflicts_leave_it_in_place_through_the_channel() {
    let runner = start();
    fs::write(runner.dir.path().join("a"), "one").unwrap();
    let rename = |expect_from: &[u8], expect_to: Expect| FsRequest::Rename {
        from: "a".into(),
        to: "b".into(),
        expect_from: sha(expect_from),
        expect_to,
    };

    let stale = runner.call(rename(b"stale", Expect::Absent)).await;
    assert_eq!(stale, Err(Error::Conflict));
    assert!(runner.dir.path().join("a").exists(), "конфликт перенёс файл");

    let moved = runner.call(rename(b"one", Expect::Absent)).await;
    assert!(
        matches!(moved, Ok(FsReply::Renamed(attr)) if attr.size == 3),
        "перенос с верным хешем: {moved:?}"
    );
    assert_eq!(fs::read(runner.dir.path().join("b")).unwrap(), b"one");
    assert!(!runner.dir.path().join("a").exists(), "источник остался");

    let missing = runner.call(rename(b"one", Expect::Absent)).await;
    assert_eq!(missing, Err(Error::NotFound), "повторный перенос того же источника");
}

#[tokio::test]
async fn path_outside_workdir_is_refused_through_the_channel() {
    let runner = start();
    let outside = runner.dir.path().parent().unwrap().join(format!(
        "iapetus-outside-{}",
        runner.dir.path().file_name().unwrap().to_str().unwrap()
    ));
    fs::write(&outside, "SECRET").unwrap();
    let relative = format!("../{}", outside.file_name().unwrap().to_str().unwrap());

    let read = runner.call(FsRequest::Read { path: relative.clone() }).await;
    let written = runner.call(write(&relative, Expect::Hash(sha(b"SECRET")), b"X")).await;
    let content = fs::read(&outside).unwrap();
    fs::remove_file(&outside).unwrap();

    assert_eq!(read, Err(Error::OutsideWorkdir));
    assert_eq!(written, Err(Error::OutsideWorkdir));
    assert_eq!(content, b"SECRET", "файл вне рабочего каталога изменён");
}

#[tokio::test]
async fn garbage_request_gets_an_error_and_the_runner_keeps_serving() {
    let runner = start();
    for garbage in [
        vec![],
        vec![0xFF; 32],
        vec![9, 1, 2, 3],
        protocol::encode(&project(FsRequest::Stat { path: String::new() }))
            .unwrap()
            .repeat(2),
    ] {
        let mut replies = runner.raw(garbage.clone()).await;
        let reply = tokio::time::timeout(HANG_LIMIT, replies.next()).await.unwrap().unwrap().unwrap();
        let response: FsResponse = protocol::decode_body(&reply).unwrap();
        assert!(
            matches!(response, Err(Error::Io(_))),
            "мусор {garbage:?} не дал ошибку: {response:?}"
        );
    }
    let stat = runner.call(FsRequest::Stat { path: String::new() }).await;
    assert!(
        matches!(stat, Ok(FsReply::Stat(_))),
        "после мусора runner перестал отвечать: {stat:?}"
    );
}

/// Команда с известным числом событий: ждёт файл `go`, печатает две строки и записывает запуск в `runs`.
const WAITING: &str = "echo x >> runs; echo first; while [ ! -e go ]; do sleep 0.02; done; echo second";

#[tokio::test]
async fn command_keeps_running_when_the_channel_breaks_and_the_session_collects_the_rest_after_reconnect() {
    let mut runner = start();
    let mut replies = runner.exec(7, WAITING).await;
    let first = tokio::time::timeout(HANG_LIMIT, replies.next()).await.unwrap().unwrap().unwrap();
    assert!(matches!(protocol::decode_body::<ExecEvent>(&first), Ok(ExecEvent::Output { data, .. }) if data == b"first\n"));

    runner.reconnect();
    drop(replies);
    fs::write(runner.dir.path().join("go"), "").unwrap();
    let events = collect(runner.attach(7, 1).await).await;
    assert_eq!(
        stdout(&events),
        "second\n",
        "остаток вывода после переподключения потерян или повторён"
    );
    assert!(
        matches!(
            events.last(),
            Some(ExecEvent::Exit(ExecExit {
                status: ExitStatus::Code(0),
                ..
            }))
        ),
        "итог команды потерян: {events:?}"
    );
    assert_eq!(
        fs::read_to_string(runner.dir.path().join("runs")).unwrap(),
        "x\n",
        "команда запущена повторно"
    );
}

#[tokio::test]
async fn command_started_again_with_a_known_id_gives_the_stored_result_and_never_runs_twice() {
    let runner = start();
    let first = collect(runner.exec(3, "echo x >> runs; echo done").await).await;
    assert!(matches!(first.last(), Some(ExecEvent::Exit(_))));
    // Команда уже завершилась, но её итог ещё ждёт сессию: повтор запроса отдаёт его, а не запускает команду.
    let again = collect(runner.exec(3, "echo x >> runs").await).await;
    assert_eq!(again, first, "повтор завершённого id не вернул сохранённый итог");
    assert_eq!(
        fs::read_to_string(runner.dir.path().join("runs")).unwrap(),
        "x\n",
        "команда с занятым id запущена повторно"
    );
}

#[tokio::test]
async fn cancel_of_a_command_the_session_did_not_start_is_refused_and_does_not_touch_the_running_one() {
    let runner = start();
    let mut replies = runner.exec(5, WAITING).await;
    tokio::time::timeout(HANG_LIMIT, replies.next()).await.unwrap().unwrap().unwrap();
    // Новая сессия: журнал прежней сброшен, а её команда ещё не успела остановиться.
    runner.jobs.clear();
    let body = protocol::encode(&DataRequest::Cancel(CancelRequest { id: 5 })).unwrap();
    let mut answer = runner.raw(body).await;
    let answer = tokio::time::timeout(HANG_LIMIT, answer.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(
        protocol::decode_body::<Result<(), Error>>(&answer).unwrap(),
        Err(Error::NotFound),
        "отмена чужой команды принята"
    );
    fs::write(runner.dir.path().join("go"), "").unwrap();
    let events = collect(replies).await;
    assert!(
        matches!(
            events.last(),
            Some(ExecEvent::Exit(ExecExit {
                status: ExitStatus::Code(0),
                ..
            }))
        ),
        "чужая отмена остановила команду: {events:?}"
    );
}

#[tokio::test]
async fn attach_at_any_cut_point_replays_exactly_the_missing_events() {
    let runner = start();
    let full = collect(runner.exec(1, "echo a; echo b >&2; echo c; exit 4").await).await;
    assert!(full.len() >= 3, "команда дала слишком мало событий для проверки: {full:?}");
    for cut in 0..=full.len() {
        let rest = collect(runner.attach(1, cut as u64).await).await;
        if cut == full.len() {
            assert!(
                matches!(rest.as_slice(), [ExecEvent::Err(Error::Io(_))]),
                "остаток после последнего события: {rest:?}"
            );
        } else {
            assert_eq!(rest, full[cut..], "после обрыва на сообщении {cut} пришло не то, что осталось");
        }
    }
}

#[tokio::test]
async fn result_of_a_previous_session_is_not_kept() {
    let runner = start();
    collect(runner.exec(1, "echo a").await).await;
    runner.jobs.clear();
    let events = collect(runner.attach(1, 0).await).await;
    assert_eq!(events, [ExecEvent::Err(Error::NotFound)], "итог прежней сессии остался");
}

/// Домашний каталог runner этот обработчик не обслуживает: запрос с `Root::Home` — путь вне рабочего каталога, а не
/// чтение рабочего каталога по ошибке.
#[tokio::test]
async fn home_root_is_refused_as_outside_the_workdir() {
    let runner = start();
    fs::write(runner.dir.path().join("f"), b"x").unwrap();
    let response = runner
        .call_data(DataRequest::Fs {
            root: Root::Home,
            request: FsRequest::Stat { path: "f".into() },
        })
        .await;
    assert_eq!(
        response,
        Err(Error::OutsideWorkdir),
        "запрос с корнем Home выполнен в рабочем каталоге"
    );
}

/// Запросы с `Root::Home` идут в `$HOME`, а не в проект: запись в `$HOME` не попадает в рабочий каталог, чтение видит
/// файлы `$HOME`, а `.claude` в нём закрыт.
#[tokio::test]
async fn home_root_reads_and_writes_the_home_directory_and_keeps_claude_closed() {
    let mut runner = start();
    let home = tempfile::tempdir().unwrap();
    let home_path = fs::canonicalize(home.path()).unwrap();
    fs::write(home_path.join("note"), b"home").unwrap();
    fs::create_dir(home_path.join(".claude")).unwrap();
    fs::write(home_path.join(".claude/token"), b"secret").unwrap();
    let served = Arc::new(Workdir::open_home(&home_path, &runner.path()).unwrap());
    runner.client = link_with_home(&runner.workdir, Some(served), &runner.executor, &runner.jobs);
    let at_home = |request| DataRequest::Fs { root: Root::Home, request };

    let read = runner.call_data(at_home(FsRequest::Read { path: "note".into() })).await;
    assert!(
        matches!(&read, Ok(FsReply::Read { content: Content(bytes), .. }) if bytes == b"home"),
        "файл $HOME не прочитан: {read:?}"
    );
    let written = runner.call_data(at_home(write("new", Expect::Absent, b"fresh"))).await;
    assert!(written.is_ok(), "запись в $HOME: {written:?}");
    assert_eq!(fs::read(home_path.join("new")).unwrap(), b"fresh", "файл не лёг в $HOME");
    assert!(!runner.path().join("new").exists(), "запись в $HOME попала в проект");
    let secret = runner
        .call_data(at_home(FsRequest::Read {
            path: ".claude/token".into(),
        }))
        .await;
    assert_eq!(secret, Err(Error::Access), "конфигурация агента на runner прочитана");
}
