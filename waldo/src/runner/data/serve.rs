//! Обслуживание запросов сессии, пришедших по каналу данных: файловые операции, команды и их отмена.

use std::sync::Arc;

use bytes::Bytes;
use nerve::mux::{Incoming, Requests};
use nerve::protocol::{self, AttachRequest, CancelRequest, DataRequest, Error, ExecEvent, ExecRequest, FsRequest, Root};
use serde::Serialize;
use tokio::sync::Semaphore;

use super::jobs::Jobs;
use crate::runner::exec::Executor;
use crate::runner::fs::Workdir;
use crate::runner::mcp::Mcp;

#[cfg(test)]
#[path = "../../../tests/runner/data/serve.rs"]
mod tests;

/// Сколько файловых запросов выполняется одновременно: каждый держит поток блокирующего пула.
pub(super) const MAX_IN_FLIGHT: u16 = 64;

/// Отвечает на запросы, пока канал не закроется. Выполняющийся файловый запрос держит одно из `MAX_IN_FLIGHT` разрешений
/// `permits` до конца: кто забрал все, тот дождался всех запросов.
pub(super) async fn serve(
    mut requests: Requests,
    workdir: Arc<Workdir>,
    home: Option<Arc<Workdir>>,
    executor: Arc<Executor>,
    jobs: Arc<Jobs>,
    permits: Arc<Semaphore>,
    mcp: Arc<Mcp>,
) {
    while let Some(incoming) = requests.recv().await {
        let request = match protocol::decode_body::<DataRequest>(incoming.body()) {
            Ok(request) => request,
            Err(error) => {
                // Какого вида ждали ответ, неизвестно: `Err` одинаково кодируется в ответах на файловый запрос и
                // отмену, а команда прочтёт такой ответ как неразобранный и сообщит об этом сама.
                tracing::warn!(%error, "запрос сессии не разобран");
                let response: Result<(), Error> = Err(Error::Io("тело запроса не разобрано".to_owned()));
                reply(&incoming, &response).await;
                continue;
            }
        };
        match request {
            DataRequest::Fs { root, request } => {
                let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                    return;
                };
                let target = match root {
                    Root::Project => Some(Arc::clone(&workdir)),
                    Root::Home => home.clone(),
                };
                tokio::spawn(async move {
                    fs(&incoming, request, target).await;
                    drop(permit);
                });
            }
            DataRequest::Exec(request) => {
                let (executor, jobs) = (Arc::clone(&executor), Arc::clone(&jobs));
                tokio::spawn(async move { exec(&incoming, request, &executor, &jobs).await });
            }
            DataRequest::Attach(request) => {
                let jobs = Arc::clone(&jobs);
                tokio::spawn(async move { attach(&incoming, request, &jobs).await });
            }
            DataRequest::Cancel(CancelRequest { id }) => reply(&incoming, &cancel(id, &executor, &jobs)).await,
            DataRequest::Mcp(call) => {
                let mcp = Arc::clone(&mcp);
                tokio::spawn(async move { reply(&incoming, &mcp.call(call).await).await });
            }
        }
    }
}

async fn reply<T: Serialize>(incoming: &Incoming, response: &T) {
    // Ответ состоит из чисел, строк и байтов, поэтому кодирование не падает; пустое тело сессия прочтёт как ошибку.
    let body = protocol::encode(response).unwrap_or_else(|e| {
        tracing::error!(error = %e, "ответ не закодирован");
        Vec::new()
    });
    if let Err(e) = incoming.respond(Bytes::from(body), true).await {
        tracing::warn!(error = %e, "ответ не отправлен");
    }
}

/// Останавливает команду, которую запустила текущая сессия. Выполняющаяся команда прежней сессии, ещё не успевшая
/// остановиться, чужая: её id мог достаться новой команде, и отмена не должна попасть в старую.
fn cancel(id: u64, executor: &Executor, jobs: &Jobs) -> Result<(), Error> {
    if jobs.get(id).is_none() {
        return Err(Error::NotFound);
    }
    executor.cancel(id)
}

/// `target` — каталог запроса; `None` — `$HOME` не отдаётся, и такой путь — как путь вне рабочего каталога.
async fn fs(incoming: &Incoming, request: FsRequest, target: Option<Arc<Workdir>>) {
    let Some(workdir) = target else {
        reply(incoming, &Err::<(), _>(Error::OutsideWorkdir)).await;
        return;
    };
    let response = match tokio::task::spawn_blocking(move || workdir.handle(request)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!(error = %e, "обработчик файловой операции упал");
            Err(Error::Io("внутренняя ошибка runner".to_owned()))
        }
    };
    reply(incoming, &response).await;
}

/// Запускает команду и отдаёт её поток с начала. Повтор запроса с известным id команду не запускает, а отдаёт поток
/// уже запущенной. Оборвётся канал — команда останется в журнале, и сессия заберёт остаток.
async fn exec(incoming: &Incoming, request: ExecRequest, executor: &Arc<Executor>, jobs: &Jobs) {
    jobs.start(request, executor).stream(0, incoming).await;
}

/// Отдаёт остаток потока команды, начиная с первого сообщения, которого у сессии нет.
async fn attach(incoming: &Incoming, request: AttachRequest, jobs: &Jobs) {
    match jobs.get(request.id) {
        Some(job) => job.stream(request.received, incoming).await,
        None => reply(incoming, &ExecEvent::Err(Error::NotFound)).await,
    }
}
