//! Выпуск Claude Code из манифеста: какие платформы и суммы принимаются и что пишется в журнал.

use std::sync::{Arc, Mutex, PoisonError};

use super::*;

/// Манифест 2.1.286 с записями `platforms`; `None` — записи нет.
fn manifest_of(x64: Option<&str>, arm64: Option<&str>) -> String {
    let entry =
        |name: &str, sum: Option<&str>| sum.map(|sum| format!(r#""{name}": {{ "binary": "claude", "checksum": "{sum}", "size": 1 }}"#));
    let entries: Vec<String> = [entry("linux-x64", x64), entry("linux-arm64", arm64)]
        .into_iter()
        .flatten()
        .collect();
    format!(r#"{{ "version": "2.1.286", "platforms": {{ {} }} }}"#, entries.join(", "))
}

/// Предупреждения, которые `release_from` записал в журнал, пока считал манифест.
fn release_with_log(manifest: &str) -> (anyhow::Result<AgentRelease>, String) {
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Log::default();
    let subscriber = tracing_subscriber::fmt().with_writer({
        let log = log.clone();
        move || log.clone()
    });
    let release = tracing::subscriber::with_default(subscriber.finish(), || {
        release_from(manifest.as_bytes(), "2.1.286", "https://example.org")
    });
    let text = String::from_utf8(log.0.lock().unwrap_or_else(PoisonError::into_inner).clone()).unwrap();
    (release, text)
}

fn names(release: &AgentRelease) -> Vec<&str> {
    release
        .artifacts
        .iter()
        .map(|artifact| match artifact.platform {
            Platform::LinuxX64 => "linux-x64",
            Platform::LinuxArm64 => "linux-arm64",
        })
        .collect()
}

#[test]
fn manifest_with_both_valid_platforms_is_offered_with_both_and_without_warnings() {
    let sum = "a".repeat(64);
    let (release, log) = release_with_log(&manifest_of(Some(&sum), Some(&sum)));

    assert_eq!(names(&release.unwrap()), ["linux-x64", "linux-arm64"], "платформы потеряны");
    assert!(!log.contains("WARN"), "предупреждение при верном манифесте: {log}");
}

#[test]
fn manifest_without_a_platform_is_offered_with_the_other_one_and_a_warning() {
    let sum = "a".repeat(64);
    let (release, log) = release_with_log(&manifest_of(Some(&sum), None));

    assert_eq!(
        names(&release.unwrap()),
        ["linux-x64"],
        "версия без linux-arm64 не предложена с одной платформой"
    );
    assert!(
        log.contains("WARN") && log.contains("linux-arm64"),
        "нет предупреждения о платформе: {log}"
    );
}

#[test]
fn manifest_with_a_broken_checksum_is_offered_without_that_platform_and_with_a_warning() {
    let sum = "a".repeat(64);
    for broken in ["", "ABC", &"A".repeat(64), &"g".repeat(64), &"a".repeat(63)] {
        let (release, log) = release_with_log(&manifest_of(Some(broken), Some(&sum)));

        assert_eq!(
            names(&release.unwrap()),
            ["linux-arm64"],
            "платформа с суммой {broken:?} предложена"
        );
        assert!(
            log.contains("WARN") && log.contains("linux-x64"),
            "нет предупреждения для суммы {broken:?}: {log}"
        );
    }
}

#[test]
fn manifest_without_a_valid_platform_is_not_offered() {
    let (release, log) = release_with_log(&manifest_of(Some("ABC"), None));

    assert!(release.is_err(), "версия без валидных платформ предложена");
    assert!(
        log.contains("linux-x64") && log.contains("linux-arm64"),
        "платформы без причины: {log}"
    );
}
