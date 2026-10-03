use super::*;

#[test]
fn versions_compare_as_numbers_and_only_plain_ones_are_accepted() {
    assert!(parse_version("2.1.10") > parse_version("2.1.9"), "версии сравниваются как строки");
    for bad in [
        "",
        "2.1",
        "2.1.x",
        "2.1.3.4",
        "../2.1.3",
        "2.1.-3",
        "2..3",
        "2.1.3 ",
        "2.1.99999999999999999999999",
    ] {
        assert_eq!(parse_version(bad), None, "версия {bad:?} принята");
    }
}

#[test]
fn sha256_is_sixty_four_lowercase_hex_digits() {
    assert!(is_sha256(&"ab09".repeat(16)));
    for bad in ["", "ABC", &"A".repeat(64), &"g".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
        assert!(!is_sha256(bad), "принята сумма {bad:?}");
    }
}

#[test]
fn digest_must_match_its_algorithm_length() {
    assert!(is_well_formed(&Digest::Sha256("ab".repeat(32))));
    assert!(is_well_formed(&Digest::Sha512("ab".repeat(64))));
    assert!(!is_well_formed(&Digest::Sha256("ab".repeat(64))), "sha512 принята за sha256");
    assert!(!is_well_formed(&Digest::Sha512("ab".repeat(32))), "sha256 принята за sha512");
    assert!(!is_well_formed(&Digest::Sha512("AB".repeat(64))), "заглавные цифры приняты");
    assert!(!is_well_formed(&Digest::Sha256("AB".repeat(32))), "заглавные приняты в sha256");
}
