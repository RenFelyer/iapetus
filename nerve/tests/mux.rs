//! Утверждения о мультиплексоре: чужая версия, обрыв потока в любом месте и порядок уведомлений.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use proptest::prelude::*;
use tokio::io::{AsyncWriteExt, DuplexStream};

use super::frame::{LAST, Outgoing, REQUEST, Scheduler, encode_frame};
use super::{Closed, FRAME_BODY, Mux, Requests};
use crate::protocol::VERSION;

/// Сколько ждать того, что должно случиться сразу: дольше — зависание.
const HANG: Duration = Duration::from_secs(20);

fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

async fn within<F: Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(HANG, future)
        .await
        .unwrap_or_else(|_| panic!("зависание: {what}"))
}

fn hello(version: u16) -> BytesMut {
    let mut out = BytesMut::new();
    encode_frame(&mut out, 0, 0, &version.to_be_bytes());
    out
}

fn body_strategy() -> impl Strategy<Value = Vec<u8>> {
    // Границы кадра отдельно: ошибки на стыках кадров живут именно там.
    prop_oneof![
        (0..64usize).prop_map(|n| vec![1; n]),
        Just(vec![2; FRAME_BODY - 1]),
        Just(vec![3; FRAME_BODY]),
        Just(vec![4; FRAME_BODY + 1]),
        Just(vec![5; 2 * FRAME_BODY]),
        prop::collection::vec(any::<u8>(), 0..3 * FRAME_BODY),
    ]
}

fn pair() -> ((Mux, Requests), (Mux, Requests)) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    (Mux::new(a), Mux::new(b))
}

proptest! {
    /// Изменения на runner применяются по порядку: большое уведомление не должно пропустить вперёд мелкое после него.
    #[test]
    fn notifications_arrive_in_the_order_they_were_sent_whatever_their_sizes(bodies in prop::collection::vec(body_strategy(), 1..12)) {
        let in_order = run(async {
            let ((sender, _), (_receiver, mut requests)) = pair();
            for body in &bodies {
                sender.notify(Bytes::from(body.clone())).await.unwrap();
            }
            for body in &bodies {
                let got = within("уведомление", requests.recv()).await.unwrap();
                if got.body()[..] != body[..] {
                    return false;
                }
            }
            true
        });
        prop_assert!(in_order, "уведомления пришли не в порядке отправки");
    }
}

// ---------------------------------------------------------------- приветствие, мусор и обрыв

/// Mux над одной половиной потока; вторая у теста, как «сырой» собеседник.
fn raw_peer() -> ((Mux, Requests), DuplexStream) {
    let (a, b) = tokio::io::duplex(1 << 20);
    (Mux::new(a), b)
}

#[tokio::test]
async fn peer_with_another_protocol_version_closes_the_channel_with_a_version_error() {
    let ((mux, _), mut peer) = raw_peer();
    peer.write_all(&hello(VERSION + 1)).await.unwrap();
    assert_eq!(within("закрытие", mux.closed()).await, Closed::Version(Some(VERSION + 1)));
}

/// Поток, который собеседник написал целиком: приветствие и три запроса, один из них в двух кадрах.
fn scripted_stream() -> (Vec<u8>, Vec<Vec<u8>>) {
    let bodies = vec![vec![1u8; 10], vec![2u8; FRAME_BODY + 5], vec![3u8; 3]];
    let mut out = hello(VERSION);
    let mut scheduler = Scheduler::default();
    for (i, body) in bodies.iter().enumerate() {
        scheduler.push(Outgoing {
            id: u32::try_from(i).unwrap() + 1,
            flags: REQUEST | LAST,
            body: Bytes::from(body.clone()),
        });
    }
    while !scheduler.is_empty() {
        scheduler.round(&mut out, 1);
    }
    (out.to_vec(), bodies)
}

/// Поток оборван после `k` байт: доставлены только целые сообщения, неискажённые, а закрытие названо ошибкой.
async fn deliver_prefix(stream: &[u8], k: usize) -> (Vec<Bytes>, Closed) {
    let ((mux, mut incoming), mut peer) = raw_peer();
    peer.write_all(&stream[..k]).await.unwrap();
    // Закрывается только запись собеседника: Mux в ответ ещё может писать, и обрыв он увидит по чтению.
    peer.shutdown().await.unwrap();
    let mut delivered = Vec::new();
    while let Some(request) = within("доставка", incoming.recv()).await {
        delivered.push(request.into_body());
    }
    let reason = within("закрытие", mux.closed()).await;
    let call = within("вызов после обрыва", async {
        match mux.call(Bytes::new()).await {
            Err(e) => Err(e),
            Ok(mut responses) => responses.next().await.map(|_| ()),
        }
    })
    .await;
    assert!(call.is_err(), "вызов после обрыва потока на байте {k} не сообщил об ошибке");
    (delivered, reason)
}

#[tokio::test]
async fn stream_cut_at_every_byte_delivers_only_whole_messages_and_reports_the_break() {
    let (stream, bodies) = scripted_stream();
    let mut previous = 0;
    for k in 0..=stream.len() {
        let (delivered, reason) = deliver_prefix(&stream, k).await;
        assert!(matches!(reason, Closed::Io(_)), "обрыв на байте {k}: причина {reason:?}");
        for body in &delivered {
            assert!(
                bodies.iter().any(|expected| expected[..] == body[..]),
                "обрыв на байте {k}: доставлено искажённое сообщение"
            );
        }
        assert!(
            delivered.len() >= previous,
            "обрыв на байте {k}: доставлено меньше, чем при более коротком обрыве"
        );
        previous = delivered.len();
    }
    assert_eq!(previous, bodies.len(), "целый поток доставлен не полностью");
}
