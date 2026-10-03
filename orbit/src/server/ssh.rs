//! SSH orbit server: центр сертификации, внешний и внутренний слушатели, вход по имени входа.
//!
//! Один процесс `russh`, два слушателя: внешний принимает runner и worker, внутренний — session. Оболочек, `exec`,
//! pty и пробросов, кроме `direct-tcpip` jump, нет.
//!
//! | Слушатель | Логин | Метод | Что разрешено |
//! |---|---|---|---|
//! | внешний | `iapetus-enroll` | `password` — `orbit_key` | подсистема `iapetus-control`, запросы входа |
//! | внешний | `iapetus-control` | сертификат | подсистема `iapetus-control`, запросы роли по `key_id` |
//! | внешний | `iapetus-jump` | сертификат | `direct-tcpip` в session по маршруту реестра |
//! | внутренний | `session-<fp>` | `password` — секрет session | подсистема `iapetus-control`, запросы session |
//!
//! Роль — по `key_id` сертификата: `worker:` — worker, иначе fingerprint runner. До входа соединение живёт
//! [`LOGIN_DEADLINE`]; первый отказ во входе приходит сразу, следующие — с паузой.

pub(super) mod ca;
mod jump;
pub(super) mod routes;

#[cfg(test)]
#[path = "../../tests/server/ssh.rs"]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve::fingerprint::Fingerprint;
use nerve::ssh::{
    CLOCK_SKEW, CONTROL_LOGIN, CONTROL_SUBSYSTEM, ENROLL_LOGIN, JUMP_LOGIN, KEEPALIVE_INTERVAL, KEEPALIVE_MAX, PACKET,
    SESSION_LOGIN_PREFIX, Trust, WINDOW,
};
use russh::keys::{Certificate, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Session, run_stream};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect, MethodKind};
use tokio::net::{TcpListener, TcpStream};

pub(super) use self::jump::{Jump, Visitor};
use super::control::{Control, Peer};
use super::registry::{OrbitKey, SessionSecret};
use nerve::ssh::WORKER_KEY_ID;

/// Сколько соединение живёт без входа.
pub(super) const LOGIN_DEADLINE: Duration = Duration::from_secs(10);

/// Какой слушатель принял соединение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    /// Внешний: runner и worker, входы `iapetus-enroll`, `iapetus-control`, `iapetus-jump`.
    External,
    /// Внутренний, из сети Docker: только `session-<fp>`.
    Internal,
}

/// Конфигурация слушателя: методы входа по стороне, хост-сертификат, keepalive 5 с × 3. Окно и пакет — как у
/// канала данных: через jump идёт его шифротекст, и узкое окно jump ограничило бы всю передачу.
pub(super) fn config(host_key: PrivateKey, host_cert: Certificate, side: Side) -> Config {
    let methods = match side {
        Side::External => [MethodKind::Password, MethodKind::PublicKey][..].into(),
        Side::Internal => [MethodKind::Password][..].into(),
    };
    Config {
        keys: vec![host_key],
        certificates: vec![host_cert],
        methods,
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: None,
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        window_size: WINDOW,
        maximum_packet_size: PACKET,
        channel_buffer_size: 1024,
        nodelay: true,
        ..Config::default()
    }
}

/// Что слушатель делает с соединениями: общее для всех его соединений.
#[derive(Debug, Clone)]
pub(super) struct Gate {
    side: Side,
    control: Arc<Control>,
    trust: Arc<Trust>,
    jump: Arc<Jump>,
}

impl Gate {
    pub(super) fn new(side: Side, control: Arc<Control>, trust: Arc<Trust>, jump: Arc<Jump>) -> Self {
        Self {
            side,
            control,
            trust,
            jump,
        }
    }
}

/// Принимает соединения на `listener`, пока не оборвётся приём.
pub(super) async fn serve(gate: Gate, listener: TcpListener, config: Config) -> std::io::Result<()> {
    let config = Arc::new(config);
    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(connection(gate.clone(), Arc::clone(&config), stream, peer));
    }
}

/// Одно соединение: SSH до закрытия, но без входа за [`LOGIN_DEADLINE`] — не дольше.
async fn connection(gate: Gate, config: Arc<Config>, stream: TcpStream, peer: SocketAddr) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%peer, %error, "TCP_NODELAY не выставлен");
    }
    let entry = Entry {
        gate,
        peer,
        login: None,
        authenticated: Arc::new(AtomicBool::new(false)),
        pending: HashMap::new(),
        served: false,
    };
    let authenticated = Arc::clone(&entry.authenticated);
    let session = match run_stream(config, stream, entry).await {
        Ok(session) => session,
        Err(error) => {
            tracing::debug!(%peer, %error, "SSH-соединение не начато");
            return;
        }
    };
    let handle = session.handle();
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(LOGIN_DEADLINE).await;
        if !authenticated.load(Ordering::Acquire) {
            tracing::info!(%peer, "вход не выполнен вовремя, соединение закрыто");
            drop(
                handle
                    .disconnect(Disconnect::ByApplication, "вход не выполнен вовремя".to_owned(), String::new())
                    .await,
            );
        }
    });
    if let Err(error) = session.await {
        tracing::debug!(%peer, %error, "SSH-соединение закрыто с ошибкой");
    }
    watchdog.abort();
}

/// Кто вошёл и что ему открыто.
#[derive(Debug, Clone)]
enum Login {
    /// Подсистема `iapetus-control` с набором запросов роли.
    Control(Peer),
    /// `direct-tcpip` через jump.
    Jump(Visitor),
}

/// Одно соединение: его вход и каналы, открытые до запроса подсистемы.
struct Entry {
    gate: Gate,
    peer: SocketAddr,
    login: Option<Login>,
    authenticated: Arc<AtomicBool>,
    /// Принятые каналы `session`: подсистема запрашивается отдельным сообщением.
    pending: HashMap<ChannelId, Channel<Msg>>,
    /// Канал управления уже запущен: второй на соединение не открывается.
    served: bool,
}

impl Entry {
    fn accept(&mut self, login: Login) -> Auth {
        tracing::info!(peer = %self.peer, ?login, "вход принят");
        self.login = Some(login);
        self.authenticated.store(true, Ordering::Release);
        Auth::Accept
    }

    fn reject(&self, user: &str, reason: &str) -> Auth {
        tracing::warn!(peer = %self.peer, user, reason, "вход отклонён");
        Auth::reject()
    }

    /// Роль и срок по сертификату, уже проверенному по CA. `Err` — причина отказа.
    fn visitor(&self, cert: &Certificate) -> Result<Visitor, &'static str> {
        // Сертификат подписан с началом срока на допуск раньше момента выдачи.
        let issued = cert.valid_after() + CLOCK_SKEW.as_secs();
        let key_issued = *self.gate.control.registry().orbit_key_issued().borrow();
        if key_issued.is_some_and(|key| u64::try_from(key).is_ok_and(|key| key > issued)) {
            return Err("сертификат выдан до нового orbit_key");
        }
        if cert.key_id() == WORKER_KEY_ID {
            return Ok(Visitor::Worker { issued });
        }
        let Ok(fingerprint) = cert.key_id().parse::<Fingerprint>() else {
            return Err("key_id сертификата не fingerprint узла и не worker");
        };
        if self.gate.control.registry().active_node(&fingerprint).is_err() {
            return Err("runner неизвестен или отозван");
        }
        Ok(Visitor::Runner { fingerprint, issued })
    }
}

impl Handler for Entry {
    type Error = anyhow::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let control = Arc::clone(&self.gate.control);
        match self.gate.side {
            Side::External if user == ENROLL_LOGIN => {
                if control.registry().check_orbit_key(&OrbitKey::new(password)).is_err() {
                    return Ok(self.reject(user, "неверный orbit_key"));
                }
                Ok(self.accept(Login::Control(Peer::Enroll)))
            }
            Side::Internal => {
                let Some(session) = user
                    .strip_prefix(SESSION_LOGIN_PREFIX)
                    .and_then(|text| text.parse::<Fingerprint>().ok())
                else {
                    return Ok(self.reject(user, "имя входа не session-<fp>"));
                };
                let matches = control.registry().session_secret_matches(&session, &SessionSecret::new(password));
                if !matches.unwrap_or(false) {
                    return Ok(self.reject(user, "неверный секрет session"));
                }
                let peer = Peer::Session {
                    fingerprint: session,
                    ip: self.peer.ip(),
                };
                Ok(self.accept(Login::Control(peer)))
            }
            Side::External => Ok(self.reject(user, "по паролю входит только iapetus-enroll")),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_publickey(&mut self, user: &str, _key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(self.reject(user, "вход по ключу без сертификата"))
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_openssh_certificate(&mut self, user: &str, cert: &Certificate) -> Result<Auth, Self::Error> {
        if self.gate.side != Side::External || ![CONTROL_LOGIN, JUMP_LOGIN].contains(&user) {
            return Ok(self.reject(
                user,
                "по сертификату входят только iapetus-control и iapetus-jump на внешнем слушателе",
            ));
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if let Err(reason) = self.gate.trust.check_user_cert(cert, user, now) {
            return Ok(self.reject(user, reason));
        }
        let visitor = match self.visitor(cert) {
            Ok(visitor) => visitor,
            Err(reason) => return Ok(self.reject(user, reason)),
        };
        if user == JUMP_LOGIN {
            return Ok(self.accept(Login::Jump(visitor)));
        }
        let valid_before = cert.valid_before();
        let peer = match visitor {
            Visitor::Runner { fingerprint, issued } => Peer::Runner {
                fingerprint,
                issued,
                valid_before,
            },
            Visitor::Worker { issued } => Peer::Worker { issued, valid_before },
        };
        Ok(self.accept(Login::Control(peer)))
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !matches!(self.login, Some(Login::Control(_))) || self.served || !self.pending.is_empty() {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        }
        reply.accept().await;
        self.pending.insert(channel.id(), channel);
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        let (Some(Login::Control(peer)), Some(opened)) = (self.login.clone(), self.pending.remove(&channel)) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if name != CONTROL_SUBSYSTEM {
            tracing::warn!(peer = %self.peer, name, "подсистема не iapetus-control");
            session.channel_failure(channel)?;
            return Ok(());
        }
        session.channel_success(channel)?;
        self.served = true;
        let control = Arc::clone(&self.gate.control);
        let handle = session.handle();
        tokio::spawn(async move {
            control.serve(peer, opened.into_stream()).await;
            drop(handle.disconnect(Disconnect::ByApplication, String::new(), String::new()).await);
        });
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(Login::Jump(visitor)) = &self.login else {
            reply.reject(ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        };
        let opened = self.gate.jump.open(visitor, channel, host, port, reply).await;
        // Вошедший, которого в сессию не пускают (нет назначения, агент не запущен, названа не та сессия), теряет и соединение.
        if !opened {
            session.disconnect(Disconnect::ByApplication, "туннель в сессию не открыт", "")?;
        }
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(session.channel_failure(channel)?)
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        Ok(session.channel_failure(channel)?)
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn exec_request(&mut self, channel: ChannelId, _data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        Ok(session.channel_failure(channel)?)
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn env_request(&mut self, channel: ChannelId, _name: &str, _value: &str, session: &mut Session) -> Result<(), Self::Error> {
        Ok(session.channel_failure(channel)?)
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _protocol: &str,
        _cookie: &str,
        _screen: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(session.channel_failure(channel)?)
    }
}
