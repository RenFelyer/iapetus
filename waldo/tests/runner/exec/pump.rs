//! Поток вывода без процессов: трубы — `tokio::io::duplex`, время — виртуальное.

use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;
use tokio::time::advance;

use super::*;

struct Pumped {
    stdout: DuplexStream,
    stderr: DuplexStream,
    events: mpsc::Receiver<ExecEvent>,
    _exited: oneshot::Sender<()>,
    task: JoinHandle<Result<(), SinkClosed>>,
}

fn start(secrets: &[&str]) -> Pumped {
    let (stdout, stdout_read) = tokio::io::duplex(4096);
    let (stderr, stderr_read) = tokio::io::duplex(4096);
    let (events_tx, events) = mpsc::channel(64);
    let (exited, exited_rx) = oneshot::channel();
    let secrets = Secrets::new(secrets.iter().map(|secret| secret.as_bytes().to_vec()));
    let task = tokio::spawn(async move { pump(stdout_read, stderr_read, secrets, &events_tx, exited_rx).await });
    Pumped {
        stdout,
        stderr,
        events,
        _exited: exited,
        task,
    }
}

/// Даёт задачам выполнить всё, что они могут без продвижения времени.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

fn drain(events: &mut mpsc::Receiver<ExecEvent>) -> Vec<ExecEvent> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}

#[tokio::test(start_paused = true)]
async fn heartbeat_continues_while_a_secret_prefix_is_held_back() {
    let mut pumped = start(&["ghp_abc123"]);
    pumped.stdout.write_all(b"ghp_ab").await.unwrap();
    settle().await;
    assert!(drain(&mut pumped.events).is_empty(), "начало секрета ушло открыто");
    advance(EXEC_HEARTBEAT).await;
    settle().await;
    assert_eq!(
        drain(&mut pumped.events),
        [ExecEvent::Heartbeat],
        "удержанный хвост выдал себя за вывод и отложил heartbeat"
    );
}

#[tokio::test(start_paused = true)]
async fn secret_split_between_reads_is_masked() {
    let mut pumped = start(&["ghp_abc123"]);
    for piece in ["token ghp_", "abc", "123 end"] {
        pumped.stdout.write_all(piece.as_bytes()).await.unwrap();
        settle().await;
    }
    drop(pumped.stdout);
    drop(pumped.stderr);
    pumped.task.await.unwrap().unwrap();
    let text: String = drain(&mut pumped.events)
        .into_iter()
        .map(|event| match event {
            ExecEvent::Output { data, .. } => String::from_utf8(data).unwrap(),
            other => panic!("лишнее событие {other:?}"),
        })
        .collect();
    assert_eq!(text, "token *** end", "секрет на границе чтений прошёл открыто или вывод искажён");
}
