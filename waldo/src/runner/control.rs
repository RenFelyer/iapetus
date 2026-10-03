//! Управление runner по каналу управления server: начало и конец сессии, запросы на клон и подписка на окружение
//! (`env`). Команд по этому каналу runner не принимает: команды сессии, как и её файлы, идут по каналу данных.

pub(super) mod env;
pub(super) mod host;

use std::sync::Arc;

use bytes::Bytes;
use nerve::connect::server::{Channel, ask};
use nerve::mux::{Incoming, Requests};
use nerve::protocol::{self, Error, RunnerHost, RunnerRequest, SessionMessage, ToRunner};
use serde::Serialize;

use super::clone::{self, Lifetime, Parent};
use super::data::Data;
use super::exec::{Executor, session_vars};
use env::ServerEnv;

#[cfg(test)]
#[path = "../../tests/runner/control.rs"]
mod tests;

/// Обслуживает открытый канал управления, пока он жив: принимает запросы server и держит подписку на окружение.
/// Запрос не из набора runner закрывает канал. `parent` — у runner, который сам клон, его нет: клонов он не создаёт.
pub(super) async fn serve(
    channel: Channel,
    env: &ServerEnv,
    executor: &Arc<Executor>,
    data: &Data,
    lifetime: Option<&Lifetime>,
    parent: Option<&Parent>,
    host: &RunnerHost,
) {
    tracing::info!("канал управления открыт");
    let Channel { mux, mut requests, .. } = channel;
    // Server считает runner в сети только после этого запроса: назначенная session получит окружение runner сразу.
    match ask::<_, Result<(), Error>>(&mux, &RunnerRequest::Host(host.clone())).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return tracing::warn!(%error, "server не принял окружение runner"),
        Err(error) => return tracing::warn!(error = format!("{error:#}"), "окружение runner не отправлено server"),
    }
    tokio::select! {
        followed = env.follow(&mux) => if let Err(error) = followed {
            tracing::warn!(error = format!("{error:#}"), "подписка на окружение прервалась");
        },
        () = data.mcp().follow(&mux) => {}
        () = answer_requests(&mut requests, executor, data, lifetime, parent) => {}
    }
}

async fn answer_requests(
    requests: &mut Requests,
    executor: &Arc<Executor>,
    data: &Data,
    lifetime: Option<&Lifetime>,
    parent: Option<&Parent>,
) {
    while let Some(incoming) = requests.recv().await {
        let request: ToRunner = match protocol::decode_body(incoming.body()) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "запрос не из набора runner, канал управления закрывается");
                return;
            }
        };
        match request {
            ToRunner::Session(session) => {
                apply_session(session, executor, data, lifetime).await;
                respond(&incoming, &Ok::<(), Error>(())).await;
            }
            ToRunner::Clone(request) => {
                let response = match parent {
                    Some(parent) => clone::start_clone(parent, &request.dir).await,
                    None => Err(Error::Io("клон не создаёт клонов: вернитесь к родителю".to_owned())),
                };
                respond(&incoming, &response).await;
            }
        }
    }
}

/// Начало и конец назначения. После каждого подключения server повторяет состояние: `Start` идущего назначения ничего не
/// меняет, иначе канал данных переоткрывался бы при каждом обрыве канала управления. `Start` другого назначения сначала
/// останавливает прежнее, `End` другого назначения игнорируется. Возвращается, когда остановка закончена.
async fn apply_session(session: SessionMessage, executor: &Arc<Executor>, data: &Data, lifetime: Option<&Lifetime>) {
    match session {
        SessionMessage::Start {
            assignment,
            author,
            github_token,
        } => {
            let current = data.assignment();
            if current == Some(assignment) {
                return;
            }
            if current.is_some() {
                stop(executor, data, lifetime).await;
            }
            executor.start(assignment, session_vars(author.clone(), github_token));
            data.start(assignment);
            data.mcp().start(assignment, executor);
            if let Some(lifetime) = lifetime {
                lifetime.start(&author);
            }
        }
        SessionMessage::End { assignment } => {
            if data.assignment() == Some(assignment) {
                stop(executor, data, lifetime).await;
            }
        }
    }
}

/// Останавливает назначение: канал данных закрыт и файловые операции дождались, затем команды и их группы процессов
/// убиты, переменные сессии сброшены, наблюдение снято.
async fn stop(executor: &Executor, data: &Data, lifetime: Option<&Lifetime>) {
    data.stop().await;
    data.mcp().stop().await;
    executor.stop().await;
    if let Some(lifetime) = lifetime {
        lifetime.end();
    }
}

async fn respond<T: Serialize>(incoming: &Incoming, response: &T) {
    // Ответ состоит из чисел, строк и байтов: кодирование не падает.
    let Ok(body) = protocol::encode(response) else {
        tracing::error!("ответ не закодирован");
        return;
    };
    if let Err(error) = incoming.respond(Bytes::from(body), true).await {
        tracing::warn!(%error, "ответ не отправлен");
    }
}
