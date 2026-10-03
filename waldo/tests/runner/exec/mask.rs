use proptest::prelude::*;

use super::*;

fn secrets(values: &[&str]) -> Secrets {
    Secrets::new(values.iter().map(|value| value.as_bytes().to_vec()))
}

fn mask_in_chunks(secrets: &Secrets, data: &[u8], cuts: &[usize]) -> Vec<u8> {
    secrets.mask_in_chunks(data, cuts)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[track_caller]
fn assert_masked(secrets: &Secrets, input: &str, expected: &str) {
    let masked = secrets.mask_all(input.as_bytes());
    assert_eq!(String::from_utf8_lossy(&masked), expected, "вывод {input:?} замаскирован неверно");
}

#[test]
fn secret_is_replaced_with_mask() {
    assert_masked(&secrets(&["ghp_abc123"]), "token=ghp_abc123;\n", "token=***;\n");
}

#[test]
fn every_occurrence_is_replaced() {
    assert_masked(&secrets(&["s3cr3t"]), "s3cr3t s3cr3ts3cr3t", "*** ******");
}

#[test]
fn secret_split_at_every_boundary_is_never_shown() {
    let secrets = secrets(&["ghp_abc123"]);
    let text = b"before ghp_abc123 after";
    for cut in 0..=text.len() {
        let masked = mask_in_chunks(&secrets, text, &[cut]);
        assert_eq!(
            String::from_utf8_lossy(&masked),
            "before *** after",
            "секрет, разрезанный на байте {cut}, прошёл открыто или вывод изменился"
        );
    }
}

#[test]
fn held_tail_that_never_became_a_secret_is_released_at_the_end() {
    let secrets = secrets(&["ghp_abc123"]);
    assert_eq!(
        mask_in_chunks(&secrets, b"value ghp_ab", &[8]),
        b"value ghp_ab",
        "начало секрета в конце потока потеряно"
    );
}

#[test]
fn values_shorter_than_the_minimum_are_not_masked() {
    assert_masked(&secrets(&["abc", ""]), "abc abc", "abc abc");
}

#[test]
fn overlapping_secrets_are_masked_as_one_region() {
    // `cdef` продолжает `abcd`: замаскировать только `abcd` значило бы показать `ef`.
    assert_masked(&secrets(&["abcd", "cdef"]), "xabcdefx", "x***x");
}

#[test]
fn shorter_secret_inside_a_longer_one_does_not_leak_the_rest() {
    let secrets = secrets(&["abcd", "abcdef"]);
    for cut in 0..=6 {
        let masked = mask_in_chunks(&secrets, b"abcdef", &[cut]);
        assert_eq!(masked, b"***", "разрез на байте {cut}: короткий секрет открыл хвост длинного");
    }
}

#[test]
fn partly_arrived_secret_does_not_release_the_start_of_an_overlapping_one() {
    // `xabc` целиком в первом куске, а `bcdef` только начался: удержать надо и `x`, иначе откроется начало `xabc`.
    let secrets = secrets(&["xabc", "bcdef"]);
    for cut in 0..=7 {
        let masked = mask_in_chunks(&secrets, b"xabcdef", &[cut]);
        assert_eq!(masked, b"***", "разрез на байте {cut}: открыто начало секрета");
    }
}

#[test]
fn secret_names_follow_the_rule() {
    let cases = [
        ("GITHUB_TOKEN", true),
        ("api_key", true),
        ("AWS_SECRET_ACCESS_KEY", true),
        ("DB_PASSWORD", true),
        ("TOKEN", true),
        ("MONKEY", false),
        ("KEYBOARD", false),
        ("AWS_ACCESS_KEY_ID", false),
        ("PATH", false),
    ];
    for (name, secret) in cases {
        assert_eq!(is_secret_name(name), secret, "имя {name}: секрет ли — {secret}");
    }
}

fn secret_strategy() -> impl Strategy<Value = Vec<String>> {
    // Малый алфавит: секреты пересекаются друг с другом и с текстом.
    prop::collection::vec("[abc]{4,6}", 1..4)
}

proptest! {
    #[test]
    fn secret_cut_into_any_chunks_never_passes_openly(
        values in secret_strategy(),
        text in "[abc .]{0,40}",
        cuts in prop::collection::vec(0..60usize, 0..8),
    ) {
        let secrets = Secrets::new(values.iter().map(|value| value.as_bytes().to_vec()));
        let mut cuts = cuts;
        cuts.sort_unstable();
        let masked = mask_in_chunks(&secrets, text.as_bytes(), &cuts);
        for value in &values {
            prop_assert!(
                !contains(&masked, value.as_bytes()),
                "секрет {value:?} прошёл открыто: {:?} -> {:?}", text, String::from_utf8_lossy(&masked)
            );
        }
    }

    #[test]
    fn masking_does_not_depend_on_how_output_is_chunked(
        values in secret_strategy(),
        text in "[abc .]{0,40}",
        cuts in prop::collection::vec(0..60usize, 0..8),
    ) {
        let secrets = Secrets::new(values.iter().map(|value| value.as_bytes().to_vec()));
        let mut cuts = cuts;
        cuts.sort_unstable();
        prop_assert_eq!(
            mask_in_chunks(&secrets, text.as_bytes(), &cuts),
            secrets.mask_all(text.as_bytes()),
            "разбиение {:?} изменило результат для {:?}", cuts, text
        );
    }
}

impl Secrets {
    /// Маскирует всю запись целиком.
    pub(crate) fn mask_all(&self, data: &[u8]) -> Vec<u8> {
        self.mask_in_chunks(data, &[])
    }

    /// Прогоняет вывод через маскировщик кусками по возрастающим границам `cuts` и склеивает всё, что он отдал.
    pub(crate) fn mask_in_chunks(&self, data: &[u8], cuts: &[usize]) -> Vec<u8> {
        let mut masker = Masker::new(self.clone());
        let mut out = Vec::new();
        let mut start = 0;
        for &cut in cuts.iter().chain(std::iter::once(&data.len())) {
            let cut = cut.clamp(start, data.len());
            out.extend(masker.push(&data[start..cut]));
            start = cut;
        }
        out.extend(masker.finish());
        out
    }
}
