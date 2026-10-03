//! Соединение `iapetus-enroll`: узел без действующего сертификата входит по `orbit_key`, шлёт один запрос входа,
//! получает ответ, и соединение закрывается.

use std::time::Duration;

use nerve::mux::{Mux, Requests};
use nerve::protocol::EnrollRequest;

use super::{Control, decode, reply, runner, worker};

/// Сколько вход ждёт запрос и, после ответа, закрытия соединения клиентом.
pub(in crate::server) const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

pub(super) async fn serve(control: &Control, _mux: &Mux, mut requests: Requests) {
    let Ok(Some(incoming)) = tokio::time::timeout(REQUEST_DEADLINE, requests.recv()).await else {
        tracing::info!("вход без запроса закрыт");
        return;
    };
    let Some(request) = decode::<EnrollRequest>(&incoming, "iapetus-enroll") else {
        return;
    };
    let answered = match request {
        EnrollRequest::Register(request) => reply(&incoming, &runner::register(control, &request)).await,
        EnrollRequest::Renew(request) => reply(&incoming, &runner::renew(control, &request)).await,
        EnrollRequest::WorkerCertificate(request) => reply(&incoming, &worker::certificate(control, &request)).await,
    };
    if !answered {
        return;
    }
    // Закрытие сразу оборвало бы запись ответа: клиент закрывает соединение сам, получив его.
    if let Ok(Some(_)) = tokio::time::timeout(REQUEST_DEADLINE, requests.recv()).await {
        tracing::warn!("вход прислал второй запрос, соединение закрыто");
    }
}
