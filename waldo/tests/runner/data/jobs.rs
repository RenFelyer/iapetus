//! Журнал команд: вытеснение вывода и забывание завершённых команд.

use std::time::Duration;

use nerve::mux::{Mux, Requests};
use nerve::protocol::{ExecExit, ExitStatus, Stream};

use super::*;

const HANG_LIMIT: Duration = Duration::from_secs(20);

fn output(len: usize) -> ExecEvent {
    ExecEvent::Output {
        stream: Stream::Stdout,
        data: vec![b'x'; len],
    }
}

fn exit() -> ExecEvent {
    ExecEvent::Exit(ExecExit {
        status: ExitStatus::Code(0),
        cwd: "/w".to_owned(),
    })
}

/// Канал, у которого на другом конце сессия: она читает ответы на единственный запрос.
async fn ask() -> (Incoming, nerve::mux::Responses, Mux, Requests) {
    let (near, far) = tokio::io::duplex(1 << 20);
    let (session, session_requests) = Mux::new(near);
    let (_runner, mut requests) = Mux::new(far);
    let replies = session.call(Bytes::new()).await.unwrap();
    let incoming = tokio::time::timeout(HANG_LIMIT, requests.recv()).await.unwrap().unwrap();
    (incoming, replies, session, session_requests)
}

async fn events_of(replies: &mut nerve::mux::Responses) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    while let Some(body) = tokio::time::timeout(HANG_LIMIT, replies.next()).await.unwrap().unwrap() {
        events.push(protocol::decode_body(&body).unwrap());
    }
    events
}

#[tokio::test]
async fn output_pushed_out_of_the_journal_is_reported_not_skipped() {
    let job = Job::new();
    for _ in 0..(RETAIN_BYTES >> 20) + 6 {
        job.push(output(1 << 20));
    }
    job.push(exit());
    let first = job.log().first;
    assert!(first > 0, "журнал не вытеснил ничего сверх предела");

    let (incoming, mut replies, _session, _requests) = ask().await;
    job.stream(0, &incoming).await;
    let events = events_of(&mut replies).await;
    assert!(
        matches!(events.as_slice(), [ExecEvent::Err(Error::Io(text))] if text.contains("вытеснен")),
        "вытесненное принято за пустоту или пропущено молча: {events:?}"
    );

    // Хвост журнала цел: два последних события (вывод и итог) доходят.
    let end = {
        let log = job.log();
        log.first + log.events.len() as u64
    };
    let (incoming, mut replies, _session, _requests) = ask().await;
    job.stream(end - 2, &incoming).await;
    let events = events_of(&mut replies).await;
    assert_eq!(events.len(), 2, "хвост журнала: {events:?}");
    assert_eq!(events.last(), Some(&exit()), "остаток журнала не дошёл до итога");
}
