//! Изменения на runner: события канала данных.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::vfs::Vfs;
use crate::session::data::Event;

/// Применяет события канала данных к ядрам ФС, пока источник событий не закроется: пачки изменений обновляют кэш
/// метаданных (каждое ядро берёт пачки своего каталога runner), а открытие и потеря канала делают его недействительным.
pub(crate) async fn follow(mut events: mpsc::UnboundedReceiver<Event>, vfses: Vec<Arc<Vfs>>) {
    while let Some(event) = events.recv().await {
        for vfs in &vfses {
            match &event {
                Event::Connected | Event::Disconnected => vfs.link_changed(),
                Event::Watch(batch) => vfs.apply_watch(batch),
            }
        }
    }
}
