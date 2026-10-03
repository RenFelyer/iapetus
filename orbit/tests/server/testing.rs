//! Общая обстановка тестов server: настоящая SQLite и центр сертификации во временном каталоге,
//! `orbit_key` системы и runner, зарегистрированные так же, как это делает waldo.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use nerve::fingerprint::Fingerprint;
use nerve::mux::Mux;
use nerve::protocol::{Error, RENEW_NAMESPACE, RegisterRequest, RenewRequest, RunnerHost, RunnerRequest};
use russh::keys::ssh_key::{Algorithm, HashAlg, SshSig};
use russh::keys::{Certificate, PrivateKey};
use tempfile::TempDir;

use super::enroll;
use super::registry::{OrbitKey, Registry, TokenCipher};
use super::ssh::ca::CertificateAuthority;

/// Окружение runner, которое тестовые runner сообщают server.
pub(super) fn runner_host() -> RunnerHost {
    RunnerHost {
        project: "/srv/project".to_owned(),
        hostname: "runner-host".to_owned(),
        os_release: "ID=test\n".to_owned(),
        home: "/home/runner".to_owned(),
        home_origin: String::new(),
        home_served: false,
    }
}

/// Runner сообщает server своё окружение, как первый запрос соединения; server ставит его в сеть.
pub(super) async fn report_host(mux: &Mux) {
    let reply: Result<(), Error> = tokio::time::timeout(
        Duration::from_secs(5),
        nerve::connect::server::ask(mux, &RunnerRequest::Host(runner_host())),
    )
    .await
    .expect("server не ответил на окружение runner")
    .expect("ответ на окружение runner не получен");
    assert_eq!(reply, Ok(()), "server не принял окружение runner");
}

pub(super) fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

pub(super) fn random_fingerprint() -> Fingerprint {
    use std::str::FromStr;
    let bytes: [u8; 16] = rand::random();
    let text = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat();
    Fingerprint::from_str(&text).unwrap()
}

/// Запрос продления с `orbit_key`, подписанный `key` так, как подписывает узел.
pub(super) fn signed_renewal(key: &PrivateKey, fingerprint: &Fingerprint, timestamp: u64, orbit_key: &OrbitKey) -> RenewRequest {
    let payload = RenewRequest::payload(fingerprint, timestamp);
    let signature = SshSig::sign(key, RENEW_NAMESPACE, HashAlg::Sha256, &payload).unwrap();
    RenewRequest {
        orbit_key: orbit_key.expose().to_owned(),
        fingerprint: fingerprint.clone(),
        timestamp,
        signature: signature.to_pem(russh::keys::ssh_key::LineEnding::LF).unwrap().into_bytes(),
    }
}

pub(super) fn unix(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

/// Зарегистрированный узел: его закрытый ключ и последний выданный сертификат.
pub(super) struct TestNode {
    pub(super) fingerprint: Fingerprint,
    pub(super) key: PrivateKey,
    pub(super) certificate: Certificate,
}

pub(super) struct Env {
    pub(super) dir: TempDir,
    /// Общий с jump в тестах SSH.
    pub(super) registry: Arc<Mutex<Registry>>,
    pub(super) ca: Arc<CertificateAuthority>,
    pub(super) orbit_key: OrbitKey,
}

impl Env {
    pub(super) fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let mut registry = Registry::open(&dir.path().join("registry.db"), TokenCipher::from_key([7; 32])).unwrap();
        let ca = Arc::new(CertificateAuthority::load_or_create(&dir.path().join("ca")).unwrap());
        let orbit_key = registry.issue_orbit_key(unix(SystemTime::now()).cast_signed()).unwrap();
        Self {
            dir,
            registry: Arc::new(Mutex::new(registry)),
            ca,
            orbit_key,
        }
    }

    pub(super) fn reg(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap()
    }

    /// Заменяет `orbit_key` системы новым, выданным в момент `now`; прежний перестаёт действовать.
    pub(super) fn rotate_orbit_key(&mut self, now: SystemTime) -> OrbitKey {
        let new = self.reg().issue_orbit_key(unix(now).cast_signed()).unwrap();
        std::mem::replace(&mut self.orbit_key, new)
    }

    /// Продление с действующим `orbit_key`.
    pub(super) fn renewal(&self, node: &TestNode, timestamp: u64) -> RenewRequest {
        signed_renewal(&node.key, &node.fingerprint, timestamp, &self.orbit_key)
    }

    pub(super) fn request(&self, name: &str, fingerprint: &Fingerprint, key: &PrivateKey) -> RegisterRequest {
        RegisterRequest {
            orbit_key: self.orbit_key.expose().to_owned(),
            fingerprint: fingerprint.clone(),
            public_key: key.public_key().to_openssh().unwrap(),
            name: name.to_owned(),
            os: "linux".to_owned(),
            parent: None,
        }
    }

    /// Регистрирует новый runner в момент `now`.
    pub(super) fn enroll_at(&self, name: &str, now: SystemTime) -> TestNode {
        let (fingerprint, key) = (random_fingerprint(), random_key());
        let request = self.request(name, &fingerprint, &key);
        let reply = enroll::register(&mut self.reg(), &self.ca, &request, now).unwrap();
        TestNode {
            fingerprint,
            key,
            certificate: Certificate::from_openssh(&reply.certificate).unwrap(),
        }
    }

    pub(super) fn enroll(&self, name: &str) -> TestNode {
        self.enroll_at(name, SystemTime::now())
    }
}

// ---------------------------------------------------------------- server на 127.0.0.1

use std::net::SocketAddr;
use std::time::Duration;

use nerve::connect::server::Server;
use nerve::protocol::Author;
use nerve::ssh::Trust;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::control::Control;
use super::ssh::ca::Validity;
use super::ssh::routes::registry_routes;
use super::ssh::{Gate, Jump, Side};

/// Имя, под которым клиенты подключаются к server: принципал его хост-сертификата.
pub(super) const HOST: &str = "orbit-server";

/// Адреса серверов данных, куда jump ведёт сессии: сессия без записи адреса не имеет.
pub(super) type Addrs = Arc<Mutex<Vec<(Fingerprint, SocketAddr)>>>;

/// Сервер «сессии» для jump: при подключении называет себя меткой, затем возвращает всё, что получит.
pub(super) async fn echo_server(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                if stream.write_all(tag.as_bytes()).await.is_err() {
                    return;
                }
                let mut buf = [0u8; 256];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Настоящий SSH-сервер server на 127.0.0.1: внешний и внутренний слушатели, jump по реестру, канал управления.
pub(super) struct Stand {
    pub(super) env: Env,
    pub(super) control: Arc<Control>,
    pub(super) external: SocketAddr,
    pub(super) internal: SocketAddr,
    pub(super) data: Addrs,
}

impl Stand {
    pub(super) async fn start() -> Self {
        let env = Env::new();
        let control = Arc::new(Control::new(
            Arc::clone(&env.registry),
            Arc::clone(&env.ca),
            None,
            Author {
                name: "Iapetus".to_owned(),
                email: "iapetus@localhost".to_owned(),
            },
            env.dir.path().join("docs"),
        ));
        let data: Addrs = Arc::default();
        let routes = {
            let data = Arc::clone(&data);
            let agent = Arc::clone(&control);
            registry_routes(
                Arc::clone(&env.registry),
                move |session| data.lock().unwrap().iter().find(|(fp, _)| fp == session).map(|(_, addr)| *addr),
                move |session| agent.agent_running(session),
            )
        };
        let jump = Arc::new(Jump::new(Arc::clone(&control), routes));
        let trust = Arc::new(Trust::new(&[env.ca.public_key()]));
        let host_key = random_key();
        let host_cert = env
            .ca
            .issue_host(
                host_key.public_key(),
                "orbit-server",
                &[HOST],
                Validity::from_now(Duration::from_hours(1)),
            )
            .unwrap();
        let mut addrs = Vec::new();
        for side in [Side::External, Side::Internal] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            let mut config = super::ssh::config(host_key.clone(), host_cert.clone(), side);
            // Отказ в тестах не ждёт секунду, как на боевом сервере.
            config.auth_rejection_time = Duration::from_millis(1);
            let gate = Gate::new(side, Arc::clone(&control), Arc::clone(&trust), Arc::clone(&jump));
            tokio::spawn(super::ssh::serve(gate, listener, config));
        }
        Self {
            env,
            control,
            external: addrs[0],
            internal: addrs[1],
            data,
        }
    }

    /// Куда и кому доверять клиенту `nerve`.
    pub(super) fn server(&self, side: Side) -> Server {
        let address = match side {
            Side::External => self.external,
            Side::Internal => self.internal,
        };
        Server {
            address: address.to_string(),
            host: HOST.to_owned(),
            ca: self.env.ca.public_key(),
        }
    }

    /// Сессия в реестре с секретом для входа.
    pub(super) fn session(&self) -> (Fingerprint, String) {
        let mut registry = self.env.reg();
        let session = registry.create_session(nerve::protocol::Agent::ClaudeCode).unwrap();
        let secret = registry.issue_session_secret(&session).unwrap();
        (session, secret.expose().to_owned())
    }

    /// Выдаёт новый `orbit_key` позже всех уже выданных сертификатов; возвращает его секрет.
    pub(super) fn new_orbit_key(&self) -> String {
        let later = unix(SystemTime::now() + Duration::from_secs(2));
        self.env.reg().issue_orbit_key(later.cast_signed()).unwrap().expose().to_owned()
    }

    /// Ключ и сертификат worker: выдан server по действующему `orbit_key`.
    pub(super) fn worker(&self, name: &str) -> (PrivateKey, Certificate) {
        let key = random_key();
        let request = nerve::protocol::WorkerCertificateRequest {
            orbit_key: self.env.orbit_key.expose().to_owned(),
            public_key: key.public_key().to_openssh().unwrap(),
            name: name.to_owned(),
        };
        let reply = enroll::worker_certificate(&self.env.reg(), &self.env.ca, &request, SystemTime::now()).unwrap();
        (key, Certificate::from_openssh(&reply.certificate).unwrap())
    }
}
