//! Подпись манифеста: проверка ключом выпуска и отказ на всём, что подписью не является.

use proptest::prelude::*;

use super::*;

const TEST_KEY: &str = include_str!("test-key.asc");
const TEST_FINGERPRINT: &str = "9CA642F235A91292D540C53E41B8F293FBCF1FBD";
const MANIFEST: &[u8] = include_bytes!("manifest-2.1.286.json");
const SIGNATURE: &str = include_str!("manifest-2.1.286.json.sig");
const OTHER_SIGNATURE: &str = include_str!("manifest-2.1.287.json.sig");
const ANTHROPIC_KEY: &str = include_str!("../../../src/server/update/claude-code.asc");
const ANTHROPIC_FINGERPRINT: &str = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE";

fn key() -> ReleaseKey {
    ReleaseKey::parse(TEST_KEY, TEST_FINGERPRINT).unwrap()
}

fn armor(label: &str, body: &[u8]) -> String {
    format!(
        "-----BEGIN PGP {label}-----\n\n{}\n-----END PGP {label}-----\n",
        STANDARD.encode(body)
    )
}

#[test]
fn signature_of_the_manifest_by_the_pinned_key_passes() {
    key().verify(MANIFEST, SIGNATURE).expect("верная подпись отвергнута");
}

#[test]
fn changed_manifest_fails() {
    let mut changed = MANIFEST.to_vec();
    let position = changed.iter().position(|byte| *byte == b'6').unwrap();
    changed[position] = b'7';
    assert!(key().verify(&changed, SIGNATURE).is_err(), "изменённый манифест прошёл проверку");
}

#[test]
fn signature_of_another_manifest_fails() {
    assert!(
        key().verify(MANIFEST, OTHER_SIGNATURE).is_err(),
        "подпись другого манифеста принята"
    );
}

#[test]
fn signature_by_another_key_fails_even_if_it_is_valid_for_its_own_key() {
    let anthropic = ReleaseKey::parse(ANTHROPIC_KEY, ANTHROPIC_FINGERPRINT).unwrap();
    assert!(
        anthropic.verify(MANIFEST, SIGNATURE).is_err(),
        "подпись чужого ключа принята как подпись ключа Anthropic"
    );
}

#[test]
fn key_with_another_fingerprint_is_refused() {
    assert!(
        ReleaseKey::parse(TEST_KEY, ANTHROPIC_FINGERPRINT).is_err(),
        "ключ принят под чужим отпечатком"
    );
    assert!(ReleaseKey::parse(TEST_KEY, "zz").is_err(), "принят отпечаток не из 40 цифр");
}

#[test]
fn fingerprint_is_strict_hex_without_signs() {
    assert!(parse_fingerprint(TEST_FINGERPRINT).is_ok());
    assert!(
        parse_fingerprint(&TEST_FINGERPRINT.to_lowercase()).is_ok(),
        "строчные цифры отвергнуты"
    );
    let signed = format!("+A{}", "00".repeat(19));
    assert_eq!(signed.len(), 40);
    assert!(parse_fingerprint(&signed).is_err(), "знак + принят как часть цифры отпечатка");
    assert!(parse_fingerprint(&format!("-1{}", "00".repeat(19))).is_err());
}

#[test]
fn any_truncation_of_the_signature_fails() {
    let body = dearmor(SIGNATURE, "SIGNATURE").unwrap();
    for length in 0..body.len() {
        let armored = armor("SIGNATURE", &body[..length]);
        assert!(
            key().verify(MANIFEST, &armored).is_err(),
            "обрезанная до {length} байт подпись принята"
        );
    }
}

proptest! {
    #[test]
    fn garbage_is_an_error_not_a_panic(body in proptest::collection::vec(any::<u8>(), 0..700), text in ".{0,300}") {
        prop_assert!(key().verify(MANIFEST, &armor("SIGNATURE", &body)).is_err());
        prop_assert!(key().verify(MANIFEST, &text).is_err());
        prop_assert!(ReleaseKey::parse(&text, TEST_FINGERPRINT).is_err());
        prop_assert!(ReleaseKey::parse(&armor("PUBLIC KEY BLOCK", &body), TEST_FINGERPRINT).is_err());
    }
}
