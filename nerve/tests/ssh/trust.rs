//! Каждое условие проверки нарушается отдельным сертификатом, остальные при этом верны.

use russh::keys::ssh_key::certificate::Builder;
use russh::keys::ssh_key::{Algorithm, PrivateKey};

use super::*;

const NOW: u64 = 1_000_000;
const LOGIN: &str = "iapetus";
const HOST: &str = "session.test";

fn key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

/// Верный сертификат от `NOW - 30` до `before`; `configure` нарушает одно условие.
fn cert(ca: &PrivateKey, cert_type: CertType, principal: &str, before: u64, configure: impl FnOnce(&mut Builder)) -> Certificate {
    let mut builder = Builder::new([1u8; 16], key().public_key().key_data().clone(), NOW - 30, before).unwrap();
    builder.cert_type(cert_type).unwrap();
    builder.key_id("node").unwrap();
    builder.valid_principal(principal).unwrap();
    configure(&mut builder);
    builder.sign(ca).unwrap()
}

fn trust(ca: &PrivateKey) -> Trust {
    let ca: PublicKey = ca.public_key().clone();
    Trust::new(&[ca])
}

#[test]
fn user_certificate_names_the_violated_condition() {
    let ca = key();
    let trust = trust(&ca);
    let good = cert(&ca, CertType::User, LOGIN, NOW + 30, |_| {});
    assert_eq!(trust.check_user_cert(&good, LOGIN, NOW), Ok(()), "верный сертификат принят");

    let foreign = cert(&key(), CertType::User, LOGIN, NOW + 30, |_| {});
    assert_eq!(
        trust.check_user_cert(&foreign, LOGIN, NOW),
        Err("подпись неверна или CA не доверен")
    );
    assert_eq!(trust.check_user_cert(&good, LOGIN, NOW + 3600), Err("срок истёк"));
    assert_eq!(trust.check_user_cert(&good, LOGIN, NOW - 3600), Err("срок ещё не наступил"));

    let host = cert(&ca, CertType::Host, LOGIN, NOW + 30, |_| {});
    assert_eq!(trust.check_user_cert(&host, LOGIN, NOW), Err("сертификат не пользовательский"));

    let long = cert(&ca, CertType::User, LOGIN, NOW + MAX_CERTIFICATE_WINDOW.as_secs(), |_| {});
    assert_eq!(trust.check_user_cert(&long, LOGIN, NOW), Err("срок жизни больше допустимого"));

    assert_eq!(
        trust.check_user_cert(&good, "someone", NOW),
        Err("имени входа нет среди принципалов")
    );

    let critical = cert(&ca, CertType::User, LOGIN, NOW + 30, |builder| {
        builder.critical_option("force-command", "/bin/true").unwrap();
    });
    assert_eq!(trust.check_user_cert(&critical, LOGIN, NOW), Err("есть critical options"));
}

#[test]
fn host_certificate_names_the_violated_condition() {
    let ca = key();
    // Предел окна действует на пользовательские сертификаты: хост-сертификат шире предела принимается.
    let trust = trust(&ca);
    let good = cert(&ca, CertType::Host, HOST, NOW + 2 * MAX_CERTIFICATE_WINDOW.as_secs(), |_| {});
    assert_eq!(trust.check_host_cert(&good, HOST, NOW), Ok(()), "верный хост-сертификат принят");

    let foreign = cert(&key(), CertType::Host, HOST, NOW + 30, |_| {});
    assert_eq!(trust.check_host_cert(&foreign, HOST, NOW), Err("подпись неверна или CA не доверен"));
    assert_eq!(
        trust.check_host_cert(&good, HOST, NOW + 4 * MAX_CERTIFICATE_WINDOW.as_secs()),
        Err("срок истёк")
    );
    assert_eq!(trust.check_host_cert(&good, HOST, NOW - 3600), Err("срок ещё не наступил"));

    let user = cert(&ca, CertType::User, HOST, NOW + 30, |_| {});
    assert_eq!(trust.check_host_cert(&user, HOST, NOW), Err("сертификат не хост-сертификат"));

    assert_eq!(
        trust.check_host_cert(&good, "other.test", NOW),
        Err("имени хоста нет среди принципалов")
    );
}
