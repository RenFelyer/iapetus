//! Вход на server через настоящий SSH на 127.0.0.1 и настоящую SQLite: runner регистрируется, получает
//! сертификат и входит клиентом `russh`. Каждое условие входа нарушается отдельным тестом, остальные при этом
//! верны. Здесь же — jump и центр сертификации; запросы канала управления — в `control`.

use std::borrow::Cow;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve::connect::server::{ConnectError, Login as ClientLogin, open};
use nerve::fingerprint::Fingerprint;
use nerve::ssh::{CLOCK_SKEW, CONTROL_LOGIN, CONTROL_SUBSYSTEM, DATA_LOGIN, ENROLL_LOGIN, JUMP_LOGIN, session_login};
use russh::Preferred;
use russh::client::{self, AuthResult, Handle};
use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::ssh_key::{HashAlg, PrivateKey};
use russh::keys::{Algorithm, Certificate, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::Side;
use super::ca::{CertificateAuthority, Validity};
use crate::server::testing::{HOST, Stand, TestNode, echo_server, random_key};

const HOUR: Duration = Duration::from_secs(3600);

fn ca_in(dir: &tempfile::TempDir) -> CertificateAuthority {
    CertificateAuthority::load_or_create(&dir.path().join("ca")).unwrap()
}

fn short() -> Validity {
    Validity::from_now(Duration::from_secs(60))
}

fn unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Клиент принимает только хост-сертификат нашего CA с именем server.
struct HostCertClient {
    ca: PublicKey,
}

impl client::Handler for HostCertClient {
    type Error = russh::Error;

    #[expect(clippy::unused_async_trait_impl, reason = "сигнатура задана трейтом Handler")]
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::Certificate(cert) = key else {
            return Ok(false);
        };
        let trusted = [self.ca.fingerprint(HashAlg::Sha256)];
        Ok(cert.cert_type() == CertType::Host
            && cert.validate_at(unix(SystemTime::now()), &trusted).is_ok()
            && cert.valid_principals().iter().any(|principal| principal == HOST))
    }
}

/// Стенд с двумя сессиями: их серверы данных отзываются метками `S1` и `S2`.
struct JumpStand {
    stand: Stand,
    s1: Fingerprint,
    s2: Fingerprint,
}

async fn jump_stand() -> JumpStand {
    let stand = Stand::start().await;
    let (s1, s2) = {
        let mut registry = stand.env.reg();
        let mut session = || registry.create_session(nerve::protocol::Agent::ClaudeCode).unwrap();
        (session(), session())
    };
    let (first, second) = (echo_server("S1").await, echo_server("S2").await);
    stand.data.lock().unwrap().extend([(s1.clone(), first), (s2.clone(), second)]);
    JumpStand { stand, s1, s2 }
}

impl Stand {
    async fn raw(&self, side: Side) -> Handle<HostCertClient> {
        let addr = match side {
            Side::External => self.external,
            Side::Internal => self.internal,
        };
        // Без явного согласия клиента сервер предъявил бы голый ключ вместо хост-сертификата.
        let preferred = Preferred {
            host_key_certificates: Cow::Borrowed(&[Algorithm::Ed25519]),
            ..Preferred::DEFAULT
        };
        let config = client::Config {
            preferred,
            ..client::Config::default()
        };
        client::connect(
            Arc::new(config),
            addr,
            HostCertClient {
                ca: self.env.ca.public_key(),
            },
        )
        .await
        .unwrap()
    }

    /// Вход сертификатом на внешний слушатель; `Some`, если server его принял.
    async fn login(&self, login: &str, key: &PrivateKey, cert: Certificate) -> Option<Handle<HostCertClient>> {
        let mut handle = self.raw(Side::External).await;
        let result = handle.authenticate_openssh_cert(login, Arc::new(key.clone()), cert).await.unwrap();
        matches!(result, AuthResult::Success).then_some(handle)
    }

    /// Вход по паролю; `Some`, если server его принял.
    async fn password(&self, side: Side, login: &str, password: &str) -> Option<Handle<HostCertClient>> {
        let mut handle = self.raw(side).await;
        let result = handle.authenticate_password(login, password).await.unwrap();
        matches!(result, AuthResult::Success).then_some(handle)
    }

    async fn jump_as(&self, node: &TestNode) -> Handle<HostCertClient> {
        self.login(JUMP_LOGIN, &node.key, node.certificate.clone())
            .await
            .expect("сертификат зарегистрированного runner принят")
    }

    async fn assert_login_rejected(&self, login: &str, key: &PrivateKey, cert: Certificate, why: &str) {
        assert!(self.login(login, key, cert).await.is_none(), "server принял сертификат: {why}");
    }

    /// Вход worker на jump: ключ на этот запуск и сертификат, выданный server по действующему `orbit_key`.
    async fn jump_worker(&self, name: &str) -> Handle<HostCertClient> {
        let (key, cert) = self.worker(name);
        self.login(JUMP_LOGIN, &key, cert).await.expect("сертификат worker не принят")
    }

    fn assign(&self, session: &Fingerprint, runner: &TestNode) {
        self.env.reg().assign_runner(session, &runner.fingerprint).unwrap();
    }

    /// Сообщает server, что в session запущен агент: канал управления session и `Hello` с состоянием `success`.
    async fn wake_agent(&self, session: &Fingerprint, secret: &str) -> nerve::connect::server::Channel {
        let channel = open(&self.server(Side::Internal), &ClientLogin::Session { session, secret })
            .await
            .expect("session вошла на внутренний слушатель");
        let hello = nerve::protocol::SessionRequest::Hello(nerve::protocol::Status {
            state: nerve::session_state::SessionState::Success,
            reason: None,
            cause: None,
            limit_reset: None,
            runner: None,
            agent_running: true,
            activity: nerve::protocol::Activity::default(),
            limits: Vec::new(),
            restart_pending: false,
        });
        let answer: Result<(), nerve::protocol::Error> = nerve::connect::server::ask(&channel.mux, &hello).await.unwrap();
        answer.expect("Hello принят");
        channel
    }
}

/// Открывает туннель и читает метку сессии, в которую он привёл.
async fn tunnel_tag(handle: &Handle<HostCertClient>, host: &str) -> Option<String> {
    let channel = handle.channel_open_direct_tcpip(host, 22, "127.0.0.1", 0).await.ok()?;
    let mut stream = channel.into_stream();
    let mut tag = [0u8; 2];
    stream.read_exact(&mut tag).await.ok()?;
    stream.write_all(b"ping").await.unwrap();
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"ping", "туннель возвращает отправленное");
    Some(String::from_utf8(tag.to_vec()).unwrap())
}

async fn assert_lands_in(handle: &Handle<HostCertClient>, host: &str, tag: &str) {
    assert_eq!(
        tunnel_tag(handle, host).await.as_deref(),
        Some(tag),
        "туннель с хостом {host:?} привёл не в {tag}"
    );
}

async fn assert_tunnel_refused(handle: &Handle<HostCertClient>, host: &str, why: &str) {
    let tag = tunnel_tag(handle, host).await;
    assert!(tag.is_none(), "jump открыл туннель к {host:?} (сессия {tag:?}): {why}");
}

// ---------------------------------------------------------------- iapetus-enroll

#[tokio::test]
async fn enroll_login_with_a_wrong_orbit_key_is_rejected() {
    let stand = Stand::start().await;
    assert!(
        stand.password(Side::External, ENROLL_LOGIN, "не тот ключ").await.is_none(),
        "iapetus-enroll принял неверный orbit_key"
    );
}

#[tokio::test]
async fn enroll_login_with_a_replaced_orbit_key_is_rejected() {
    let stand = Stand::start().await;
    let old = stand.env.orbit_key.expose().to_owned();
    stand.new_orbit_key();
    assert!(
        stand.password(Side::External, ENROLL_LOGIN, &old).await.is_none(),
        "iapetus-enroll принял прежний orbit_key"
    );
}

#[tokio::test]
async fn enroll_login_with_the_current_orbit_key_is_accepted() {
    let stand = Stand::start().await;
    let key = stand.env.orbit_key.expose().to_owned();
    assert!(
        stand.password(Side::External, ENROLL_LOGIN, &key).await.is_some(),
        "iapetus-enroll не принял действующий orbit_key"
    );
}

#[tokio::test]
async fn enroll_login_is_not_accepted_on_the_internal_listener() {
    let stand = Stand::start().await;
    let key = stand.env.orbit_key.expose().to_owned();
    assert!(
        stand.password(Side::Internal, ENROLL_LOGIN, &key).await.is_none(),
        "внутренний слушатель принял вход узла"
    );
}

#[tokio::test]
async fn orbit_key_does_not_open_the_control_or_jump_logins() {
    let stand = Stand::start().await;
    let key = stand.env.orbit_key.expose().to_owned();
    for login in [CONTROL_LOGIN, JUMP_LOGIN, DATA_LOGIN, "root"] {
        assert!(
            stand.password(Side::External, login, &key).await.is_none(),
            "orbit_key открыл вход {login}: он годится только для iapetus-enroll"
        );
    }
}

// ---------------------------------------------------------------- session-<fp>

#[tokio::test]
async fn session_login_with_its_secret_is_accepted() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    assert!(
        stand.password(Side::Internal, &session_login(&session), &secret).await.is_some(),
        "внутренний слушатель не принял session с её секретом"
    );
}

#[tokio::test]
async fn session_login_with_a_wrong_secret_is_rejected() {
    let stand = Stand::start().await;
    let (session, _) = stand.session();
    assert!(
        stand
            .password(Side::Internal, &session_login(&session), "чужой секрет")
            .await
            .is_none(),
        "session принята с неверным секретом"
    );
}

#[tokio::test]
async fn session_login_with_the_secret_of_another_session_is_rejected() {
    let stand = Stand::start().await;
    let (session, _) = stand.session();
    let (_, other_secret) = stand.session();
    assert!(
        stand
            .password(Side::Internal, &session_login(&session), &other_secret)
            .await
            .is_none(),
        "session принята с секретом другой session"
    );
}

#[tokio::test]
async fn session_login_for_a_session_without_a_secret_is_rejected() {
    let stand = Stand::start().await;
    let session = stand.env.reg().create_session(nerve::protocol::Agent::ClaudeCode).unwrap();
    assert!(
        stand.password(Side::Internal, &session_login(&session), "").await.is_none(),
        "session без выданного секрета вошла с пустым паролем"
    );
}

#[tokio::test]
async fn session_login_for_an_unknown_session_is_rejected() {
    let stand = Stand::start().await;
    let (_, secret) = stand.session();
    let ghost = crate::server::testing::random_fingerprint();
    assert!(
        stand.password(Side::Internal, &session_login(&ghost), &secret).await.is_none(),
        "вход неизвестной session принят"
    );
}

#[tokio::test]
async fn session_login_on_the_external_listener_is_rejected() {
    let stand = Stand::start().await;
    let (session, secret) = stand.session();
    assert!(
        stand.password(Side::External, &session_login(&session), &secret).await.is_none(),
        "внешний слушатель принял вход session: она недоступна из сети пользователя"
    );
}

#[tokio::test]
async fn internal_listener_accepts_only_session_logins() {
    let stand = Stand::start().await;
    let (_, secret) = stand.session();
    for login in [CONTROL_LOGIN, JUMP_LOGIN, "root"] {
        assert!(
            stand.password(Side::Internal, login, &secret).await.is_none(),
            "внутренний слушатель принял вход {login}"
        );
    }
}

#[tokio::test]
async fn certificate_login_is_not_accepted_on_the_internal_listener() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let mut handle = stand.raw(Side::Internal).await;
    let result = handle
        .authenticate_openssh_cert(CONTROL_LOGIN, Arc::new(node.key.clone()), node.certificate.clone())
        .await
        .unwrap();
    assert!(
        !matches!(result, AuthResult::Success),
        "внутренний слушатель принял сертификат runner"
    );
}

// ---------------------------------------------------------------- вход по сертификату

#[tokio::test]
async fn certificate_from_another_ca_is_rejected() {
    let stand = Stand::start().await;
    let other_dir = tempfile::tempdir().unwrap();
    let other_ca = ca_in(&other_dir);
    let node = stand.env.enroll("r");
    let cert = other_ca
        .issue_node(node.key.public_key(), node.fingerprint.as_str(), short())
        .unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "подпись чужого CA")
        .await;
}

#[tokio::test]
async fn expired_certificate_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll_at("r", SystemTime::now() - HOUR);
    stand
        .assert_login_rejected(
            CONTROL_LOGIN,
            &node.key,
            node.certificate,
            "сертификат выпущен час назад на 15 минут",
        )
        .await;
}

#[tokio::test]
async fn certificate_not_yet_valid_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll_at("r", SystemTime::now() + HOUR);
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, node.certificate, "срок наступит через час")
        .await;
}

#[tokio::test]
async fn host_certificate_is_rejected_as_a_login() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let cert = stand
        .env
        .ca
        .issue_host(node.key.public_key(), node.fingerprint.as_str(), &[CONTROL_LOGIN], short())
        .unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "тип host вместо user")
        .await;
}

#[tokio::test]
async fn certificate_without_the_login_principal_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let cert = stand
        .env
        .ca
        .issue_user(node.key.public_key(), node.fingerprint.as_str(), &["someone"], short())
        .unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "имени входа нет среди принципалов")
        .await;
}

#[tokio::test]
async fn certificate_for_jump_only_does_not_open_the_control_channel() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let cert = stand
        .env
        .ca
        .issue_user(node.key.public_key(), node.fingerprint.as_str(), &[JUMP_LOGIN], short())
        .unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "в принципалах только iapetus-jump")
        .await;
}

#[tokio::test]
async fn login_under_a_name_missing_from_principals_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand
        .assert_login_rejected("root", &node.key, node.certificate, "root нет в принципалах, и это не логин server")
        .await;
}

#[tokio::test]
async fn data_login_does_not_open_server() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand
        .assert_login_rejected(
            DATA_LOGIN,
            &node.key,
            node.certificate,
            "iapetus-data — вход в session, не на server",
        )
        .await;
}

#[tokio::test]
async fn certificate_with_too_long_lifetime_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let cert = stand
        .env
        .ca
        .issue_node(node.key.public_key(), node.fingerprint.as_str(), Validity::from_now(HOUR))
        .unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "срок жизни час при пределе 15 минут")
        .await;
}

#[tokio::test]
async fn certificate_with_critical_options_is_rejected() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let cert = custom_cert(
        &raw_ca_key(&stand.env.dir),
        node.key.public_key(),
        node.fingerprint.as_str(),
        |builder| {
            builder.critical_option("force-command", "/bin/true").unwrap();
        },
    );
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, cert, "есть critical option")
        .await;
}

#[tokio::test]
async fn certificate_whose_key_id_is_not_a_fingerprint_is_rejected() {
    let stand = Stand::start().await;
    let key = random_key();
    let cert = custom_cert(&raw_ca_key(&stand.env.dir), key.public_key(), "не fingerprint!", |_| {});
    stand
        .assert_login_rejected(CONTROL_LOGIN, &key, cert, "key_id не разбирается как fingerprint")
        .await;
}

#[tokio::test]
async fn certificate_of_an_unregistered_fingerprint_is_rejected() {
    let stand = Stand::start().await;
    let key = random_key();
    let stranger = crate::server::testing::random_fingerprint();
    let cert = stand.env.ca.issue_node(key.public_key(), stranger.as_str(), short()).unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &key, cert, "действительная подпись CA, но узла нет в реестре")
        .await;
}

#[tokio::test]
async fn certificate_of_a_revoked_runner_is_rejected_although_still_valid() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand.env.reg().revoke_node(&node.fingerprint).unwrap();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &node.key, node.certificate, "узел отозван, сертификат ещё не истёк")
        .await;
}

#[tokio::test]
async fn plain_key_without_certificate_is_rejected() {
    let stand = Stand::start().await;
    let key = random_key();
    let mut handle = stand.raw(Side::External).await;
    let result = handle
        .authenticate_publickey(CONTROL_LOGIN, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .unwrap();
    assert!(!matches!(result, AuthResult::Success), "server принял обычный ключ без сертификата");
}

#[tokio::test]
async fn certificate_does_not_work_with_the_key_of_another() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand
        .assert_login_rejected(CONTROL_LOGIN, &random_key(), node.certificate, "сертификат предъявлен чужим ключом")
        .await;
}

#[tokio::test]
async fn worker_certificate_issued_before_the_new_orbit_key_is_rejected() {
    let stand = Stand::start().await;
    let (key, cert) = stand.worker("alice");
    stand.new_orbit_key();
    stand
        .assert_login_rejected(CONTROL_LOGIN, &key, cert, "сертификат выдан по прежнему orbit_key; срок не истёк")
        .await;
}

#[tokio::test]
async fn runner_enters_with_the_certificate_it_got_at_registration() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let certificate = &node.certificate;
    open(
        &stand.server(Side::External),
        &ClientLogin::Certificate {
            key: &node.key,
            certificate,
        },
    )
    .await
    .expect("runner с действующим сертификатом не вошёл");
}

#[tokio::test]
async fn certificate_login_of_an_unknown_server_host_does_not_send_credentials() {
    // Клиент nerve доверяет хост-сертификату только с именем из orbit_url: server с другим именем — отказ до пароля.
    let stand = Stand::start().await;
    let mut server = stand.server(Side::External);
    server.host = "другой-хост".to_owned();
    let key = stand.env.orbit_key.expose();
    let result = open(&server, &ClientLogin::Enroll { orbit_key: key }).await;
    assert!(
        matches!(result, Err(ConnectError::Other(_))),
        "клиент вошёл на server с чужим именем хоста"
    );
}

// ---------------------------------------------------------------- каналы

/// Запрос канала на новом соединении с каналом управления; `true` — server его отклонил.
async fn channel_request_refused(stand: &Stand, request: impl AsyncFnOnce(&russh::Channel<client::Msg>)) -> bool {
    let node = stand.env.enroll("r");
    let handle = stand
        .login(CONTROL_LOGIN, &node.key, node.certificate.clone())
        .await
        .expect("вход принят");
    let channel = handle.channel_open_session().await.unwrap();
    request(&channel).await;
    tokio::time::timeout(Duration::from_secs(5), wait_failure(channel)).await == Ok(true)
}

#[tokio::test]
async fn control_login_gets_no_exec() {
    let stand = Stand::start().await;
    assert!(
        channel_request_refused(&stand, async |channel| channel.exec(true, "id").await.unwrap()).await,
        "exec не отклонён"
    );
}

#[tokio::test]
async fn control_login_gets_no_shell() {
    let stand = Stand::start().await;
    assert!(
        channel_request_refused(&stand, async |channel| channel.request_shell(true).await.unwrap()).await,
        "shell не отклонён"
    );
}

#[tokio::test]
async fn control_login_gets_no_pty() {
    let stand = Stand::start().await;
    let refused = channel_request_refused(&stand, async |channel| {
        channel.request_pty(true, "xterm", 80, 24, 0, 0, &[]).await.unwrap();
    })
    .await;
    assert!(refused, "pty не отклонён");
}

/// Ждёт ответ на запрос канала; `true` — отказ.
async fn wait_failure(mut channel: russh::Channel<client::Msg>) -> bool {
    loop {
        match channel.wait().await {
            Some(russh::ChannelMsg::Failure) => return true,
            Some(russh::ChannelMsg::Success) | None => return false,
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn unknown_subsystem_is_refused() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let handle = stand
        .login(CONTROL_LOGIN, &node.key, node.certificate.clone())
        .await
        .expect("вход принят");
    let channel = handle.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), wait_failure(channel)).await,
        Ok(true),
        "подсистема sftp открыта"
    );
}

#[tokio::test]
async fn control_login_does_not_open_tunnels() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand
        .stand
        .login(CONTROL_LOGIN, &runner.key, runner.certificate.clone())
        .await
        .expect("вход принят");
    assert_tunnel_refused(&handle, stand.s1.as_str(), "iapetus-control даёт канал управления, не jump").await;
}

#[tokio::test]
async fn second_control_channel_on_one_connection_is_refused() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    let handle = stand
        .login(CONTROL_LOGIN, &node.key, node.certificate.clone())
        .await
        .expect("вход принят");
    let first = handle.channel_open_session().await.unwrap();
    first.request_subsystem(true, CONTROL_SUBSYSTEM).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), wait_failure(first)).await,
        Ok(false),
        "первый канал управления не открыт"
    );
    assert!(
        handle.channel_open_session().await.is_err(),
        "второй канал управления на одном соединении открыт"
    );
}

// ---------------------------------------------------------------- куда ведёт jump

#[tokio::test]
async fn runner_lands_in_its_assigned_session_whatever_it_asks_for() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_as(&runner).await;

    assert_lands_in(&handle, stand.s1.as_str(), "S1").await;
    assert_lands_in(&handle, stand.s2.as_str(), "S1").await;
    assert_lands_in(&handle, "что-угодно", "S1").await;
}

#[tokio::test]
async fn runner_assigned_to_another_session_does_not_reach_the_first() {
    let stand = jump_stand().await;
    let (first, second) = (stand.stand.env.enroll("r1"), stand.stand.env.enroll("r2"));
    stand.stand.assign(&stand.s1, &first);
    stand.stand.assign(&stand.s2, &second);
    let (first, second) = (stand.stand.jump_as(&first).await, stand.stand.jump_as(&second).await);

    assert_lands_in(&second, stand.s1.as_str(), "S2").await;
    assert_lands_in(&first, stand.s2.as_str(), "S1").await;
}

#[tokio::test]
async fn runner_without_assignment_is_refused() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    let handle = stand.stand.jump_as(&runner).await;

    assert_tunnel_refused(&handle, stand.s1.as_str(), "runner не назначен ни одной сессии").await;
}

#[tokio::test]
async fn runner_follows_a_changed_assignment_and_loses_a_released_one() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_as(&runner).await;
    assert_lands_in(&handle, "x", "S1").await;

    stand.stand.env.reg().release_runner(&stand.s1).unwrap();
    assert_tunnel_refused(&handle, "x", "назначение снято").await;

    stand.stand.assign(&stand.s2, &runner);
    let handle = stand.stand.jump_as(&runner).await;
    assert_lands_in(&handle, "x", "S2").await;
}

#[tokio::test]
async fn worker_reaches_the_session_it_picked_once_the_agent_runs() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s2, &runner);
    let secret = stand.stand.env.reg().issue_session_secret(&stand.s2).unwrap();
    let _session = stand.stand.wake_agent(&stand.s2, secret.expose()).await;
    let handle = stand.stand.jump_worker("alice").await;

    assert_lands_in(&handle, stand.s2.as_str(), "S2").await;
}

#[tokio::test]
async fn worker_is_refused_and_disconnected_while_the_agent_is_not_running() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s2, &runner);
    let handle = stand.stand.jump_worker("alice").await;

    assert_tunnel_refused(&handle, stand.s2.as_str(), "runner назначен, но агент в session не запущен").await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.is_closed() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "соединение worker без права входа не закрыто");
}

#[tokio::test]
async fn worker_is_refused_while_the_session_has_no_runner_even_if_the_agent_runs() {
    let stand = jump_stand().await;
    let secret = stand.stand.env.reg().issue_session_secret(&stand.s1).unwrap();
    let _session = stand.stand.wake_agent(&stand.s1, secret.expose()).await;
    let handle = stand.stand.jump_worker("alice").await;

    assert_tunnel_refused(&handle, stand.s1.as_str(), "агент запущен, но runner ещё не назначен").await;
}

#[tokio::test]
async fn worker_asking_for_an_unknown_host_is_refused() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_worker("alice").await;

    assert_tunnel_refused(&handle, "нет-такой-сессии", "worker называет сессию сам, а её нет").await;
}

#[tokio::test]
async fn new_orbit_key_closes_worker_tunnels_and_old_worker_certificates() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let secret = stand.stand.env.reg().issue_session_secret(&stand.s1).unwrap();
    let _session = stand.stand.wake_agent(&stand.s1, secret.expose()).await;
    let worker = stand.stand.jump_worker("alice").await;
    let channel = worker
        .channel_open_direct_tcpip(stand.s1.as_str(), 22, "127.0.0.1", 0)
        .await
        .unwrap();
    let mut open = channel.into_stream();
    let mut tag = [0u8; 2];
    open.read_exact(&mut tag).await.unwrap();

    stand.stand.new_orbit_key();

    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), open.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "открытый туннель worker пережил выдачу нового orbit_key");
    assert_tunnel_refused(&worker, stand.s1.as_str(), "туннель по сертификату до нового orbit_key").await;
}

#[tokio::test]
async fn new_orbit_key_closes_runner_tunnels_and_old_runner_certificates_at_once() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_as(&runner).await;
    let channel = handle.channel_open_direct_tcpip("x", 22, "127.0.0.1", 0).await.unwrap();
    let mut open = channel.into_stream();
    let mut tag = [0u8; 2];
    open.read_exact(&mut tag).await.unwrap();

    stand.stand.new_orbit_key();

    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), open.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "открытый туннель runner пережил выдачу нового orbit_key");
    stand
        .stand
        .assert_login_rejected(
            JUMP_LOGIN,
            &runner.key,
            runner.certificate.clone(),
            "сертификат runner выдан до нового orbit_key",
        )
        .await;
}

#[tokio::test]
async fn runner_certificate_issued_before_the_new_orbit_key_does_not_open_the_control_channel() {
    let stand = Stand::start().await;
    let node = stand.env.enroll("r");
    stand.new_orbit_key();
    stand
        .assert_login_rejected(
            CONTROL_LOGIN,
            &node.key,
            node.certificate,
            "сертификат runner выдан по прежнему orbit_key",
        )
        .await;
}

async fn assert_disconnected(handle: &Handle<HostCertClient>, why: &str) {
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.is_closed() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "соединение не закрыто: {why}");
}

#[tokio::test]
async fn runner_without_assignment_loses_the_connection_when_it_asks_for_a_tunnel() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    let handle = stand.stand.jump_as(&runner).await;

    assert_tunnel_refused(&handle, stand.s1.as_str(), "runner не назначен ни одной сессии").await;
    assert_disconnected(&handle, "runner без назначения").await;
}

#[tokio::test]
async fn revoked_runner_loses_jump_although_its_certificate_is_still_valid() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_as(&runner).await;
    assert_lands_in(&handle, "x", "S1").await;

    stand.stand.env.reg().revoke_node(&runner.fingerprint).unwrap();

    assert_tunnel_refused(&handle, "x", "runner отозван, сертификат ещё не истёк").await;
}

#[tokio::test]
async fn revoking_a_runner_closes_its_open_tunnels() {
    let stand = jump_stand().await;
    let runner = stand.stand.env.enroll("r");
    stand.stand.assign(&stand.s1, &runner);
    let handle = stand.stand.jump_as(&runner).await;
    let channel = handle.channel_open_direct_tcpip("x", 22, "127.0.0.1", 0).await.unwrap();
    let mut open = channel.into_stream();
    let mut tag = [0u8; 2];
    open.read_exact(&mut tag).await.unwrap();

    stand.stand.control.revoke_runner(&runner.fingerprint).unwrap();

    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), open.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "открытый туннель отозванного runner не закрыт");
}

#[tokio::test]
async fn session_channel_is_not_opened_on_jump() {
    let stand = Stand::start().await;
    let handle = stand.jump_as(&stand.env.enroll("r")).await;
    assert!(
        handle.channel_open_session().await.is_err(),
        "jump открыл канал session: команд на jump нет"
    );
}

#[tokio::test]
async fn reverse_forwarding_is_not_granted() {
    let stand = Stand::start().await;
    let handle = stand.jump_as(&stand.env.enroll("r")).await;
    assert!(
        handle.tcpip_forward("127.0.0.1", 0).await.is_err(),
        "server согласился на обратный проброс"
    );
}

/// Подписывает сертификат с настройкой, которой нет у методов выдачи.
fn custom_cert(ca_key: &PrivateKey, subject: &PublicKey, key_id: &str, configure: impl FnOnce(&mut Builder)) -> Certificate {
    let now = unix(SystemTime::now());
    let mut builder = Builder::new([1u8; 16], subject.key_data().clone(), now - CLOCK_SKEW.as_secs(), now + 60).unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.key_id(key_id).unwrap();
    builder.valid_principal(CONTROL_LOGIN).unwrap();
    configure(&mut builder);
    builder.sign(ca_key).unwrap()
}

fn raw_ca_key(dir: &tempfile::TempDir) -> PrivateKey {
    PrivateKey::read_openssh_file(dir.path().join("ca")).unwrap()
}

// ---------------------------------------------------------------- центр сертификации

#[test]
fn node_certificate_carries_fingerprint_as_key_id_and_only_the_three_logins_as_principals() {
    let dir = tempfile::tempdir().unwrap();
    let ca = ca_in(&dir);
    let cert = ca.issue_node(random_key().public_key(), "node-9", short()).unwrap();
    assert_eq!(cert.key_id(), "node-9", "key_id — fingerprint узла");
    assert_eq!(
        cert.valid_principals(),
        [CONTROL_LOGIN, JUMP_LOGIN, DATA_LOGIN],
        "принципалы — только имена входа"
    );
    assert_eq!(cert.cert_type(), CertType::User);
}

#[test]
fn certificate_window_is_lifetime_after_now_with_clock_skew_before() {
    let dir = tempfile::tempdir().unwrap();
    let ca = ca_in(&dir);
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let cert = ca
        .issue_host(
            random_key().public_key(),
            "h",
            &["jump"],
            Validity {
                now,
                lifetime: Duration::from_secs(600),
            },
        )
        .unwrap();
    assert_eq!(cert.valid_after(), 1_000_000 - CLOCK_SKEW.as_secs());
    assert_eq!(cert.valid_before(), 1_000_600);
    assert_eq!(cert.cert_type(), CertType::Host);
}

#[test]
fn certificate_without_principals_is_not_issued() {
    // Сертификат без принципалов OpenSSH считает действительным для любого имени; выдачу такого запрещает ssh-key.
    let dir = tempfile::tempdir().unwrap();
    let ca = ca_in(&dir);
    assert!(
        ca.issue_user(random_key().public_key(), "id", &[], short()).is_err(),
        "пустые принципалы дают сертификат на любое имя"
    );
}

#[test]
fn authority_debug_shows_no_key_material() {
    let dir = tempfile::tempdir().unwrap();
    let ca = ca_in(&dir);
    let pem = std::fs::read_to_string(dir.path().join("ca")).unwrap();
    let body: String = pem.lines().filter(|line| !line.starts_with("-----")).collect();
    let shown = format!("{ca:?} {ca:#?}");
    assert!(!shown.contains(&body[..40]), "Debug содержит закрытый ключ: {shown}");
    assert!(shown.contains("SHA256:"), "Debug показывает отпечаток: {shown}");
}

#[test]
fn created_key_file_is_private_and_loads_back_as_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ca");
    let created = CertificateAuthority::load_or_create(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "права ключа CA"
    );
    let loaded = CertificateAuthority::load_or_create(&path).unwrap();
    assert_eq!(
        created.public_key(),
        loaded.public_key(),
        "второй запуск читает тот же ключ, а не создаёт новый"
    );
}

#[test]
fn key_file_readable_by_others_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ca");
    CertificateAuthority::load_or_create(&path).unwrap();
    for mode in [0o640, 0o604] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(CertificateAuthority::load(&path).is_err(), "ключ CA с правами {mode:o} принят");
    }
}
