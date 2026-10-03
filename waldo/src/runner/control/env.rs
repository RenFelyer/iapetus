//! Слой окружения от server: подписка `Env` по каналу управления. Server присылает снимок положенных runner слоёв
//! (`all.VAR`, `<родитель>.VAR`, `<свой fingerprint>.VAR`), затем изменения до закрытия соединения.
//!
//! Снимок свеж, пока жива подписка: после обрыва он сбрасывается, и до нового снимка команда ждёт, а затем не
//! запускается. Команда без переменных server или со старыми значениями молча ушла бы не туда — в другой аккаунт, с
//! отозванным ключом, — а отказ с причиной виден сразу.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::bail;
use nerve::connect::server::subscribe;
use nerve::fingerprint::Fingerprint;
use nerve::mux::Mux;
use nerve::protocol::{EnvChange, EnvEvent, RunnerRequest};
use tokio::sync::watch;

/// Ключ (`all.VAR`, `<fingerprint>.VAR`) → значение.
type Snapshot = HashMap<String, String>;

/// Префикс ключей, общих для всех runner.
const SHARED_PREFIX: &str = "all";

#[derive(Debug)]
pub(in crate::runner) struct ServerEnv {
    /// `None` — свежего снимка нет.
    snapshot: watch::Sender<Option<Snapshot>>,
    fingerprint: Fingerprint,
    /// У клона — runner, от которого он создан: слой родителя слабее своего.
    parent: Option<Fingerprint>,
}

impl ServerEnv {
    pub(in crate::runner) fn new(fingerprint: Fingerprint, parent: Option<Fingerprint>) -> Self {
        Self {
            snapshot: watch::channel(None).0,
            fingerprint,
            parent,
        }
    }

    /// Подписывается на `Env` по каналу `mux` и ведёт снимок до закрытия подписки; потом сбрасывает его.
    pub(in crate::runner) async fn follow(&self, mux: &Mux) -> anyhow::Result<()> {
        let result = self.follow_events(mux).await;
        self.snapshot.send_replace(None);
        result
    }

    async fn follow_events(&self, mux: &Mux) -> anyhow::Result<()> {
        let mut events = subscribe::<_, EnvEvent>(mux, &RunnerRequest::Env).await?;
        while let Some(event) = events.next().await? {
            match event {
                EnvEvent::Snapshot(entries) => {
                    let snapshot = entries.into_iter().map(|entry| (entry.key, entry.value)).collect();
                    self.snapshot.send_replace(Some(snapshot));
                    tracing::info!("снимок окружения server получен");
                }
                EnvEvent::Change(change) => self.snapshot.send_modify(|snapshot| {
                    let Some(snapshot) = snapshot else { return };
                    match change {
                        EnvChange::Set { key, value } => snapshot.insert(key, value),
                        EnvChange::Delete { key } => snapshot.remove(&key),
                    };
                }),
            }
        }
        bail!("server закрыл подписку на окружение")
    }

    /// Переменные для runner: `all.`, затем слой родителя, затем свой; поздний слой сильнее. Ждёт свежего снимка не
    /// дольше `wait`.
    pub(in crate::runner) async fn vars(&self, wait: Duration) -> Result<BTreeMap<String, String>, String> {
        let mut snapshot = self.snapshot.subscribe();
        let fresh = tokio::time::timeout(wait, snapshot.wait_for(Option::is_some)).await;
        let Ok(Ok(snapshot)) = fresh else {
            return Err("переменные server недоступны".to_owned());
        };
        let Some(snapshot) = snapshot.as_ref() else {
            return Err("переменные server недоступны".to_owned());
        };
        let parent = self.parent.as_ref().map(Fingerprint::as_str);
        let mut vars = BTreeMap::new();
        for prefix in [Some(SHARED_PREFIX), parent, Some(self.fingerprint.as_str())].into_iter().flatten() {
            for (key, value) in snapshot {
                if let Some(name) = key.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('.')) {
                    vars.insert(name.to_owned(), value.clone());
                }
            }
        }
        Ok(vars)
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/control/env.rs"]
mod tests;
