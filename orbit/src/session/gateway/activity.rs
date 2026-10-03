//! Чем занят агент по запросам, которые сейчас идут через шлюз.
//!
//! Занятость считается по жизни запроса, а не по событиям: счётчик растёт, когда запрос
//! принят, и убывает, когда охранник запроса сброшен. Сбрасывается он при любом конце —
//! ответ дочитан, клиент или upstream оборвал поток, upstream недоступен, — поэтому
//! занятость не залипает, даже если часть событий шлюза потерялась.

use nerve::protocol::Activity;
use tokio::sync::watch;

use super::RequestKind;

/// Что считает запрос в занятости.
#[derive(Clone, Copy)]
enum Counted {
    Subagent,
    Compaction,
}

/// Запрос учтён в занятости, пока охранник жив.
pub(super) struct Guard {
    tracker: watch::Sender<Activity>,
    counted: Counted,
}

/// Считает запросы подагентов и сжатий; подписчики видят каждое изменение.
#[derive(Debug, Clone)]
pub(crate) struct Tracker(watch::Sender<Activity>);

impl Default for Tracker {
    fn default() -> Self {
        Self(watch::channel(Activity::default()).0)
    }
}

impl Tracker {
    pub(crate) fn subscribe(&self) -> watch::Receiver<Activity> {
        self.0.subscribe()
    }

    /// Учитывает запрос; служебные запросы, не меняющие занятость, не учитываются.
    pub(super) fn begin(&self, kind: &RequestKind) -> Option<Guard> {
        let counted = match kind {
            RequestKind::Subagent { .. } => Counted::Subagent,
            RequestKind::Compaction => Counted::Compaction,
            RequestKind::Main | RequestKind::Title | RequestKind::Other => return None,
        };
        self.0.send_modify(|activity| *counter(activity, counted) += 1);
        Some(Guard {
            tracker: self.0.clone(),
            counted,
        })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.tracker
            .send_modify(|activity| *counter(activity, self.counted) = counter(activity, self.counted).saturating_sub(1));
    }
}

fn counter(activity: &mut Activity, counted: Counted) -> &mut u32 {
    match counted {
        Counted::Subagent => &mut activity.subagents,
        Counted::Compaction => &mut activity.compactions,
    }
}
