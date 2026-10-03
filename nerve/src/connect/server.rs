//! Клиент канала управления: вход на SSH orbit server, подсистема `iapetus-control`, `Mux` поверх неё и
//! переподключение. Им пользуются runner, worker и session.
//!
//! Три входа ([`Login`]): `iapetus-enroll` по паролю — `orbit_key` — для регистрации и выдачи сертификата,
//! `iapetus-control` по сертификату, `session-<fp>` по секрету session. Пароль уходит только после проверки
//! хост-сертификата server по CA: без неё соединение рвётся до входа.

#[cfg(test)]
#[path = "../../tests/connect/server.rs"]
mod tests;

use std::fmt;
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use russh::client::Handle;
use russh::keys::{Certificate, PrivateKey, PublicKey};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;

use super::certificate;
use super::session::{HostVerifier, dial, open_mux};
use crate::fingerprint::Fingerprint;
use crate::mux::{Closed, Mux, Requests, Responses};
use crate::protocol::{
    self, EnrollRequest, Error, RenewRequest, RenewResponse, RunnerRequest, WorkerCertificateRequest, WorkerCertificateResponse,
    WorkerRequest,
};
use crate::ssh::{CONTROL_LOGIN, CONTROL_SUBSYSTEM, ENROLL_LOGIN, session_login};

/// Нижняя граница паузы между попытками подключения; пауза удваивается до [`RECONNECT_MAX`].
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);

/// Верхняя граница паузы между попытками подключения.
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Пауза после несовпадения версии протокола: повторять чаще бесполезно и шумно.
pub const VERSION_PAUSE: Duration = Duration::from_secs(30);

/// Сколько ждать подключения и входа, а также ответа на запрос входа: server закрывает вход без запроса через 10 с.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Где server и кому в нём доверять.
#[derive(Debug, Clone)]
pub struct Server {
    /// Адрес SSH orbit server, `хост:порт`.
    pub address: String,
    /// Имя, которое server носит в своём хост-сертификате.
    pub host: String,
    /// Открытый ключ CA (`orbit_ca`): хост-сертификат server должен быть им подписан.
    pub ca: PublicKey,
}

/// Чем клиент входит.
pub enum Login<'a> {
    /// `iapetus-enroll`: узел без действующего сертификата, пароль — `orbit_key`.
    Enroll { orbit_key: &'a str },
    /// `iapetus-control`: runner или worker с сертификатом.
    Certificate { key: &'a PrivateKey, certificate: &'a Certificate },
    /// `session-<fp>`: session, пароль — её секрет.
    Session { session: &'a Fingerprint, secret: &'a str },
}

impl fmt::Debug for Login<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enroll { .. } => f.write_str("Enroll"),
            Self::Certificate { certificate, .. } => f
                .debug_struct("Certificate")
                .field("key_id", &certificate.key_id())
                .finish_non_exhaustive(),
            Self::Session { session, .. } => f.debug_struct("Session").field("session", session).finish_non_exhaustive(),
        }
    }
}

/// Почему подключиться не удалось.
#[derive(Debug)]
pub enum ConnectError {
    /// server не принял вход: сертификат истёк или недействителен, пароль или секрет неверны.
    Rejected,
    /// server ответил `Access`: узел отозван или `orbit_key` заменён; повторять бесполезно.
    Denied,
    /// Нет сети, хост-сертификат не принят, подсистема не открылась и тому подобное: повтор возможен.
    Other(anyhow::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected => f.write_str("server не принял вход"),
            Self::Denied => f.write_str("server отказал в доступе: узел отозван или orbit_key заменён"),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<anyhow::Error> for ConnectError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

/// Открытый канал управления. Пока значение живёт, живёт SSH-соединение.
pub struct Channel {
    pub mux: Mux,
    /// Запросы server.
    pub requests: Requests,
    _ssh: Handle<HostVerifier>,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel").finish_non_exhaustive()
    }
}

/// Ответы на один запрос, разобранные в `T`.
#[derive(Debug)]
pub struct Replies<T> {
    responses: Responses,
    reply: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Replies<T> {
    /// Следующий ответ; `None` — последний уже получен.
    pub async fn next(&mut self) -> anyhow::Result<Option<T>> {
        let Some(body) = self.responses.next().await? else {
            return Ok(None);
        };
        let reply = protocol::decode_body(&body).map_err(|error| anyhow!("ответ не разобран: {error}"))?;
        Ok(Some(reply))
    }
}

/// Запрос с одним ответом.
pub async fn ask<Req: Serialize, Resp: DeserializeOwned>(mux: &Mux, request: &Req) -> anyhow::Result<Resp> {
    let body = mux.request(protocol::encode(request)?.into()).await?;
    protocol::decode_body(&body).map_err(|error| anyhow!("ответ не разобран: {error}"))
}

/// Запрос, на который идёт поток ответов.
pub async fn subscribe<Req: Serialize, Resp: DeserializeOwned>(mux: &Mux, request: &Req) -> anyhow::Result<Replies<Resp>> {
    let responses = mux.call(protocol::encode(request)?.into()).await?;
    Ok(Replies {
        responses,
        reply: PhantomData,
    })
}

/// Входит на server и открывает канал управления. Версия протокола уходит первым кадром `Mux`; несовпадение
/// закрывает канал с причиной [`Closed::Version`], её видно по [`Mux::closed`].
pub async fn open(server: &Server, login: &Login<'_>) -> Result<Channel, ConnectError> {
    tokio::time::timeout(CONNECT_TIMEOUT, open_unbounded(server, login))
        .await
        .map_err(|_| anyhow!("server {} не ответил за {CONNECT_TIMEOUT:?}", server.address))?
}

async fn open_unbounded(server: &Server, login: &Login<'_>) -> Result<Channel, ConnectError> {
    let mut ssh = dial(&server.address, &server.host, &server.ca).await?;
    let result = match login {
        Login::Enroll { orbit_key } => ssh.authenticate_password(ENROLL_LOGIN, *orbit_key).await,
        Login::Certificate { key, certificate } => {
            ssh.authenticate_openssh_cert(CONTROL_LOGIN, Arc::new((*key).clone()), (*certificate).clone())
                .await
        }
        Login::Session { session, secret } => ssh.authenticate_password(session_login(session), *secret).await,
    };
    if !result.context("вход на server не выполнен")?.success() {
        return Err(ConnectError::Rejected);
    }
    let (mux, requests) = open_mux(&ssh, CONTROL_SUBSYSTEM).await?;
    Ok(Channel { mux, requests, _ssh: ssh })
}

/// Запрос входа `iapetus-enroll`: открывает соединение, шлёт `request`, возвращает ответ и закрывает соединение.
/// `Resp` — ответ на этот запрос: [`RegisterResponse`](protocol::RegisterResponse), [`RenewResponse`] или
/// [`WorkerCertificateResponse`].
pub async fn enroll<Resp: DeserializeOwned>(server: &Server, orbit_key: &str, request: &EnrollRequest) -> Result<Resp, ConnectError> {
    let channel = open(server, &Login::Enroll { orbit_key }).await?;
    let response = tokio::time::timeout(CONNECT_TIMEOUT, ask(&channel.mux, request))
        .await
        .map_err(|_| anyhow!("server не ответил на запрос входа"))??;
    channel.mux.close();
    Ok(response)
}

/// Кто держит сертификат.
#[derive(Debug, Clone)]
pub enum Role {
    /// Runner: сертификат на ключ узла, `key_id` — fingerprint; продление подписывается ключом узла.
    Runner { fingerprint: Fingerprint },
    /// Worker: сертификат на одноразовый ключ, принципал — `name`.
    Worker { name: String },
}

/// Runner или worker с ключом, `orbit_key` и сертификатом: подключается к каналу управления, продлевает сертификат
/// и переподключается.
pub struct Node {
    server: Server,
    key: PrivateKey,
    orbit_key: String,
    role: Role,
    certificate: watch::Sender<Option<Certificate>>,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node").field("role", &self.role).finish_non_exhaustive()
    }
}

/// [`ConnectError::Denied`] как результат [`keep_connected`]: цикл подключения остановлен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied;

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&ConnectError::Denied.to_string())
    }
}

impl std::error::Error for Denied {}

/// Подключается и обслуживает канал, пока server не откажет в доступе. `connect` открывает канал, `serve` работает с
/// ним и возвращается, когда канал закрыт. После обрыва пауза растёт от [`RECONNECT_MIN`] вдвое до
/// [`RECONNECT_MAX`] с разбросом; после несовпадения версии — [`VERSION_PAUSE`]. Ошибки подключения пишутся в
/// журнал и повторяются; [`ConnectError::Denied`] завершает цикл.
pub async fn keep_connected(connect: impl AsyncFn() -> Result<Channel, ConnectError>, serve: impl AsyncFn(Channel)) -> Denied {
    let mut pause = Pause::default();
    loop {
        match connect().await {
            Ok(channel) => {
                pause = Pause::default();
                let mux = channel.mux.clone();
                serve(channel).await;
                let reason = if mux.is_closed() {
                    mux.closed().await
                } else {
                    mux.close();
                    Closed::Local
                };
                tracing::warn!(%reason, "канал управления закрыт");
                if matches!(reason, Closed::Version(_)) {
                    tokio::time::sleep(VERSION_PAUSE).await;
                    continue;
                }
            }
            Err(ConnectError::Denied) => return Denied,
            Err(error) => tracing::warn!(%error, "канал управления не открыт"),
        }
        tokio::time::sleep(pause.next()).await;
    }
}

/// Паузы между попытками: удвоение с разбросом вверх до половины паузы, в пределах от минимума до максимума.
#[derive(Debug, Clone, Copy)]
struct Pause {
    base: Duration,
}

impl Default for Pause {
    fn default() -> Self {
        Self { base: RECONNECT_MIN }
    }
}

impl Pause {
    fn next(&mut self) -> Duration {
        let pause = self.base.mul_f64(1.0 + rand::random::<f64>() / 2.0).min(RECONNECT_MAX);
        self.base = (self.base * 2).min(RECONNECT_MAX);
        pause
    }
}

impl Node {
    /// `certificate` — сертификат, который у узла уже есть (runner хранит его на диске); без него первое
    /// подключение начнётся с запроса через `iapetus-enroll`.
    pub fn new(server: Server, key: PrivateKey, orbit_key: String, role: Role, certificate: Option<Certificate>) -> Self {
        Self {
            server,
            key,
            orbit_key,
            role,
            certificate: watch::channel(certificate).0,
        }
    }

    pub fn certificate(&self) -> Option<Certificate> {
        self.certificate.borrow().clone()
    }

    /// Новые сертификаты: runner сохраняет их на диск.
    pub fn certificates(&self) -> watch::Receiver<Option<Certificate>> {
        self.certificate.subscribe()
    }

    /// Открывает канал управления по сертификату. Нет сертификата, он истёк или server его отклонил — сначала новый
    /// через `iapetus-enroll`.
    pub async fn connect(&self) -> Result<Channel, ConnectError> {
        let now = unix(SystemTime::now());
        let expired = self.certificate().is_none_or(|certificate| now >= certificate.valid_before());
        if expired {
            self.renew_by_enroll().await?;
        }
        match self.open().await {
            Err(ConnectError::Rejected) if !expired => {
                self.renew_by_enroll().await?;
                self.open().await
            }
            result => result,
        }
    }

    async fn open(&self) -> Result<Channel, ConnectError> {
        let certificate = self.certificate().context("сертификата нет")?;
        open(
            &self.server,
            &Login::Certificate {
                key: &self.key,
                certificate: &certificate,
            },
        )
        .await
    }

    /// Подключается и обслуживает канал, продлевая сертификат по живому каналу; см. [`keep_connected`]. Возвращается,
    /// когда server отказал в доступе.
    pub async fn run(&self, serve: impl AsyncFn(Channel)) -> Denied {
        keep_connected(
            || self.connect(),
            async |channel: Channel| {
                let mux = channel.mux.clone();
                tokio::select! {
                    () = serve(channel) => {}
                    Denied = self.keep_fresh(&mux) => {}
                }
            },
        )
        .await
    }

    /// Продлевает сертификат по каналу `mux`, пока он живёт; возвращается при `Access`.
    async fn keep_fresh(&self, mux: &Mux) -> Denied {
        certificate::keep_fresh_until(
            || self.certificate(),
            async || match self.renew(Some(mux)).await {
                Ok(()) => Ok(ControlFlow::Continue(())),
                Err(ConnectError::Denied) => Ok(ControlFlow::Break(Denied)),
                Err(error) => Err(error.into()),
            },
        )
        .await
    }

    /// Продлевает сертификат: по каналу управления `mux`, без него — через `iapetus-enroll`.
    pub async fn renew(&self, mux: Option<&Mux>) -> Result<(), ConnectError> {
        let text = match (&self.role, mux) {
            (Role::Runner { fingerprint }, mux) => {
                let request =
                    RenewRequest::signed(&self.key, fingerprint, &self.orbit_key, unix(SystemTime::now())).map_err(|e| anyhow!(e))?;
                let response: RenewResponse = match mux {
                    Some(mux) => tokio::time::timeout(CONNECT_TIMEOUT, ask(mux, &RunnerRequest::Renew(request)))
                        .await
                        .map_err(|_| anyhow!("server не ответил на продление"))??,
                    None => enroll(&self.server, &self.orbit_key, &EnrollRequest::Renew(request)).await?,
                };
                response.map(|reply| reply.certificate)
            }
            (Role::Worker { name }, mux) => {
                let request = WorkerCertificateRequest {
                    orbit_key: self.orbit_key.clone(),
                    public_key: self.key.public_key().to_openssh().map_err(|e| anyhow!(e))?,
                    name: name.clone(),
                };
                let response: WorkerCertificateResponse = match mux {
                    Some(mux) => tokio::time::timeout(CONNECT_TIMEOUT, ask(mux, &WorkerRequest::WorkerCertificate(request)))
                        .await
                        .map_err(|_| anyhow!("server не ответил на запрос сертификата"))??,
                    None => enroll(&self.server, &self.orbit_key, &EnrollRequest::WorkerCertificate(request)).await?,
                };
                response.map(|reply| reply.certificate)
            }
        };
        let text = text.map_err(|error| match error {
            Error::Access => ConnectError::Denied,
            other => ConnectError::Other(anyhow!("server отказал в сертификате: {other}")),
        })?;
        let certificate = Certificate::from_openssh(&text).context("сертификат от server не разбирается")?;
        if certificate.public_key() != self.key.public_key().key_data() {
            return Err(anyhow!("сертификат от server выдан не на ключ узла").into());
        }
        if let Role::Runner { fingerprint } = &self.role
            && certificate.key_id() != fingerprint.as_str()
        {
            return Err(anyhow!("key_id сертификата от server не совпадает с fingerprint узла").into());
        }
        self.certificate.send_replace(Some(certificate));
        tracing::info!("сертификат получен");
        Ok(())
    }

    async fn renew_by_enroll(&self) -> Result<(), ConnectError> {
        self.renew(None).await
    }
}

fn unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}
