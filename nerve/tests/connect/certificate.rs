use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::ssh_key::{Algorithm, PrivateKey};

use super::*;

const MINUTE: u64 = 60;

/// Сертификат на окно `[0, 15 минут)`.
fn certificate() -> Certificate {
    let ca = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let subject = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let mut builder = Builder::new([0u8; 16], subject.public_key().key_data().clone(), 0, 15 * MINUTE).unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.key_id("node").unwrap();
    builder.valid_principal("iapetus").unwrap();
    builder.sign(&ca).unwrap()
}

fn at(minutes: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(minutes * MINUTE)
}

#[test]
fn certificate_is_renewed_five_minutes_before_expiry() {
    let certificate = certificate();
    assert!(
        !renewal_due(Some(&certificate), at(9)),
        "продление раньше, чем за пять минут до конца"
    );
    assert_eq!(
        renewal_delay(Some(&certificate), at(9)),
        Duration::from_secs(MINUTE),
        "остаток до продления посчитан неверно"
    );
    assert!(renewal_due(Some(&certificate), at(10)), "продления нет, хотя до конца пять минут");
    assert!(renewal_due(Some(&certificate), at(20)), "истёкший сертификат не продлевается");
}

#[tokio::test(start_paused = true)]
async fn renewal_fires_at_exactly_five_minutes_before_expiry() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let ca = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let subject = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let mut builder = Builder::new([0u8; 16], subject.public_key().key_data().clone(), now, now + 15 * MINUTE).unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.key_id("node").unwrap();
    builder.valid_principal("iapetus").unwrap();
    let certificate = builder.sign(&ca).unwrap();

    let started = tokio::time::Instant::now();
    let elapsed = keep_fresh_until(|| Some(certificate.clone()), async || Ok(ControlFlow::Break(started.elapsed()))).await;

    let expected = Duration::from_secs(10 * MINUTE);
    let deviation = elapsed.abs_diff(expected);
    assert!(deviation <= Duration::from_secs(1), "продление в {elapsed:?} вместо {expected:?}");
}

#[test]
fn missing_certificate_is_renewed() {
    assert!(renewal_due(None, at(0)), "без сертификата продление не нужно");
}
