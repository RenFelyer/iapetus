//! Соединение `iapetus-enroll`: только запросы входа, ответ и закрытие.

use nerve::connect::server::enroll as enroll_request;
use nerve::protocol::{EnrollRequest, NodesOp, RegisterResponse, WorkerRequest};

use super::super::enroll::REQUEST_DEADLINE;
use super::*;
use crate::server::testing::{random_fingerprint, random_key, signed_renewal, unix};

#[tokio::test(start_paused = true)]
async fn enroll_connection_without_a_request_is_closed_after_the_deadline() {
    let direct = Direct::new();
    let (mux, _requests) = direct.connect(Peer::Enroll);

    tokio::time::sleep(REQUEST_DEADLINE.saturating_sub(Duration::from_secs(1))).await;
    assert_open(&mux, "вход закрыт раньше срока");
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_closed(&mux, "вход без запроса за 10 с").await;
}

#[tokio::test]
async fn enroll_connection_closes_on_a_message_that_is_not_a_login_request() {
    let stand = Stand::start().await;
    let key = stand.env.orbit_key.expose().to_owned();
    let channel = open(&stand.server(Side::External), &Login::Enroll { orbit_key: &key })
        .await
        .unwrap();

    assert_foreign_message_closes(&channel, &WorkerRequest::Nodes(NodesOp::NewOrbitKey)).await;
}

#[tokio::test]
async fn enroll_connection_registers_a_runner_and_answers_before_closing() {
    let stand = Stand::start().await;
    let (fingerprint, key) = (random_fingerprint(), random_key());
    let request = stand.env.request("r", &fingerprint, &key);
    let orbit_key = stand.env.orbit_key.expose().to_owned();

    let response: RegisterResponse = enroll_request(&stand.server(Side::External), &orbit_key, &EnrollRequest::Register(request))
        .await
        .expect("запрос входа не дошёл");

    let reply = response.expect("регистрация отклонена");
    assert!(
        reply.certificate.starts_with("ssh-ed25519-cert"),
        "в ответе не сертификат: {}",
        reply.certificate
    );
}

#[tokio::test]
async fn enroll_connection_renews_a_runner_certificate() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let request = signed_renewal(
        &node.key,
        &node.fingerprint,
        unix(std::time::SystemTime::now()),
        &stand.env.orbit_key,
    );
    let orbit_key = stand.env.orbit_key.expose().to_owned();

    let response: protocol::RenewResponse = enroll_request(&stand.server(Side::External), &orbit_key, &EnrollRequest::Renew(request))
        .await
        .expect("запрос входа не дошёл");

    assert!(response.is_ok(), "продление по orbit_key отклонено: {response:?}");
}

#[tokio::test]
async fn enroll_connection_refuses_a_revoked_runner_with_access_error() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand.env.reg().revoke_node(&node.fingerprint).unwrap();
    let request = signed_renewal(
        &node.key,
        &node.fingerprint,
        unix(std::time::SystemTime::now()),
        &stand.env.orbit_key,
    );
    let orbit_key = stand.env.orbit_key.expose().to_owned();

    let response: protocol::RenewResponse = enroll_request(&stand.server(Side::External), &orbit_key, &EnrollRequest::Renew(request))
        .await
        .expect("запрос входа не дошёл");

    assert!(
        is_error(&response, &Error::Access),
        "отозванный runner получил не Access: {response:?}"
    );
}

#[tokio::test]
async fn enroll_connection_refuses_renewal_with_a_replaced_orbit_key_with_access_error() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let request = signed_renewal(
        &node.key,
        &node.fingerprint,
        unix(std::time::SystemTime::now()),
        &stand.env.orbit_key,
    );
    let old = stand.env.orbit_key.expose().to_owned();
    let current = stand.new_orbit_key();

    // Вход на `iapetus-enroll` по новому ключу, но запрос продления подписан со старым `orbit_key`.
    let response: protocol::RenewResponse = enroll_request(&stand.server(Side::External), &current, &EnrollRequest::Renew(request))
        .await
        .expect("запрос входа не дошёл");

    assert_ne!(old, current, "orbit_key не заменён");
    assert!(
        is_error(&response, &Error::Access),
        "продление со старым orbit_key дало не Access: {response:?}"
    );
}
