//! SSH-сервер сессии: в него входят назначенный runner (канал данных) и worker (терминал агента).
//!
//! Оба входят сертификатом через jump; сертификат даёт только право войти. Runner — логин `iapetus-data`; для него
//! сессия — сервер без shell, `exec` и pty с одной подсистемой с мультиплексором ([`Mux`]), которую сервер отдаёт
//! [`Data`]. Worker — сертификат с `key_id` `worker`, логин — его `name`; ему нужны pty и shell, и только при
//! расширении `permit-pty` в сертификате: тогда он становится зрителем терминала агента ([`Terminal`]).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nerve::fingerprint::Fingerprint;
use nerve::mux::Mux;
use nerve::ssh::{DATA_LOGIN, DATA_SUBSYSTEM, KEEPALIVE_INTERVAL, KEEPALIVE_MAX, NODE_LOGIN, PACKET, Trust, WINDOW, WORKER_KEY_ID};
use russh::keys::{Certificate, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Disconnect, MethodKind};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use super::data::Data;
use super::terminal::{Size, Terminal};

#[cfg(test)]
#[path = "../../tests/session/ssh.rs"]
mod tests;

/// Расширение сертификата, без которого pty не выдаётся.
const PERMIT_PTY: &str = "permit-pty";

/// Префикс логинов Iapetus: имя worker с ним не может быть именем входа.
const RESERVED_LOGIN_PREFIX: &str = "iapetus-";

/// Конфигурация сервера: только publickey с сертификатом, хост-сертификат, окно и keepalive.
pub(super) fn config(host_key: PrivateKey, host_cert: Certificate) -> Config {
    Config {
        keys: vec![host_key],
        certificates: vec![host_cert],
        methods: [MethodKind::PublicKey][..].into(),
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
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

/// SSH-сервер канала данных.
#[derive(Debug, Clone)]
pub(super) struct Listener {
    data: Data,
    /// Центр сертификации узлов.
    trust: Arc<Trust>,
    config: Arc<Config>,
    /// Слот терминала агента: его заполняет сессия, когда агент запущен; пока он пуст, worker не пускают.
    terminal: watch::Receiver<Option<Terminal>>,
    /// Сколько worker сейчас смотрят терминал.
    viewers: watch::Sender<usize>,
}

/// Worker смотрит терминал, пока значение живо.
#[derive(Debug)]
struct Viewing {
    viewers: watch::Sender<usize>,
}

impl Viewing {
    fn start(viewers: &watch::Sender<usize>) -> Self {
        viewers.send_modify(|count| *count += 1);
        Self { viewers: viewers.clone() }
    }
}

impl Drop for Viewing {
    fn drop(&mut self) {
        self.viewers.send_modify(|count| *count -= 1);
    }
}

impl Listener {
    pub(super) fn new(data: Data, trust: Trust, config: Config, terminal: watch::Receiver<Option<Terminal>>) -> Self {
        Self {
            data,
            trust: Arc::new(trust),
            config: Arc::new(config),
            terminal,
            viewers: watch::channel(0).0,
        }
    }

    /// Сколько worker смотрят терминал; меняется при входе зрителя и при его уходе.
    pub(super) fn viewers(&self) -> watch::Receiver<usize> {
        self.viewers.subscribe()
    }

    /// Принимает соединения на `listener`, пока не оборвётся приём.
    pub(super) async fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        let mut server = self.clone();
        server.run_on_socket(Arc::clone(&self.config), &listener).await
    }
}

impl Server for Listener {
    type Handler = Connection;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Connection {
        Connection {
            data: self.data.clone(),
            trust: Arc::clone(&self.trust),
            id: self.data.next_connection_id(),
            terminal: self.terminal.clone(),
            viewers: self.viewers.clone(),
            runner: None,
            worker: None,
            channel: None,
            bound: false,
            worker_channel: None,
            pty: None,
            viewing: false,
        }
    }
}

/// Одно входящее соединение.
#[derive(Debug)]
pub(super) struct Connection {
    data: Data,
    trust: Arc<Trust>,
    id: u64,
    terminal: watch::Receiver<Option<Terminal>>,
    viewers: watch::Sender<usize>,
    /// Fingerprint из сертификата, принятого при входе.
    runner: Option<Fingerprint>,
    /// Вошёл worker; в нём — разрешён ли pty.
    worker: Option<Worker>,
    /// Канал `session` runner, открытый, но ещё не ставший каналом данных.
    channel: Option<Channel<Msg>>,
    /// Канал данных открыт: другой такой же не нужен.
    bound: bool,
    /// Канал `session` worker.
    worker_channel: Option<ChannelId>,
    /// Окно, с которым worker запросил pty.
    pty: Option<Size>,
    /// Worker смотрит терминал: `shell` принят, а канал ещё не закрыт. Счётчик зрителей держит задача `forward`.
    viewing: bool,
}

#[derive(Debug, Clone, Copy)]
struct Worker {
    /// В сертификате есть `permit-pty`.
    may_pty: bool,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.data.release(self.id);
        if let Some(terminal) = self.terminal() {
            terminal.detach(self.id);
        }
    }
}

/// Отдаёт зрителю вывод терминала; когда канал вывода закрыт (агент вышел, зритель отстал), закрывает SSH-канал.
/// Зритель перестаёт числиться, как только вывод кончился или не доставляется, не дожидаясь ответа клиента на закрытие.
async fn forward(mut output: mpsc::Receiver<bytes::Bytes>, handle: Handle, channel: ChannelId, viewing: Viewing) {
    while let Some(chunk) = output.recv().await {
        if handle.data(channel, chunk).await.is_err() {
            return;
        }
    }
    drop(viewing);
    if handle.close(channel).await.is_err() {
        tracing::debug!("соединение worker закрыто раньше канала");
    }
}

impl Connection {
    /// Терминал агента на сейчас: после перезапуска агента в слоте новый.
    fn terminal(&self) -> Option<Terminal> {
        self.terminal.borrow().clone()
    }

    fn check_certificate(&self, user: &str, cert: &Certificate) -> Result<Fingerprint, &'static str> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.trust.check_user_cert(cert, user, now)?;
        let runner: Fingerprint = cert.key_id().parse().map_err(|_| "key_id сертификата не fingerprint узла")?;
        if !self.data.is_assigned(&runner) {
            return Err("runner не назначен этой сессии");
        }
        Ok(runner)
    }

    /// Worker входит под `name` из сертификата: логин должен быть среди принципалов и не быть служебным.
    fn check_worker_certificate(&self, user: &str, cert: &Certificate) -> Result<Worker, &'static str> {
        if user == NODE_LOGIN || user.starts_with(RESERVED_LOGIN_PREFIX) {
            return Err("имя входа служебное");
        }
        if cert.key_id() != WORKER_KEY_ID {
            return Err("key_id сертификата не worker");
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.trust.check_user_cert(cert, user, now)?;
        Ok(Worker {
            may_pty: cert.extensions().contains_key(PERMIT_PTY),
        })
    }
}

impl Handler for Connection {
    type Error = anyhow::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_publickey(&mut self, user: &str, _key: &PublicKey) -> Result<Auth, Self::Error> {
        tracing::warn!(user, "канал данных: вход по ключу без сертификата отклонён");
        Ok(Auth::reject())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_openssh_certificate(&mut self, user: &str, cert: &Certificate) -> Result<Auth, Self::Error> {
        if user == DATA_LOGIN {
            return match self.check_certificate(user, cert) {
                Ok(runner) => {
                    self.runner = Some(runner);
                    Ok(Auth::Accept)
                }
                Err(reason) => {
                    tracing::warn!(user, key_id = cert.key_id(), reason, "канал данных: вход отклонён");
                    Ok(Auth::reject())
                }
            };
        }
        match self.check_worker_certificate(user, cert) {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(Auth::Accept)
            }
            Err(reason) => {
                tracing::warn!(user, key_id = cert.key_id(), reason, "терминал: вход отклонён");
                Ok(Auth::reject())
            }
        }
    }

    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        let Some(runner) = self.runner.clone() else {
            return Ok(());
        };
        if !self.data.activate(self.id, &runner, session.handle()) {
            tracing::warn!(%runner, "runner сменился во время входа");
            self.runner = None;
            session
                .handle()
                .disconnect(Disconnect::ByApplication, "runner сменился".to_owned(), String::new())
                .await?;
        }
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.worker.is_some() {
            // Канал worker читают обработчики запросов; держать `Channel` нельзя: пока его никто не читает,
            // русш ждёт места в его очереди и останавливает соединение.
            if self.worker_channel.is_some() || self.terminal().is_none() {
                reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
                return Ok(());
            }
            self.worker_channel = Some(channel.id());
            reply.accept().await;
            return Ok(());
        }
        if self.runner.is_none() || self.channel.is_some() || self.bound {
            reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        }
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        let Some(stream) = self
            .channel
            .take_if(|opened| opened.id() == channel && name == DATA_SUBSYSTEM)
            .map(Channel::into_stream)
        else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let (mux, requests) = Mux::new(stream);
        if !self.data.attach(self.id, mux, requests) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        self.bound = true;
        session.channel_success(channel)?;
        Ok(())
    }

    // Команд и переменных окружения у сервера нет: worker получает только pty с оболочкой, то есть терминал агента.
    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn exec_request(&mut self, channel: ChannelId, _data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn env_request(&mut self, channel: ChannelId, _name: &str, _value: &str, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let allowed = self.worker.is_some_and(|worker| worker.may_pty) && self.worker_channel == Some(channel) && self.pty.is_none();
        if !allowed {
            tracing::warn!("терминал: pty отклонён");
            session.channel_failure(channel)?;
            return Ok(());
        }
        self.pty = Some(Size::new(row_height, col_width));
        session.channel_success(channel)?;
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        let (Some(size), Some(terminal), true) = (self.pty, self.terminal(), self.worker_channel == Some(channel) && !self.viewing) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let Some(output) = terminal.attach(self.id, size) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        self.viewing = true;
        session.channel_success(channel)?;
        tokio::spawn(forward(output, session.handle(), channel, Viewing::start(&self.viewers)));
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let (true, Some(terminal)) = (self.viewing && self.worker_channel == Some(channel), self.terminal()) {
            terminal.input(bytes::Bytes::copy_from_slice(data));
        }
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let (true, Some(terminal)) = (self.viewing && self.worker_channel == Some(channel), self.terminal()) {
            terminal.resize(self.id, Size::new(row_height, col_width));
        }
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        if self.worker_channel == Some(channel) {
            if let Some(terminal) = self.terminal() {
                terminal.detach(self.id);
            }
            self.viewing = false;
        }
        Ok(())
    }
}
