//! Окружение команд: запросы worker `Env.*` и подписка runner `Env`.
//!
//! Server хранит окружение в реестре (значения зашифрованы). Каждая запись рассылается подписанным runner, но
//! подписчик получает только изменения слоёв, которые ему положены: `all`, родителя (у клона) и свой.

use std::collections::BTreeSet;
use std::sync::Mutex;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{EnvChange, EnvChangeResponse, EnvEntry, Error};
use tokio::sync::broadcast;

use super::lock;
use crate::server::registry::{self, Registry};

/// Изменений, которые подписчик может не успеть прочитать; отставший теряет подписку и подписывается заново.
const BACKLOG: usize = 256;

#[derive(Debug)]
pub(in crate::server) struct Env {
    changes: broadcast::Sender<EnvChange>,
}

/// Подписка runner: снимок положенных слоёв и изменения после него.
#[derive(Debug)]
pub(in crate::server) struct EnvSubscription {
    pub(in crate::server) snapshot: Vec<EnvEntry>,
    scopes: Vec<String>,
    changes: broadcast::Receiver<EnvChange>,
}

impl Env {
    pub(in crate::server) fn new() -> Self {
        Self {
            changes: broadcast::channel(BACKLOG).0,
        }
    }

    /// Записывает или удаляет ключ и рассылает изменение. Рассылка идёт под замком реестра: подписка, которая
    /// читает снимок под тем же замком, не теряет изменение и не получает его дважды.
    pub(in crate::server) fn change(&self, registry: &Mutex<Registry>, change: &EnvChange) -> EnvChangeResponse {
        checked_key(change)?;
        let mut registry = lock(registry);
        let written = match change {
            EnvChange::Set { key, value } => registry.env_set(key, value),
            EnvChange::Delete { key } => registry.env_delete(key),
        };
        written.map_err(|e| internal(&e))?;
        // Нет подписчиков — не ошибка: send сообщает только об этом.
        if self.changes.send(change.clone()).is_err() {
            tracing::debug!("изменение окружения без подписчиков");
        }
        Ok(())
    }

    /// Все записи для `Env.List` worker.
    pub(in crate::server) fn list(registry: &Mutex<Registry>) -> Result<Vec<EnvEntry>, Error> {
        let entries = lock(registry).env_all().map_err(|e| internal(&e))?;
        Ok(entries.into_iter().map(|(key, value)| EnvEntry { key, value }).collect())
    }

    /// Имена переменных, которые runner даёт командам: слои `all`, родителя и свой, без значений.
    pub(in crate::server) fn names(registry: &Mutex<Registry>, runner: &Fingerprint) -> Result<Vec<String>, Error> {
        let registry = lock(registry);
        let scopes = scopes_of(&registry, runner)?;
        let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let entries = registry.env_in_scopes(&scope_refs).map_err(|e| internal(&e))?;
        let names: BTreeSet<String> = entries
            .into_iter()
            .filter_map(|(key, _)| key.split_once('.').map(|(_, name)| name.to_owned()))
            .collect();
        Ok(names.into_iter().collect())
    }

    /// Подписка runner: слои `all`, родителя и свой.
    pub(in crate::server) fn subscribe(&self, registry: &Mutex<Registry>, runner: &Fingerprint) -> Result<EnvSubscription, Error> {
        let registry = lock(registry);
        let changes = self.changes.subscribe();
        let scopes = scopes_of(&registry, runner)?;
        let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let snapshot = registry
            .env_in_scopes(&scope_refs)
            .map_err(|e| internal(&e))?
            .into_iter()
            .map(|(key, value)| EnvEntry { key, value })
            .collect();
        Ok(EnvSubscription { snapshot, scopes, changes })
    }
}

/// Слои окружения runner: `all`, он сам и родитель (у клона).
fn scopes_of(registry: &Registry, runner: &Fingerprint) -> Result<Vec<String>, Error> {
    let mut scopes = vec!["all".to_owned(), runner.to_string()];
    scopes.extend(
        registry
            .node_parent(runner)
            .map_err(|e| internal(&e))?
            .map(|parent| parent.to_string()),
    );
    Ok(scopes)
}

impl EnvSubscription {
    /// Следующее изменение своих слоёв; `None` — подписку надо закрыть (рассылка закрыта или подписчик отстал).
    pub(in crate::server) async fn next(&mut self) -> Option<EnvChange> {
        loop {
            match self.changes.recv().await {
                Ok(change) if self.owns(&change) => return Some(change),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    tracing::warn!("подписчик Env отстал от рассылки, соединение закрывается");
                    return None;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    fn owns(&self, change: &EnvChange) -> bool {
        let (EnvChange::Set { key, .. } | EnvChange::Delete { key }) = change;
        key.split_once('.')
            .is_some_and(|(scope, _)| self.scopes.iter().any(|own| own == scope))
    }
}

fn internal(error: &registry::Error) -> Error {
    tracing::error!(%error, "окружение не записано или не прочитано");
    Error::Io("внутренняя ошибка server".to_owned())
}

/// Возвращает ключ изменения, если он вида `all.VAR` или `<fingerprint>.VAR`.
pub(in crate::server) fn checked_key(change: &EnvChange) -> Result<&str, Error> {
    let (EnvChange::Set { key, .. } | EnvChange::Delete { key }) = change;
    if key_is_valid(key) {
        return Ok(key);
    }
    Err(Error::Io(format!("ключ окружения {key:?} не вида all.VAR или <fingerprint>.VAR")))
}

/// Ключ окружения: `all.VAR` или `<fingerprint>.VAR`, где `VAR` — имя переменной окружения.
pub(super) fn key_is_valid(key: &str) -> bool {
    let Some((scope, name)) = key.split_once('.') else {
        return false;
    };
    let scope_ok = scope == "all" || scope.parse::<Fingerprint>().is_ok();
    let mut chars = name.chars();
    let name_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    scope_ok && name_ok
}

#[cfg(test)]
#[path = "../../../tests/server/control/env.rs"]
mod tests;
