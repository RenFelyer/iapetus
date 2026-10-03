//! Маршруты jump по реестру: куда вести узел, вошедший по сертификату.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use nerve::fingerprint::Fingerprint;

use super::jump::Visitor;
use crate::server::registry::Registry;

/// Маршруты для [`super::jump::Jump`]: назначение выбирает реестр — runner ведётся в назначенную ему сессию,
/// worker в названную им, если она связана с runner и агент в ней запущен (`agent_running`); адрес сессии —
/// `session_addr`. Ошибка реестра и сессия без адреса дают отказ.
pub(crate) fn registry_routes(
    registry: Arc<Mutex<Registry>>,
    session_addr: impl Fn(&Fingerprint) -> Option<SocketAddr> + Send + Sync + 'static,
    agent_running: impl Fn(&Fingerprint) -> bool + Send + Sync + 'static,
) -> impl Fn(&Visitor, &str) -> Option<SocketAddr> + Send + Sync + 'static {
    move |visitor, requested| {
        let session = {
            let Ok(registry) = registry.lock() else {
                tracing::error!("jump: реестр недоступен, мьютекс отравлен");
                return None;
            };
            let destination = match visitor {
                Visitor::Runner { fingerprint, .. } => registry.runner_destination(fingerprint),
                Visitor::Worker { .. } => registry.worker_destination(requested),
            };
            match destination {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(?visitor, requested, %error, "jump: отказано в назначении");
                    return None;
                }
            }
        };
        if matches!(visitor, Visitor::Worker { .. }) && !agent_running(&session) {
            tracing::warn!(?visitor, %session, "jump: агент в сессии не запущен");
            return None;
        }
        let addr = session_addr(&session);
        if addr.is_none() {
            tracing::warn!(?visitor, %session, "jump: у назначенной сессии нет адреса");
        }
        addr
    }
}
