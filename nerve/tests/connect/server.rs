//! Вход на server: пароль `orbit_key` уходит только после проверки хост-сертификата по CA, а чужая версия
//! протокола закрывает канал. Server — настоящий SSH-сервер `russh` на `127.0.0.1:0`.

use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

use russh::keys::ssh_key::Algorithm;
use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Server as _, Session};
use russh::{Channel as SshChannel, ChannelId, MethodKind};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::*;
use crate::protocol::VERSION;

const HOST: &str = "orbit.test";
const ORBIT_KEY: &str = "orbit-key-secret";

fn key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

/// Хост-сертификат на `host_key`, подписанный `ca`, для имени `principal`.
fn host_certificate(ca: &PrivateKey, host_key: &PrivateKey, principal: &str) -> Certificate {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut builder = Builder::new([1u8; 16], host_key.public_key().key_data().clone(), now - 60, now + 600).unwrap();
    builder.cert_type(CertType::Host).unwrap();
    builder.key_id("orbit").unwrap();
    builder.valid_principal(principal).unwrap();
    builder.sign(ca).unwrap()
}

/// Что server предъявляет клиенту как хост.
enum HostIdentity {
    /// Голый ключ без сертификата.
    BareKey,
    /// Сертификат, подписанный указанным CA, для указанного имени.
    Certificate {
        signed_by: Box<PrivateKey>,
        principal: &'static str,
    },
}

/// Что server видел от клиента.
#[derive(Debug, Default)]
struct Seen {
    passwords: Vec<(String, String)>,
}

struct Listener {
    seen: Arc<Mutex<Seen>>,
    /// Версия протокола в первом кадре server; равная [`VERSION`] — обычный `Mux`.
    hello_version: u16,
}

impl russh::server::Server for Listener {
    type Handler = Connection;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Connection {
        Connection {
            seen: Arc::clone(&self.seen),
            hello_version: self.hello_version,
            channel: None,
        }
    }
}

struct Connection {
    seen: Arc<Mutex<Seen>>,
    hello_version: u16,
    channel: Option<SshChannel<Msg>>,
}

impl Handler for Connection {
    type Error = anyhow::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .passwords
            .push((user.to_owned(), password.to_owned()));
        if user == ENROLL_LOGIN && password == ORBIT_KEY {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: SshChannel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        let Some(opened) = self.channel.take_if(|opened| opened.id() == channel && name == CONTROL_SUBSYSTEM) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let mut stream = opened.into_stream();
        if self.hello_version == VERSION {
            let (mux, mut requests) = Mux::new(stream);
            tokio::spawn(async move {
                while let Some(request) = requests.recv().await {
                    drop(request.respond(request.body().clone(), true).await);
                }
                drop(mux);
            });
            return Ok(());
        }
        // Первый кадр `Mux`: длина тела, id 0, флаги 0, версия.
        let mut hello = vec![0, 0, 0, 2, 0, 0, 0, 0, 0];
        hello.extend_from_slice(&self.hello_version.to_be_bytes());
        tokio::spawn(async move {
            drop(stream.write_all(&hello).await);
            // Поток остаётся открытым: канал должен закрыть клиент, а не обрыв.
            std::future::pending::<()>().await;
        });
        Ok(())
    }
}

struct Fixture {
    server: Server,
    seen: Arc<Mutex<Seen>>,
}

impl Fixture {
    fn passwords(&self) -> Vec<(String, String)> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).passwords.clone()
    }
}

/// Запускает server с `identity`; клиент доверяет CA `trusted` и ждёт имя `HOST`.
async fn start(identity: HostIdentity, trusted: &PrivateKey, hello_version: u16) -> Fixture {
    let host_key = key();
    let certificates = match identity {
        HostIdentity::BareKey => vec![],
        HostIdentity::Certificate { signed_by, principal } => vec![host_certificate(&signed_by, &host_key, principal)],
    };
    let config = Config {
        keys: vec![host_key],
        certificates,
        methods: [MethodKind::Password][..].into(),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Config::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut server = Listener {
        seen: Arc::clone(&seen),
        hello_version,
    };
    tokio::spawn(async move { server.run_on_socket(Arc::new(config), &listener).await });
    Fixture {
        server: Server {
            address,
            host: HOST.to_owned(),
            ca: trusted.public_key().clone(),
        },
        seen,
    }
}

#[track_caller]
fn assert_refused_without_password(result: &Result<Channel, ConnectError>, fixture: &Fixture, why: &str) {
    assert!(result.is_err(), "вход состоялся, хотя {why}");
    assert_eq!(fixture.passwords(), vec![], "пароль orbit_key ушёл на server, хотя {why}");
}

#[tokio::test]
async fn password_is_not_sent_to_a_server_with_a_bare_host_key() {
    let ca = key();
    let fixture = start(HostIdentity::BareKey, &ca, VERSION).await;
    let result = open(&fixture.server, &Login::Enroll { orbit_key: ORBIT_KEY }).await;
    assert_refused_without_password(&result, &fixture, "server предъявил голый ключ без хост-сертификата");
}

#[tokio::test]
async fn password_is_not_sent_to_a_server_certified_by_a_foreign_ca() {
    let ca = key();
    let identity = HostIdentity::Certificate {
        signed_by: Box::new(key()),
        principal: HOST,
    };
    let fixture = start(identity, &ca, VERSION).await;
    let result = open(&fixture.server, &Login::Enroll { orbit_key: ORBIT_KEY }).await;
    assert_refused_without_password(&result, &fixture, "хост-сертификат подписан не нашим CA");
}

#[tokio::test]
async fn password_is_not_sent_to_a_server_certified_for_another_host_name() {
    let ca = key();
    let identity = HostIdentity::Certificate {
        signed_by: Box::new(ca.clone()),
        principal: "other.test",
    };
    let fixture = start(identity, &ca, VERSION).await;
    let result = open(&fixture.server, &Login::Enroll { orbit_key: ORBIT_KEY }).await;
    assert_refused_without_password(&result, &fixture, "в хост-сертификате нет имени, под которым подключились");
}

/// Контроль для тестов выше: с верным хост-сертификатом тот же вход проходит, пароль доходит, канал отвечает.
#[tokio::test]
async fn password_reaches_a_server_with_a_valid_host_certificate() {
    let ca = key();
    let identity = HostIdentity::Certificate {
        signed_by: Box::new(ca.clone()),
        principal: HOST,
    };
    let fixture = start(identity, &ca, VERSION).await;
    let channel = open(&fixture.server, &Login::Enroll { orbit_key: ORBIT_KEY }).await.unwrap();
    assert_eq!(fixture.passwords(), vec![(ENROLL_LOGIN.to_owned(), ORBIT_KEY.to_owned())]);
    let echo: String = ask(&channel.mux, &"ping".to_owned()).await.unwrap();
    assert_eq!(echo, "ping", "канал управления не отвечает");
}

#[tokio::test]
async fn channel_is_closed_when_the_server_speaks_another_protocol_version() {
    let ca = key();
    let identity = HostIdentity::Certificate {
        signed_by: Box::new(ca.clone()),
        principal: HOST,
    };
    let other = VERSION + 1;
    let fixture = start(identity, &ca, other).await;
    let channel = open(&fixture.server, &Login::Enroll { orbit_key: ORBIT_KEY }).await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(10), channel.mux.closed())
        .await
        .expect("канал не закрылся при чужой версии протокола");
    assert_eq!(
        reason,
        Closed::Version(Some(other)),
        "причина закрытия не называет версию собеседника"
    );
    assert!(
        ask::<_, String>(&channel.mux, &"ping".to_owned()).await.is_err(),
        "запрос прошёл по каналу с чужой версией протокола"
    );
}
