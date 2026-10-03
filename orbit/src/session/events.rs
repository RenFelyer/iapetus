//! События шлюза: журнал и эпоха FUSE.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::broadcast;

use super::fs;
use super::gateway::{GatewayEvent, Observation, RequestKind};

/// Меняет эпоху FUSE на конце хода: пока агент думал, файлы на runner могли измениться не по его команде.
pub(super) async fn advance_epoch_at_end_of_turn(mut events: broadcast::Receiver<Observation>, epoch: fs::Epoch) {
    loop {
        match events.recv().await {
            Ok(Observation {
                request: RequestKind::Main,
                event: GatewayEvent::EndTurn,
            }) => epoch.advance(),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                // Среди пропущенных мог быть конец хода: лишняя смена эпохи стоит одной сверки хеша, пропущенная — устаревшего файла.
                tracing::warn!(missed, "события шлюза пропущены, эпоха FUSE меняется на всякий случай");
                epoch.advance();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Инструменты, которые читают файл по `path`: его скачивают заранее. Запись нового файла предзагрузки не просит.
const READING_TOOLS: [&str; 7] = [
    "Read",
    "Edit",
    "view_file",
    "replace_file_content",
    "read_file",
    "replace",
    "view_image",
];

/// Файл, который вызов инструмента сейчас прочитает.
fn file_to_preload(event: &GatewayEvent) -> Option<&Path> {
    match event {
        GatewayEvent::ToolUse {
            name, path: Some(path), ..
        } if READING_TOOLS.contains(&name.as_str()) => Some(Path::new(path)),
        _ => None,
    }
}

/// Скачивает файл на `tool_use`, пока агент ещё не открыл его. Это подсказка: ошибки только в журнал.
pub(super) async fn preload_files(mut events: broadcast::Receiver<Observation>, vfses: Arc<[Arc<fs::Vfs>]>) {
    loop {
        match events.recv().await {
            Ok(Observation { event, .. }) => {
                if let Some(path) = file_to_preload(&event) {
                    let (vfses, path) = (Arc::clone(&vfses), path.to_owned());
                    tokio::spawn(async move { fs::preload(&vfses, &path).await });
                }
            }
            // Пропущенные вызовы агент прочтёт обычным путём.
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Пишет события шлюза в журнал; вход инструмента не пишется: в нём содержимое файлов.
pub(super) async fn log_events(mut events: broadcast::Receiver<Observation>) {
    loop {
        match events.recv().await {
            Ok(Observation {
                request,
                event: GatewayEvent::ToolUse { name, path, .. },
            }) => {
                tracing::info!(%name, ?path, ?request, "событие шлюза: tool_use");
            }
            Ok(Observation { request, event }) => tracing::info!(?event, ?request, "событие шлюза"),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "журнал событий шлюза отстал");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/session/events.rs"]
mod tests;
