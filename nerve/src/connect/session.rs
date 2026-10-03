//! SSH-соединение узла к сессии: через jump orbit server, с проверкой хост-сертификатов по CA.
//!
//! Два слоя: внешний — вход на jump сертификатом узла; внутри его `direct-tcpip` — второе SSH-соединение
//! к сессии, и jump видит только его шифротекст. На обоих слоях узел проверяет хост-сертификат своего собеседника.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mux::{Mux, Requests};
use crate::ssh::{DATA_HOST, DATA_LOGIN, DATA_SUBSYSTEM, JUMP_LOGIN, KEEPALIVE_INTERVAL, KEEPALIVE_MAX, PACKET, Trust, WINDOW};
use anyhow::{Context, bail};
use russh::client::{self, Handle};
use russh::keys::ssh_key::Algorithm;
use russh::keys::{Certificate, PrivateKey, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Preferred};
use tokio::net::TcpStream;

/// Принимает только хост-сертификат нашего CA с именем `host`: голый ключ хоста не принимается.
#[derive(Debug)]
pub struct HostVerifier {
    trust: Trust,
    host: String,
}

impl client::Handler for HostVerifier {
    type Error = anyhow::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::Certificate(cert) = key else {
            tracing::warn!(host = %self.host, "собеседник предъявил ключ без хост-сертификата");
            return Ok(false);
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        match self.trust.check_host_cert(cert, &self.host, now) {
            Ok(()) => Ok(true),
            Err(reason) => {
                tracing::warn!(host = %self.host, reason, "хост-сертификат отвергнут");
                Ok(false)
            }
        }
    }
}

/// Настройки клиента SSH для обоих слоёв.
fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Без явного согласия клиента сервер предъявил бы голый ключ вместо хост-сертификата.
        preferred: Preferred {
            host_key_certificates: Cow::Borrowed(&[Algorithm::Ed25519]),
            ..Preferred::DEFAULT
        },
        inactivity_timeout: None,
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        window_size: WINDOW,
        maximum_packet_size: PACKET,
        channel_buffer_size: 1024,
        nodelay: true,
        ..client::Config::default()
    })
}

/// Соединяется с SSH-сервером `address` (`хост:порт`) и принимает только хост-сертификат нашего CA с именем `host`.
pub(super) async fn dial(address: &str, host: &str, ca: &PublicKey) -> anyhow::Result<Handle<HostVerifier>> {
    let tcp = TcpStream::connect(address).await.with_context(|| format!("{address} недоступен"))?;
    tcp.set_nodelay(true)?;
    let verifier = HostVerifier {
        trust: Trust::new(std::slice::from_ref(ca)),
        host: host.to_owned(),
    };
    client::connect_stream(config(), tcp, verifier)
        .await
        .with_context(|| format!("SSH-соединение с {address} не установлено"))
}

/// Открывает канал `session` с подсистемой `subsystem` и запускает поверх него мультиплексор.
pub(super) async fn open_mux(handle: &Handle<HostVerifier>, subsystem: &str) -> anyhow::Result<(Mux, Requests)> {
    let mut channel = handle.channel_open_session().await.context("собеседник не открыл канал")?;
    channel
        .request_subsystem(true, subsystem)
        .await
        .with_context(|| format!("запрос подсистемы {subsystem} не отправлен"))?;
    // Отказ приходит сообщением канала; без этого ожидания узел ждал бы приветствие от собеседника, который его не шлёт.
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) | None => bail!("собеседник не открыл подсистему {subsystem}"),
            Some(_) => {}
        }
    }
    Ok(Mux::new(channel.into_stream()))
}

/// Канал данных с сессией: держит оба SSH-соединения.
pub struct Connection {
    pub mux: Mux,
    pub requests: Requests,
    _link: Link,
}

/// Куда и как подключаться.
#[derive(Debug)]
pub struct Target<'a> {
    /// Адрес SSH orbit server, `хост:порт`.
    pub jump: &'a str,
    /// Имя, которое jump носит в своём хост-сертификате.
    pub jump_host: &'a str,
    pub ca: &'a PublicKey,
}

/// Два SSH-соединения: на jump и внутри его `direct-tcpip` — к сессии. Пока значение живёт, живы оба.
pub struct Link {
    pub session: Handle<HostVerifier>,
    _jump: Handle<HostVerifier>,
}

/// Входит на jump сертификатом `cert`, через `direct-tcpip` на `destination` — в сессию под именем `login`.
/// Сертификат предъявляется на обоих слоях, хост-сертификат собеседника проверяется на обоих.
pub async fn link(target: &Target<'_>, key: &PrivateKey, cert: &Certificate, destination: &str, login: &str) -> anyhow::Result<Link> {
    let key = Arc::new(key.clone());

    let mut jump = dial(target.jump, target.jump_host, target.ca).await?;
    if !jump
        .authenticate_openssh_cert(JUMP_LOGIN, Arc::clone(&key), cert.clone())
        .await?
        .success()
    {
        bail!("jump отклонил сертификат");
    }
    let tunnel = jump
        .channel_open_direct_tcpip(destination, 22, "127.0.0.1", 0)
        .await
        .context("jump не провёл к сессии: сессии нет, она недоступна или ей не назначен runner")?;

    let session_verifier = HostVerifier {
        trust: Trust::new(std::slice::from_ref(target.ca)),
        host: DATA_HOST.to_owned(),
    };
    let mut session = client::connect_stream(config(), tunnel.into_stream(), session_verifier)
        .await
        .context("SSH-соединение с сессией не установлено")?;
    if !session.authenticate_openssh_cert(login, key, cert.clone()).await?.success() {
        bail!("сессия отклонила сертификат: проверьте принципал {login} и права сертификата");
    }
    Ok(Link { session, _jump: jump })
}

/// Подключается к назначенной сессии каналом данных.
pub async fn connect(target: &Target<'_>, key: &PrivateKey, cert: &Certificate) -> anyhow::Result<Connection> {
    let link = link(target, key, cert, DATA_HOST, DATA_LOGIN).await?;
    let (mux, requests) = open_mux(&link.session, DATA_SUBSYSTEM).await?;
    Ok(Connection {
        mux,
        requests,
        _link: link,
    })
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link").finish_non_exhaustive()
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}
