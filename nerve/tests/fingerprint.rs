use super::*;

#[test]
fn separators_and_spaces_are_not_allowed_in_a_fingerprint() {
    for s in ["a.b", "a*", "a>", "a b"] {
        assert!(matches!(s.parse::<Fingerprint>(), Err(FingerprintError::InvalidChar(_))));
    }
}

#[test]
fn all_is_reserved_for_shared_environment_keys() {
    assert_eq!("all".parse::<Fingerprint>(), Err(FingerprintError::Reserved));
}

#[test]
fn deserialization_applies_the_same_checks_as_parsing() {
    assert!(serde_json::from_str::<Fingerprint>("\"a.b\"").is_err());
    assert!(serde_json::from_str::<Fingerprint>("\"all\"").is_err());
    assert!(serde_json::from_str::<Fingerprint>("\"\"").is_err());
}
