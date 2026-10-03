//! `relay` на виртуальном времени и мост над мультиплексором на `tokio::io::duplex`.

use nerve::mux::{Incoming, Mux, Requests};
use nerve::protocol::{ExecExit, Stream};
use tokio::task::JoinHandle;
use tokio::time::advance;

use super::*;

struct Relayed {
    events: mpsc::Sender<Wire>,
    replies: mpsc::Receiver<Reply>,
    cancel: oneshot::Sender<()>,
    stop_requested: oneshot::Receiver<()>,
    task: JoinHandle<Reply>,
}

/// Запускает `relay` и даёт ему начать отсчёт молчания до того, как тест сдвинет время.
async fn start() -> Relayed {
    let (events, mut received) = mpsc::channel(16);
    let (reply, replies) = mpsc::channel(16);
    let (cancel, cancel_rx) = oneshot::channel();
    let (stop_runner, stop_requested) = oneshot::channel();
    let task = tokio::spawn(async move { relay(&mut received, &reply, cancel_rx, stop_runner).await });
    settle().await;
    Relayed {
        events,
        replies,
        cancel,
        stop_requested,
        task,
    }
}

impl Relayed {
    async fn send(&self, event: ExecEvent) {
        self.events.send(Ok(event)).await.unwrap();
    }
}

async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

fn output(stream: Stream, text: &str) -> ExecEvent {
    ExecEvent::Output {
        stream,
        data: text.as_bytes().to_vec(),
    }
}

fn exit(status: protocol::ExitStatus, cwd: &str) -> ExecEvent {
    ExecEvent::Exit(ExecExit {
        status,
        cwd: cwd.to_owned(),
    })
}

fn drain(replies: &mut mpsc::Receiver<Reply>) -> Vec<Reply> {
    std::iter::from_fn(|| replies.try_recv().ok()).collect()
}

#[tokio::test(start_paused = true)]
async fn stdout_and_stderr_go_to_one_output_in_order_and_exit_carries_code_and_cwd() {
    let mut relayed = start().await;
    relayed.send(output(Stream::Stdout, "out1 ")).await;
    relayed.send(output(Stream::Stderr, "err ")).await;
    relayed.send(output(Stream::Stdout, "out2")).await;
    relayed.send(exit(protocol::ExitStatus::Code(7), "/work/sub")).await;
    let last = relayed.task.await.unwrap();
    assert_eq!(
        drain(&mut relayed.replies),
        [
            Reply::Output(b"out1 ".to_vec()),
            Reply::Output(b"err ".to_vec()),
            Reply::Output(b"out2".to_vec())
        ],
        "потоки не слиты в один вывод в порядке прихода"
    );
    assert_eq!(
        last,
        Reply::Exit {
            status: ExitStatus::Code(7),
            cwd: "/work/sub".to_owned()
        },
        "код и cwd завершения искажены"
    );
}

#[tokio::test(start_paused = true)]
async fn relay_waits_for_the_stream_without_its_own_limit() {
    let mut relayed = start().await;
    advance(Duration::from_secs(3600)).await;
    settle().await;
    assert!(!relayed.task.is_finished(), "relay объявил команду потерянной");
    assert!(
        relayed.stop_requested.try_recv().is_err(),
        "runner попросили остановить молчащую команду"
    );
    relayed.send(exit(protocol::ExitStatus::Code(0), "/work")).await;
    assert!(
        matches!(relayed.task.await.unwrap(), Reply::Exit { .. }),
        "итог после долгой тишины потерян"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_asks_the_runner_to_stop_and_the_result_still_arrives() {
    let mut relayed = start().await;
    relayed.cancel.send(()).unwrap();
    settle().await;
    assert!(relayed.stop_requested.try_recv().is_ok(), "отмена оболочки не дошла до runner");
    assert!(!relayed.task.is_finished(), "мост закончил, не дождавшись завершения команды");
    relayed.events.send(Ok(output(Stream::Stdout, "bye"))).await.unwrap();
    relayed
        .events
        .send(Ok(exit(protocol::ExitStatus::Signal(15), "/work")))
        .await
        .unwrap();
    let last = relayed.task.await.unwrap();
    assert_eq!(
        drain(&mut relayed.replies),
        [Reply::Output(b"bye".to_vec())],
        "вывод после отмены потерян"
    );
    assert!(
        matches!(
            last,
            Reply::Exit {
                status: ExitStatus::Signal(15),
                ..
            }
        ),
        "итог после отмены потерян: {last:?}"
    );
}

/// Мост и «runner» — мультиплексор по другую сторону `duplex`, которым управляет сам тест.
struct Stand {
    epoch: Epoch,
    runner: Mux,
    requests: Requests,
    link: tokio::sync::watch::Sender<Option<Mux>>,
    tasks: mpsc::Sender<ShellTask>,
    /// Сторона сессии текущего канала: её держит тест, как держала бы сессия.
    session: (Mux, Requests),
}

fn stand() -> Stand {
    let (near, far) = tokio::io::duplex(64 * 1024);
    let (session, session_requests) = Mux::new(near);
    let (runner, requests) = Mux::new(far);
    let (link_handle, link) = Link::fixed(Some(session.clone()));
    let epoch = Epoch::new();
    let (tasks, received) = mpsc::channel(4);
    tokio::spawn(Bridge::new(link_handle, epoch.clone()).serve(received));
    Stand {
        epoch,
        runner,
        requests,
        link,
        tasks,
        session: (session, session_requests),
    }
}

struct Shell {
    replies: mpsc::Receiver<Reply>,
    _cancel: oneshot::Sender<()>,
}

impl Stand {
    async fn run(&self, id: u64, command: &str, cwd: &str) -> Shell {
        let (reply, replies) = mpsc::channel(16);
        let (cancel, cancel_rx) = oneshot::channel();
        self.tasks
            .send(ShellTask {
                id,
                command: command.to_owned(),
                cwd: cwd.to_owned(),
                reply,
                cancel: cancel_rx,
            })
            .await
            .unwrap();
        Shell { replies, _cancel: cancel }
    }

    /// Ждёт запрос от моста и возвращает его разобранным вместе с возможностью ответить.
    async fn next_request(&mut self) -> (DataRequest, Incoming) {
        let incoming = timeout(Duration::from_secs(10), self.requests.recv())
            .await
            .expect("мост не отправил запрос")
            .expect("канал закрыт");
        let request = protocol::decode_body(incoming.body()).expect("запрос моста не разобран");
        (request, incoming)
    }

    /// Обрывает канал и открывает новый: так переподключается runner. Старый мультиплексор остаётся в канале сессии
    /// закрытым, пока его не сменит новый.
    async fn reconnect(&mut self) {
        self.disconnect().await;
        self.connect();
    }

    async fn disconnect(&mut self) {
        self.runner.close();
        self.session.0.closed().await;
    }

    fn connect(&mut self) {
        let (near, far) = tokio::io::duplex(64 * 1024);
        let (session, session_requests) = Mux::new(near);
        let (runner, requests) = Mux::new(far);
        self.link.send_replace(Some(session.clone()));
        self.runner = runner;
        self.requests = requests;
        self.session = (session, session_requests);
    }

    async fn next_exec(&mut self) -> (ExecRequest, Incoming) {
        match self.next_request().await {
            (DataRequest::Exec(request), incoming) => (request, incoming),
            (other, _) => panic!("вместо команды пришло {other:?}"),
        }
    }
}

async fn answer(incoming: &Incoming, event: &ExecEvent) {
    let last = matches!(event, ExecEvent::Exit(_) | ExecEvent::Err(_));
    incoming.respond(Bytes::from(protocol::encode(event).unwrap()), last).await.unwrap();
}

async fn next_reply(shell: &mut Shell) -> Reply {
    timeout(Duration::from_secs(10), shell.replies.recv())
        .await
        .expect("оболочка не получила ответа")
        .expect("канал ответов оболочки закрыт без итога")
}

#[tokio::test]
async fn command_goes_to_the_runner_and_the_stream_comes_back_with_exit_and_cwd() {
    let mut stand = stand();
    let mut shell = stand.run(41, "echo hi; cd sub", "/work").await;
    let (request, incoming) = stand.next_exec().await;
    assert_eq!(
        request,
        ExecRequest {
            id: 41,
            command: "echo hi; cd sub".to_owned(),
            cwd: "/work".to_owned(),
            timeout_ms: None
        },
        "команда оболочки дошла до runner искажённой"
    );
    answer(
        &incoming,
        &ExecEvent::Output {
            stream: Stream::Stdout,
            data: b"hi\n".to_vec(),
        },
    )
    .await;
    answer(
        &incoming,
        &ExecEvent::Output {
            stream: Stream::Stderr,
            data: b"warn\n".to_vec(),
        },
    )
    .await;
    let before = stand.epoch.current();
    answer(
        &incoming,
        &ExecEvent::Exit(ExecExit {
            status: protocol::ExitStatus::Code(0),
            cwd: "/work/sub".to_owned(),
        }),
    )
    .await;
    assert_eq!(
        next_reply(&mut shell).await,
        Reply::Output(b"hi\n".to_vec()),
        "stdout не дошёл до оболочки"
    );
    assert_eq!(
        next_reply(&mut shell).await,
        Reply::Output(b"warn\n".to_vec()),
        "stderr не дошёл тем же выходом"
    );
    let last = next_reply(&mut shell).await;
    assert_eq!(
        last,
        Reply::Exit {
            status: ExitStatus::Code(0),
            cwd: "/work/sub".to_owned()
        },
        "завершение не дошло с кодом и cwd"
    );
    assert!(
        stand.epoch.current() > before,
        "после команды эпоха FUSE не сменилась до ответа оболочке"
    );
}

fn output_of(text: &str) -> ExecEvent {
    ExecEvent::Output {
        stream: Stream::Stdout,
        data: text.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn command_survives_a_broken_channel_and_its_rest_is_collected_after_reconnect() {
    let mut stand = stand();
    let mut shell = stand.run(1, "build", "/work").await;
    let (_, incoming) = stand.next_exec().await;
    answer(&incoming, &output_of("partial")).await;
    answer(&incoming, &ExecEvent::Heartbeat).await;
    assert_eq!(next_reply(&mut shell).await, Reply::Output(b"partial".to_vec()));
    drop(incoming);
    stand.reconnect().await;

    // Команда уже идёт на runner: после переподключения мост просит остаток, а не запускает её снова. Получено два
    // сообщения — вывод и heartbeat: runner нумерует все.
    let (request, rest) = stand.next_request().await;
    assert_eq!(
        request,
        DataRequest::Attach(AttachRequest { id: 1, received: 2 }),
        "после переподключения мост не забрал остаток потока с первого неполученного сообщения"
    );
    answer(&rest, &output_of("rest")).await;
    answer(
        &rest,
        &ExecEvent::Exit(ExecExit {
            status: protocol::ExitStatus::Code(3),
            cwd: "/work".to_owned(),
        }),
    )
    .await;
    assert_eq!(
        next_reply(&mut shell).await,
        Reply::Output(b"rest".to_vec()),
        "остаток вывода потерян"
    );
    assert_eq!(
        next_reply(&mut shell).await,
        Reply::Exit {
            status: ExitStatus::Code(3),
            cwd: "/work".to_owned()
        },
        "итог команды после переподключения не дошёл"
    );
}

#[tokio::test]
async fn runner_without_the_command_after_reconnect_is_reported_as_unknown_result() {
    let mut stand = stand();
    let mut shell = stand.run(1, "build", "/work").await;
    let (_, incoming) = stand.next_exec().await;
    drop(incoming);
    stand.reconnect().await;
    let (request, rest) = stand.next_request().await;
    assert!(
        matches!(request, DataRequest::Attach(_)),
        "после обрыва мост не запросил остаток потока"
    );
    answer(&rest, &ExecEvent::Err(Error::NotFound)).await;
    match next_reply(&mut shell).await {
        Reply::Failed(text) => assert_eq!(text, "runner перезапущен, итог неизвестен"),
        other => panic!("итог неизвестной команды не объявлен: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn silence_of_60_seconds_on_an_open_channel_fails_the_command() {
    let mut stand = stand();
    let mut shell = stand.run(1, "hang", "/work").await;
    let (_, _incoming) = stand.next_exec().await;
    advance(Duration::from_secs(59)).await;
    settle().await;
    assert!(shell.replies.try_recv().is_err(), "команда объявлена потерянной раньше 60 с тишины");
    advance(Duration::from_secs(2)).await;
    let last = shell.replies.recv().await.unwrap();
    assert!(
        matches!(&last, Reply::Failed(text) if text.contains("runner пропал")),
        "после 60 с тишины нет ошибки «runner пропал»: {last:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn heartbeat_every_30_seconds_keeps_the_command_alive() {
    let mut stand = stand();
    let mut shell = stand.run(1, "slow", "/work").await;
    let (_, incoming) = stand.next_exec().await;
    for _ in 0..10 {
        advance(Duration::from_secs(30)).await;
        answer(&incoming, &ExecEvent::Heartbeat).await;
        settle().await;
    }
    advance(Duration::from_secs(50)).await;
    answer(&incoming, &output_of("x")).await;
    settle().await;
    advance(Duration::from_secs(50)).await;
    settle().await;
    answer(
        &incoming,
        &ExecEvent::Exit(ExecExit {
            status: protocol::ExitStatus::Code(0),
            cwd: "/work".to_owned(),
        }),
    )
    .await;
    assert_eq!(shell.replies.recv().await.unwrap(), Reply::Output(b"x".to_vec()));
    let last = shell.replies.recv().await.unwrap();
    assert!(
        matches!(last, Reply::Exit { .. }),
        "команда с живым runner завершилась ошибкой: {last:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn broken_channel_for_five_minutes_does_not_fail_the_command() {
    let mut stand = stand();
    let mut shell = stand.run(1, "build", "/work").await;
    let (_, incoming) = stand.next_exec().await;
    drop(incoming);
    stand.disconnect().await;
    advance(Duration::from_secs(300)).await;
    settle().await;
    assert!(shell.replies.try_recv().is_err(), "обрыв канала засчитан как молчание runner");
    stand.connect();
    let (request, rest) = stand.next_request().await;
    assert!(
        matches!(request, DataRequest::Attach(_)),
        "после обрыва мост не запросил остаток потока"
    );
    advance(Duration::from_secs(59)).await;
    settle().await;
    assert!(
        shell.replies.try_recv().is_err(),
        "срок молчания не начат заново после переподключения"
    );
    answer(
        &rest,
        &ExecEvent::Exit(ExecExit {
            status: protocol::ExitStatus::Code(0),
            cwd: "/work".to_owned(),
        }),
    )
    .await;
    let last = shell.replies.recv().await.unwrap();
    assert!(matches!(last, Reply::Exit { .. }), "итог после долгого обрыва потерян: {last:?}");
}
