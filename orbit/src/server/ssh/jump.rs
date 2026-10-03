//! Jump: вошедший по сертификату с логином `iapetus-jump` открывает `direct-tcpip`, и server ведёт его туда, куда
//! решает [`Routes`]. Сертификат даёт только право войти; адрес из запроса сам по себе ничего не открывает.
//!
//! Сертификат worker и runner действует, пока не выдан новый `orbit_key`: выданный раньше нового ключа не пускает,
//! а туннели, открытые по нему, закрываются в момент выдачи. Туннели runner закрываются ещё при его отзыве и когда
//! он теряет назначение.

use std::net::SocketAddr;
use std::sync::Arc;

use nerve::fingerprint::Fingerprint;
use russh::server::{ChannelOpenHandle, Msg};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;

use crate::server::control::{Control, superseded};

/// Кто вошёл в jump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) enum Visitor {
    /// runner: fingerprint из `key_id` его сертификата; `issued` — когда выдан сертификат, секунды Unix.
    Runner { fingerprint: Fingerprint, issued: u64 },
    /// waldo worker: личности у него нет; `issued` — когда выдан его сертификат, секунды Unix.
    Worker { issued: u64 },
}

/// Куда вести вошедшего: он сам и хост из запроса `direct-tcpip` -> внутренний адрес сессии. Порт из запроса не
/// учитывается. `None` — отказ. Решение принимается при каждом открытии туннеля, поэтому отозванный узел теряет
/// доступ сразу.
pub(in crate::server) type Routes = dyn Fn(&Visitor, &str) -> Option<SocketAddr> + Send + Sync;

pub(in crate::server) struct Jump {
    routes: Arc<Routes>,
    control: Arc<Control>,
}

impl std::fmt::Debug for Jump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jump").finish_non_exhaustive()
    }
}

impl Jump {
    pub(in crate::server) fn new(
        control: Arc<Control>,
        routes: impl Fn(&Visitor, &str) -> Option<SocketAddr> + Send + Sync + 'static,
    ) -> Self {
        Self {
            routes: Arc::new(routes),
            control,
        }
    }

    /// Открывает туннель вошедшему и ведёт его, пока тот не закроет канал или не потеряет доступ. `false` — отказ.
    pub(in crate::server) async fn open(
        &self,
        visitor: &Visitor,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        reply: ChannelOpenHandle,
    ) -> bool {
        // Подписка до решения по реестру: отзыв между ними иначе не закрыл бы уже открытый туннель.
        let lost = lost_access(visitor.clone(), &self.control);
        let Some(addr) = (self.routes)(visitor, host) else {
            tracing::warn!(?visitor, host, port, "jump: direct-tcpip без маршрута");
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return false;
        };
        let mut upstream = match TcpStream::connect(addr).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(host, port, %addr, %error, "jump: сессия недоступна");
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return false;
            }
        };
        reply.accept().await;
        tokio::spawn(async move {
            let mut client = channel.into_stream();
            tokio::select! {
                copied = copy_bidirectional(&mut client, &mut upstream) => {
                    if let Err(error) = copied {
                        tracing::debug!(%error, "jump: поток прерван");
                    }
                }
                () = lost => tracing::info!("jump: туннель закрыт — доступ потерян"),
            }
        });
        true
    }
}

/// Завершается, когда вошедший потерял доступ: выдан новый `orbit_key` или (runner) он отозван либо потерял
/// назначение. Получатели отзывов и ключа берутся сразу, при вызове, а не при первом опросе.
fn lost_access(visitor: Visitor, control: &Control) -> impl Future<Output = ()> + use<> {
    let mut revoked = control.revocations();
    let mut released = control.releases();
    let orbit_key = control.registry().orbit_key_issued();
    async move {
        match visitor {
            Visitor::Worker { issued } => superseded(orbit_key, issued).await,
            Visitor::Runner {
                fingerprint: runner,
                issued,
            } => {
                let lost_runner = async {
                    loop {
                        let lost = tokio::select! {
                            lost = revoked.recv() => lost,
                            lost = released.recv() => lost,
                        };
                        match lost {
                            Ok(fingerprint) if fingerprint == runner => return,
                            Ok(_) => {}
                            // Пропущенные события неизвестны: безопаснее закрыть, runner откроет туннель заново и пройдёт проверку.
                            Err(RecvError::Lagged(_)) => return,
                            Err(RecvError::Closed) => std::future::pending::<()>().await,
                        }
                    }
                };
                tokio::select! {
                    () = lost_runner => {}
                    () = superseded(orbit_key, issued) => {}
                }
            }
        }
    }
}
