//! Регистрация и продление на настоящей SQLite и настоящем центре сертификации. Время задаётся
//! параметром `now`, паузы не нужны. Каждое условие отказа нарушается отдельным тестом, остальные
//! условия при этом верны.

use nerve::ssh::{CONTROL_LOGIN, DATA_LOGIN, ENROLL_LOGIN, JUMP_LOGIN, MAX_CERTIFICATE_WINDOW};
use russh::keys::Certificate;
use russh::keys::ssh_key::certificate::CertType;
use russh::keys::ssh_key::{HashAlg, LineEnding};

use super::*;
use crate::server::registry::{Entity, Error as RegistryError};
use crate::server::testing::{Env, TestNode, random_fingerprint, random_key, signed_renewal, unix};

const MINUTE: Duration = Duration::from_secs(60);

/// Фиксированный момент: сертификаты не зависят от часов машины. Лишние семь секунд делают время
/// запроса «некруглым»: подпись над округлённым временем не сойдётся.
fn epoch(minutes: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_hours(500_000) + Duration::from_secs(7) + MINUTE * u32::try_from(minutes).unwrap()
}

fn renew_at(env: &Env, request: &RenewRequest, now: SystemTime) -> Result<Certificate, Error> {
    let reply = renew(&mut env.reg(), &env.ca, request, now)?;
    Ok(Certificate::from_openssh(&reply.certificate).unwrap())
}

#[track_caller]
fn assert_registry_error<T: fmt::Debug>(result: Result<T, Error>, expected: &RegistryError, why: &str) {
    match result {
        Err(Error::Registry(got)) => assert_eq!(format!("{got:?}"), format!("{expected:?}"), "{why}"),
        other => panic!("{why}: получено {other:?}"),
    }
}

#[track_caller]
fn assert_signature_error(result: &Result<Certificate, Error>, why: &str) {
    assert!(matches!(result, Err(Error::Signature)), "{why}: получено {result:?}");
}

fn registered(env: &Env) -> TestNode {
    env.enroll_at("дом", epoch(0))
}

// ---------------------------------------------------------------- регистрация

#[test]
fn registration_issues_a_fifteen_minute_certificate_for_the_node_key() {
    let env = Env::new();
    let node = registered(&env);
    let cert = &node.certificate;

    assert_eq!(
        cert.public_key(),
        node.key.public_key().key_data(),
        "сертификат выдан не на ключ узла"
    );
    assert_eq!(cert.key_id(), node.fingerprint.as_str(), "key_id — fingerprint узла");
    assert_eq!(
        cert.valid_principals(),
        [CONTROL_LOGIN, JUMP_LOGIN, DATA_LOGIN],
        "принципалы — имена входа на SSH server (управление, jump) и на сервер данных сессии"
    );
    assert_eq!(cert.cert_type(), CertType::User);
    assert_eq!(cert.valid_before(), unix(epoch(0)) + 15 * 60, "срок — 15 минут");
}

#[test]
fn wrong_orbit_key_registers_nothing() {
    let env = Env::new();
    let (fingerprint, key) = (random_fingerprint(), random_key());
    let mut request = env.request("дом", &fingerprint, &key);
    request.orbit_key = "чужой токен".to_owned();

    let result = register(&mut env.reg(), &env.ca, &request, epoch(0));

    assert_registry_error(result, &RegistryError::BadCredentials, "неверный orbit_key принят");
    assert!(env.reg().runners().unwrap().is_empty(), "узел записан по неверному orbit_key");
}

#[test]
fn replaced_orbit_key_no_longer_registers() {
    let mut env = Env::new();
    let (fingerprint, key) = (random_fingerprint(), random_key());
    let request = env.request("дом", &fingerprint, &key);
    env.rotate_orbit_key(epoch(0));

    let result = register(&mut env.reg(), &env.ca, &request, epoch(0));

    assert_registry_error(result, &RegistryError::BadCredentials, "заменённый orbit_key принят");
}

#[test]
fn fingerprint_repeated_with_another_key_is_refused_and_the_old_key_stays() {
    let env = Env::new();
    let node = registered(&env);
    let intruder = random_key();
    let request = env.request("дом", &node.fingerprint, &intruder);

    let result = register(&mut env.reg(), &env.ca, &request, epoch(1));

    assert_registry_error(result, &RegistryError::FingerprintTaken, "fingerprint присвоен с другим ключом");
    assert_eq!(
        env.reg().node_public_key(&node.fingerprint).unwrap(),
        *node.key.public_key(),
        "ключ узла заменён"
    );
}

#[test]
fn public_key_that_is_not_openssh_is_refused() {
    let env = Env::new();
    let (fingerprint, key) = (random_fingerprint(), random_key());
    let mut request = env.request("дом", &fingerprint, &key);
    request.public_key = "это не ключ".to_owned();

    let result = register(&mut env.reg(), &env.ca, &request, epoch(0));

    assert!(matches!(result, Err(Error::PublicKey)), "принят нечитаемый ключ: {result:?}");
    assert!(env.reg().runners().unwrap().is_empty());
}

#[test]
fn public_key_of_another_algorithm_is_refused() {
    let env = Env::new();
    let (fingerprint, key) = (random_fingerprint(), random_key());
    let mut request = env.request("дом", &fingerprint, &key);
    // Ключ безопасности SK-Ed25519: читается как ключ OpenSSH, но не Ed25519.
    request.public_key =
        "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4fAAAABHNzaDo="
            .to_owned();
    assert!(
        PublicKey::from_openssh(&request.public_key).is_ok(),
        "проба не читается как ключ OpenSSH"
    );

    let result = register(&mut env.reg(), &env.ca, &request, epoch(0));

    assert!(matches!(result, Err(Error::PublicKey)), "принят ключ не Ed25519: {result:?}");
}

// ---------------------------------------------------------------- продление

#[test]
fn renewal_with_the_current_orbit_key_issues_a_later_certificate_for_the_same_key() {
    let env = Env::new();
    let node = registered(&env);
    let request = signed_renewal(&node.key, &node.fingerprint, unix(epoch(10)), &env.orbit_key);

    let cert = renew_at(&env, &request, epoch(10)).unwrap();

    assert_eq!(cert.public_key(), node.key.public_key().key_data(), "продлён не на тот ключ");
    assert_eq!(cert.key_id(), node.fingerprint.as_str());
    assert_eq!(cert.valid_before(), unix(epoch(10)) + 15 * 60, "срок считается от продления");
    assert!(cert.valid_before() > node.certificate.valid_before());
}

#[test]
fn renewal_with_a_replaced_orbit_key_is_refused_although_the_signature_is_valid() {
    let mut env = Env::new();
    let node = registered(&env);
    let old = env.rotate_orbit_key(epoch(5));
    let request = signed_renewal(&node.key, &node.fingerprint, unix(epoch(10)), &old);

    let result = renew_at(&env, &request, epoch(10));

    assert_registry_error(result, &RegistryError::BadCredentials, "продление по заменённому orbit_key");
}

#[test]
fn renewal_with_a_wrong_orbit_key_is_refused_although_the_signature_is_valid() {
    let env = Env::new();
    let node = registered(&env);
    let mut request = env.renewal(&node, unix(epoch(1)));
    request.orbit_key = "не тот ключ".to_owned();

    assert_registry_error(
        renew_at(&env, &request, epoch(1)),
        &RegistryError::BadCredentials,
        "продление по неверному orbit_key",
    );
}

#[test]
fn renewal_signed_by_another_key_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let request = signed_renewal(&random_key(), &node.fingerprint, unix(epoch(1)), &env.orbit_key);

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "подпись чужим ключом принята");
}

#[test]
fn renewal_signed_by_the_key_of_another_registered_node_is_refused() {
    let env = Env::new();
    let victim = registered(&env);
    let other = env.enroll_at("офис", epoch(0));
    let request = signed_renewal(&other.key, &victim.fingerprint, unix(epoch(1)), &env.orbit_key);

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "ключ другого узла продлил чужой сертификат");
}

#[test]
fn renewal_with_a_signature_over_another_node_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let other = env.enroll_at("офис", epoch(0));
    // Верный ключ, но подписан запрос за другой узел, а в запросе названы мы.
    let mut request = signed_renewal(&node.key, &other.fingerprint, unix(epoch(1)), &env.orbit_key);
    request.fingerprint = node.fingerprint;

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "подпись за другой fingerprint принята");
}

#[test]
fn renewal_with_a_timestamp_changed_after_signing_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let mut request = signed_renewal(&node.key, &node.fingerprint, unix(epoch(1)), &env.orbit_key);
    request.timestamp += 1;

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "время не входит в подпись");
}

#[test]
fn renewal_signed_for_another_purpose_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let timestamp = unix(epoch(1));
    let payload = RenewRequest::payload(&node.fingerprint, timestamp);
    let foreign = SshSig::sign(&node.key, "file", HashAlg::Sha256, &payload).unwrap();
    assert_ne!(RENEW_NAMESPACE, "file");
    let request = RenewRequest {
        orbit_key: env.orbit_key.expose().to_owned(),
        fingerprint: node.fingerprint,
        timestamp,
        signature: foreign.to_pem(LineEnding::LF).unwrap().into_bytes(),
    };

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "подпись из другого пространства имён принята");
}

#[test]
fn renewal_with_garbage_instead_of_a_signature_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let request = RenewRequest {
        orbit_key: env.orbit_key.expose().to_owned(),
        fingerprint: node.fingerprint,
        timestamp: unix(epoch(1)),
        signature: "не подпись".as_bytes().to_vec(),
    };

    assert_signature_error(&renew_at(&env, &request, epoch(1)), "мусор принят за подпись");
}

#[test]
fn renewal_of_a_revoked_node_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let request = signed_renewal(&node.key, &node.fingerprint, unix(epoch(1)), &env.orbit_key);
    env.reg().revoke_node(&node.fingerprint).unwrap();

    let result = renew_at(&env, &request, epoch(1));

    assert_registry_error(result, &RegistryError::Revoked, "отозванный узел продлён");
}

#[test]
fn renewal_of_an_unknown_fingerprint_is_refused() {
    let env = Env::new();
    let stranger = random_fingerprint();
    let request = signed_renewal(&random_key(), &stranger, unix(epoch(1)), &env.orbit_key);

    let result = renew_at(&env, &request, epoch(1));

    assert_registry_error(result, &RegistryError::NotFound(Entity::Node), "неизвестный узел продлён");
}

#[test]
fn renewal_request_older_than_the_skew_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let sent = unix(epoch(0));
    let request = signed_renewal(&node.key, &node.fingerprint, sent, &env.orbit_key);
    let late = UNIX_EPOCH + Duration::from_secs(sent) + MAX_REQUEST_SKEW;

    renew_at(&env, &request, late).expect("запрос на границе допуска отвергнут");
    let result = renew_at(&env, &request, late + Duration::from_secs(1));

    assert!(
        matches!(result, Err(Error::Stale)),
        "перехваченный старый запрос принят: {result:?}"
    );
}

#[test]
fn renewal_request_from_the_future_is_refused() {
    let env = Env::new();
    let node = registered(&env);
    let now = epoch(0);
    let boundary = unix(now) + MAX_REQUEST_SKEW.as_secs();

    let ok = signed_renewal(&node.key, &node.fingerprint, boundary, &env.orbit_key);
    renew_at(&env, &ok, now).expect("запрос на границе допуска отвергнут");
    let ahead = signed_renewal(&node.key, &node.fingerprint, boundary + 1, &env.orbit_key);
    let result = renew_at(&env, &ahead, now);

    assert!(matches!(result, Err(Error::Stale)), "запрос из будущего принят: {result:?}");
}

#[test]
fn renewal_with_an_unrepresentable_timestamp_is_refused_without_panic() {
    let env = Env::new();
    let node = registered(&env);
    let request = signed_renewal(&node.key, &node.fingerprint, u64::MAX, &env.orbit_key);

    let result = renew_at(&env, &request, epoch(0));

    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
}

// ---------------------------------------------------------------- сертификат worker

fn worker_request(env: &Env, name: &str) -> (WorkerCertificateRequest, russh::keys::PrivateKey) {
    let key = random_key();
    let request = WorkerCertificateRequest {
        orbit_key: env.orbit_key.expose().to_owned(),
        public_key: key.public_key().to_openssh().unwrap(),
        name: name.to_owned(),
    };
    (request, key)
}

fn worker_certificate_at(env: &Env, request: &WorkerCertificateRequest, now: SystemTime) -> Result<Certificate, Error> {
    let reply = worker_certificate(&env.reg(), &env.ca, request, now)?;
    Ok(Certificate::from_openssh(&reply.certificate).unwrap())
}

#[test]
fn worker_certificate_is_short_and_lets_in_only_under_its_name_with_a_pty() {
    let env = Env::new();
    let (request, key) = worker_request(&env, "alice");

    let cert = worker_certificate_at(&env, &request, epoch(0)).unwrap();

    assert_eq!(cert.public_key(), key.public_key().key_data(), "сертификат не на ключ worker");
    assert_eq!(cert.cert_type(), CertType::User);
    assert_eq!(cert.key_id(), nerve::ssh::WORKER_KEY_ID, "key_id не отличает worker от runner");
    assert_eq!(
        cert.valid_principals(),
        [CONTROL_LOGIN, JUMP_LOGIN, "alice"],
        "принципалы — управление, jump и имя worker"
    );
    assert_eq!(
        cert.extensions().keys().map(String::as_str).collect::<Vec<_>>(),
        ["permit-pty"],
        "worker разрешён pty и ничего больше"
    );
    assert_eq!(cert.valid_before(), unix(epoch(0)) + 15 * 60, "срок — 15 минут");
    assert!(
        cert.valid_before() - cert.valid_after() <= MAX_CERTIFICATE_WINDOW.as_secs(),
        "окно сертификата шире того, что пускает jump"
    );
}

#[test]
fn worker_certificate_with_a_replaced_orbit_key_is_refused() {
    let mut env = Env::new();
    let (request, _) = worker_request(&env, "alice");
    env.rotate_orbit_key(epoch(0));

    assert_registry_error(
        worker_certificate_at(&env, &request, epoch(1)),
        &RegistryError::BadCredentials,
        "сертификат worker по заменённому orbit_key",
    );
}

#[test]
fn worker_certificate_with_a_wrong_orbit_key_is_refused() {
    let env = Env::new();
    let (mut request, _) = worker_request(&env, "alice");
    request.orbit_key = "не тот ключ".to_owned();

    assert_registry_error(
        worker_certificate_at(&env, &request, epoch(0)),
        &RegistryError::BadCredentials,
        "сертификат worker по неверному orbit_key",
    );
}

#[test]
fn worker_name_that_is_empty_unprintable_or_reserved_gets_no_certificate() {
    let env = Env::new();
    for name in [
        "",
        "два слова",
        "a,b",
        "\u{7f}",
        CONTROL_LOGIN,
        JUMP_LOGIN,
        ENROLL_LOGIN,
        DATA_LOGIN,
        "iapetus-x",
    ] {
        let (request, _) = worker_request(&env, name);
        let result = worker_certificate_at(&env, &request, epoch(0));
        assert!(matches!(result, Err(Error::WorkerName)), "имя {name:?} принято: {result:?}");
    }
}

#[test]
fn worker_certificate_for_a_key_that_is_not_ed25519_is_refused() {
    let env = Env::new();
    let (mut request, _) = worker_request(&env, "alice");
    request.public_key = "это не ключ".to_owned();

    let result = worker_certificate_at(&env, &request, epoch(0));

    assert!(matches!(result, Err(Error::PublicKey)), "{result:?}");
}

// ---------------------------------------------------------------- первый orbit_key

fn empty_registry(dir: &tempfile::TempDir) -> Registry {
    Registry::open(&dir.path().join("first.db"), registry::TokenCipher::from_key([3; 32])).unwrap()
}

#[test]
fn first_orbit_key_is_written_to_a_private_file_only_into_an_empty_registry() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::TempDir::new().unwrap();
    let mut registry = empty_registry(&dir);
    let path = dir.path().join("orbit_key");

    assert!(
        write_first_orbit_key(&mut registry, &path, epoch(0)).unwrap(),
        "пустой реестр без первого ключа"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "файл первого orbit_key доступен не только владельцу");
    registry
        .check_orbit_key(&OrbitKey::new(text.trim()))
        .expect("ключ из файла не действует");

    std::fs::remove_file(&path).unwrap();
    assert!(
        !write_first_orbit_key(&mut registry, &path, epoch(1)).unwrap(),
        "при выданном ключе выдан ещё один первый"
    );
    assert!(!path.exists(), "второй запуск записал файл");
    registry
        .check_orbit_key(&OrbitKey::new(text.trim()))
        .expect("второй запуск заменил ключ");
}

#[test]
fn first_orbit_key_is_not_issued_while_the_registry_has_runners() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut registry = empty_registry(&dir);
    registry
        .register_node(&NewNode {
            fingerprint: random_fingerprint(),
            public_key: random_key().public_key().clone(),
            name: "дом".to_owned(),
            os: "linux".to_owned(),
            parent: None,
        })
        .unwrap();
    let path = dir.path().join("orbit_key");

    let written = write_first_orbit_key(&mut registry, &path, epoch(0)).unwrap();

    assert!(!written && !path.exists(), "реестр с runner не пуст, а первый ключ выдан");
}

#[test]
fn registering_a_clone_of_a_revoked_or_unknown_parent_is_refused() {
    let env = Env::new();
    let parent = registered(&env);
    let request = |parent: &Fingerprint| {
        let (fingerprint, key) = (random_fingerprint(), random_key());
        RegisterRequest {
            parent: Some(parent.clone()),
            ..env.request("proj-clone-1", &fingerprint, &key)
        }
    };
    let register = |request: &RegisterRequest| register(&mut env.reg(), &env.ca, request, epoch(1));
    register(&request(&parent.fingerprint)).expect("клон живого родителя отклонён");
    register(&request(&parent.fingerprint)).expect("повтор имени клона сломал регистрацию");

    assert_registry_error(
        register(&request(&random_fingerprint())),
        &RegistryError::InvalidParent,
        "клон незарегистрированного родителя принят",
    );
    env.reg().revoke_node(&parent.fingerprint).unwrap();
    assert_registry_error(
        register(&request(&parent.fingerprint)),
        &RegistryError::InvalidParent,
        "клон отозванного родителя принят",
    );
}
