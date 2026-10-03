//! Канал управления: общее для проверок ролей. Через настоящий SSH ходят клиентом `nerve`; сроки (`valid_before`,
//! вход без запроса) проверяются на `tokio::io::duplex` с остановленным временем, без SSH: его keepalive на
//! подмену часов не рассчитан.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nerve::connect::server::{Channel, Login, ask, open};
use nerve::fingerprint::Fingerprint;
use nerve::mux::{Incoming, Mux, Requests};
use nerve::protocol::{self, Error};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{Control, Peer};
use crate::server::ssh::Side;
use crate::server::testing::{Env, Stand, TestNode, report_host};

#[path = "control/enroll.rs"]
mod enroll;
#[path = "control/reassign.rs"]
mod reassign;
#[path = "control/runner.rs"]
mod runner;
#[path = "control/session.rs"]
mod session;
#[path = "control/worker.rs"]
mod worker;

const WAIT: Duration = Duration::from_secs(5);

/// Runner с сертификатом входит на канал управления.
pub(super) async fn runner_channel(stand: &Stand, node: &TestNode) -> Channel {
    let login = Login::Certificate {
        key: &node.key,
        certificate: &node.certificate,
    };
    let channel = open(&stand.server(Side::External), &login)
        .await
        .expect("runner не вошёл на канал управления");
    report_host(&channel.mux).await;
    channel
}

/// Worker с новым сертификатом входит на канал управления.
pub(super) async fn worker_channel(stand: &Stand, name: &str) -> Channel {
    let (key, certificate) = stand.worker(name);
    let login = Login::Certificate {
        key: &key,
        certificate: &certificate,
    };
    open(&stand.server(Side::External), &login)
        .await
        .expect("worker не вошёл на канал управления")
}

/// Session входит на внутренний слушатель.
pub(super) async fn session_channel(stand: &Stand, session: &Fingerprint, secret: &str) -> Channel {
    open(&stand.server(Side::Internal), &Login::Session { session, secret })
        .await
        .expect("session не вошла на внутренний слушатель")
}

fn encode<T: Serialize>(message: &T) -> Bytes {
    protocol::encode(message).unwrap().into()
}

/// Запрос с одним ответом.
pub(super) async fn ask_once<Req: Serialize, Resp: DeserializeOwned>(channel: &Channel, request: &Req) -> Resp {
    tokio::time::timeout(WAIT, ask(&channel.mux, request))
        .await
        .expect("server не ответил")
        .expect("ответ не получен")
}

/// Ждёт запрос server узлу.
pub(super) async fn next_request<T: DeserializeOwned>(channel: &mut Channel) -> (T, Incoming) {
    let incoming = tokio::time::timeout(WAIT, channel.requests.recv())
        .await
        .expect("server не прислал запрос")
        .expect("канал закрыт");
    let request = protocol::decode_body(incoming.body()).expect("запрос server не разобран");
    (request, incoming)
}

/// Отвечает на запрос server.
pub(super) async fn respond<T: Serialize>(incoming: &Incoming, response: &T) {
    incoming.respond(encode(response), true).await.unwrap();
}

pub(super) async fn assert_closed(mux: &Mux, why: &str) {
    assert!(
        tokio::time::timeout(WAIT, mux.closed()).await.is_ok(),
        "канал управления не закрыт: {why}"
    );
}

#[track_caller]
pub(super) fn assert_open(mux: &Mux, why: &str) {
    assert!(!mux.is_closed(), "канал управления закрыт: {why}");
}

/// Шлёт сообщение, которого нет в наборе роли: server закрывает соединение, не отвечая.
pub(super) async fn assert_foreign_message_closes<T: Serialize>(channel: &Channel, message: &T) {
    let answer = tokio::time::timeout(WAIT, channel.mux.request(encode(message))).await;
    assert!(
        matches!(answer, Ok(Err(_))),
        "server ответил на сообщение не из набора роли: {answer:?}"
    );
    assert_closed(&channel.mux, "сообщение не из набора роли").await;
}

/// Канал управления без SSH: `Control` на другом конце `duplex`.
pub(super) struct Direct {
    pub(super) control: Arc<Control>,
    pub(super) env: Env,
}

impl Direct {
    pub(super) fn new() -> Self {
        let env = Env::new();
        let control = Arc::new(Control::new(
            Arc::clone(&env.registry),
            Arc::clone(&env.ca),
            None,
            protocol::Author {
                name: "Iapetus".to_owned(),
                email: "iapetus@localhost".to_owned(),
            },
            env.dir.path().join("docs"),
        ));
        Self { control, env }
    }

    pub(super) fn connect(&self, peer: Peer) -> (Mux, Requests) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let control = Arc::clone(&self.control);
        tokio::spawn(async move { control.serve(peer, server).await });
        Mux::new(client)
    }
}

/// Ответ `Err(Error)` на любой запрос: ошибки не зависят от типа успешного ответа.
pub(super) fn is_error<T>(response: &Result<T, Error>, expected: &Error) -> bool {
    response.as_ref().err() == Some(expected)
}
