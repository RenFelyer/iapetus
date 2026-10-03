//! Слежение за выпусками Claude Code: что server предлагает session, а что отвергает.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{StatusCode, Uri};
use nerve::protocol::{Digest, Platform};
use tempfile::TempDir;
use tokio::net::TcpListener;

use crate::server::registry::TokenCipher;

use super::*;

const TEST_KEY: &str = include_str!("update/test-key.asc");
const TEST_FINGERPRINT: &str = "9CA642F235A91292D540C53E41B8F293FBCF1FBD";
const REAL_MANIFEST: &[u8] = include_bytes!("update/manifest-2.1.285.json");
const REAL_SIGNATURE: &str = include_str!("update/manifest-2.1.285.json.sig");
const MANIFEST_286: &[u8] = include_bytes!("update/manifest-2.1.286.json");
const SIGNATURE_286: &str = include_str!("update/manifest-2.1.286.json.sig");
const MANIFEST_287: &[u8] = include_bytes!("update/manifest-2.1.287.json");
const SIGNATURE_287: &str = include_str!("update/manifest-2.1.287.json.sig");

/// Сервер выпусков на `127.0.0.1:0`: отдаёт файлы из таблицы, остальное — 404.
#[derive(Clone, Default)]
struct Releases {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<AtomicUsize>,
}

impl Releases {
    fn put(&self, path: &str, body: impl AsRef<[u8]>) {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), body.as_ref().to_vec());
    }

    /// Выпуск `version`: манифест и его подпись.
    fn release(&self, version: &str, manifest: &[u8], signature: &str) {
        self.put(&format!("/{version}/manifest.json"), manifest);
        self.put(&format!("/{version}/manifest.json.sig"), signature);
    }

    async fn serve(&self) -> String {
        let files = Arc::clone(&self.files);
        let requests = Arc::clone(&self.requests);
        let app = Router::new().fallback(move |uri: Uri| {
            requests.fetch_add(1, Ordering::SeqCst);
            let body = files.lock().unwrap_or_else(PoisonError::into_inner).get(uri.path()).cloned();
            async move {
                match body {
                    Some(body) => (StatusCode::OK, body),
                    None => (StatusCode::NOT_FOUND, Vec::new()),
                }
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }
}

fn checksum(version: &str, platform: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("{version}-{platform}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .concat()
}

/// Источник Claude Code на сервере выпусков `base_url` под тестовым ключом.
fn test_source(base_url: String) -> Source {
    Source {
        base_url,
        key: TEST_KEY,
        fingerprint: TEST_FINGERPRINT,
        ..claude::source()
    }
}

/// Момент проверки в тестах без задержки.
const NOW: i64 = 1_000_000;

/// Реестр в файле `registry.db` каталога `dir`; повторное открытие — это перезапуск server.
fn open_registry(dir: &TempDir) -> Arc<Mutex<Registry>> {
    let registry = Registry::open(&dir.path().join("registry.db"), TokenCipher::from_key([7; 32])).unwrap();
    Arc::new(Mutex::new(registry))
}

fn watcher(url: String, delay: Duration, registry: Arc<Mutex<Registry>>) -> Updates {
    Updates::new(Agent::ClaudeCode, test_source(url), CHECK_INTERVAL, delay, registry).unwrap()
}

/// Сервер выпусков с версиями 2.1.286 и 2.1.287 под тестовым ключом, наблюдатель за ним без задержки и каталог его
/// реестра.
async fn stand(stable: &str) -> (Releases, Updates, TempDir) {
    let (releases, url, dir) = server(stable).await;
    let updates = watcher(url, Duration::ZERO, open_registry(&dir));
    (releases, updates, dir)
}

async fn server(stable: &str) -> (Releases, String, TempDir) {
    let releases = Releases::default();
    releases.put("/stable", format!("{stable}\n"));
    releases.release("2.1.286", MANIFEST_286, SIGNATURE_286);
    releases.release("2.1.287", MANIFEST_287, SIGNATURE_287);
    let url = releases.serve().await;
    (releases, url, TempDir::new().unwrap())
}

#[tokio::test]
async fn real_manifest_of_2_1_285_signed_by_anthropic_is_offered_with_its_checksums() {
    let releases = Releases::default();
    releases.put("/stable", "2.1.285\n");
    releases.release("2.1.285", REAL_MANIFEST, REAL_SIGNATURE);
    let source = Source {
        base_url: releases.serve().await,
        ..claude::source()
    };
    let dir = TempDir::new().unwrap();
    let mut updates = Updates::new(Agent::ClaudeCode, source, CHECK_INTERVAL, Duration::ZERO, open_registry(&dir)).unwrap();

    let release = updates.check(NOW).await.unwrap().expect("подписанный манифест не принят");

    assert_eq!(release.version, "2.1.285");
    assert_eq!(release.agent, Agent::ClaudeCode);
    let sums: Vec<(Platform, &Digest)> = release
        .artifacts
        .iter()
        .map(|artifact| (artifact.platform, &artifact.digest))
        .collect();
    assert_eq!(
        sums,
        [
            (
                Platform::LinuxX64,
                &Digest::Sha256("33dad1ec615a2e08cc78b494f05c110e49916de2c79d78ec8799ebf46b233d29".to_owned())
            ),
            (
                Platform::LinuxArm64,
                &Digest::Sha256("24fac77749bed3d91365d6b6915aa4b824e14318ecb6bc17adbc192f01c9173d".to_owned())
            ),
        ],
        "контрольные суммы не те, что в манифесте"
    );
}

#[tokio::test]
async fn real_manifest_does_not_pass_under_a_key_that_did_not_sign_it() {
    let (releases, mut updates, _dir) = stand("2.1.285").await;
    releases.release("2.1.285", REAL_MANIFEST, REAL_SIGNATURE);

    assert!(updates.check(NOW).await.is_err(), "манифест принят под чужим ключом");
}

#[tokio::test]
async fn manifest_with_a_wrong_signature_is_not_offered_and_a_later_good_one_is() {
    let (releases, mut updates, _dir) = stand("2.1.286").await;
    releases.release("2.1.286", MANIFEST_286, SIGNATURE_287);

    assert!(updates.check(NOW).await.is_err(), "манифест с чужой подписью предложен");
    releases.release("2.1.286", MANIFEST_286, SIGNATURE_286);
    let release = updates.check(NOW).await.unwrap();

    assert_eq!(
        release.map(|release| release.version),
        Some("2.1.286".to_owned()),
        "верный манифест не предложен после отказа"
    );
}

#[tokio::test]
async fn checksums_changed_after_signing_are_not_trusted() {
    let (releases, mut updates, _dir) = stand("2.1.286").await;
    let forged = String::from_utf8(MANIFEST_286.to_vec())
        .unwrap()
        .replace(&checksum("2.1.286", "x64"), &"0".repeat(64));
    assert_ne!(forged.as_bytes(), MANIFEST_286, "подмена не изменила манифест");
    releases.release("2.1.286", forged.as_bytes(), SIGNATURE_286);

    assert!(
        updates.check(NOW).await.is_err(),
        "контрольные суммы взяты из манифеста с неверной подписью"
    );
}

#[tokio::test]
async fn signed_manifest_of_another_version_is_not_offered() {
    let (releases, mut updates, _dir) = stand("2.1.286").await;
    releases.release("2.1.286", MANIFEST_287, SIGNATURE_287);

    assert!(
        updates.check(NOW).await.is_err(),
        "манифест 2.1.287 принят вместо запрошенного 2.1.286"
    );
}

#[tokio::test]
async fn version_not_newer_than_the_offered_one_is_not_offered_again() {
    let (releases, mut updates, _dir) = stand("2.1.286").await;
    let first = updates.check(NOW).await.unwrap();
    assert_eq!(first.map(|release| release.version), Some("2.1.286".to_owned()));

    assert_eq!(updates.check(NOW).await.unwrap(), None, "та же версия предложена дважды");
    releases.put("/stable", "2.1.285\n");
    assert_eq!(
        updates.check(NOW).await.unwrap(),
        None,
        "более старая версия предложена после новой"
    );
    releases.put("/stable", "2.1.287\n");
    let next = updates.check(NOW).await.unwrap();
    assert_eq!(
        next.map(|release| release.version),
        Some("2.1.287".to_owned()),
        "новая версия не предложена"
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_check_never_requests_releases_or_needs_a_key() {
    let (releases, _updates, _dir) = stand("2.1.286").await;
    let (offers, seen) = watch::channel(None);

    Updates::start(true, Agent::ClaudeCode, offers, || {
        Err(anyhow::anyhow!("наблюдатель собран при выключенном слежении"))
    })
    .unwrap();
    tokio::time::advance(CHECK_INTERVAL * 12).await;
    tokio::task::yield_now().await;

    assert_eq!(
        releases.requests.load(Ordering::SeqCst),
        0,
        "выключенное слежение обратилось к серверу выпусков"
    );
    assert!(seen.borrow().is_none(), "выключенное слежение предложило версию");
}

#[tokio::test(start_paused = true)]
async fn enabled_check_requests_releases_and_offers_the_version() {
    let (releases, updates, _dir) = stand("2.1.286").await;
    let (offers, mut seen) = watch::channel(None);

    Updates::start(false, Agent::ClaudeCode, offers, || Ok(updates)).unwrap();
    seen.changed().await.unwrap();

    assert!(
        releases.requests.load(Ordering::SeqCst) > 0,
        "включённое слежение не обращалось к серверу"
    );
}

#[tokio::test(start_paused = true)]
async fn each_check_runs_an_interval_after_the_previous_one_and_publishes_only_new_versions() {
    let (releases, updates, _dir) = stand("2.1.286").await;
    let (offers, mut seen) = {
        let (tx, rx) = watch::channel(None);
        (tx, rx)
    };
    tokio::spawn(updates.run(offers));

    seen.changed().await.unwrap();
    assert_eq!(
        seen.borrow_and_update().as_ref().map(|release| release.version.as_str()),
        Some("2.1.286")
    );

    releases.put("/stable", "2.1.287\n");
    tokio::time::advance(CHECK_INTERVAL / 2).await;
    tokio::task::yield_now().await;
    assert!(!seen.has_changed().unwrap(), "версия проверена раньше, чем через интервал");

    tokio::time::advance(CHECK_INTERVAL).await;
    seen.changed().await.unwrap();
    assert_eq!(
        seen.borrow_and_update().as_ref().map(|release| release.version.as_str()),
        Some("2.1.287")
    );
}

const DELAY: Duration = Duration::from_hours(72);
const DELAY_SECS: i64 = 72 * 3600;

fn offered(release: Option<AgentRelease>) -> Option<String> {
    release.map(|release| release.version)
}

#[tokio::test]
async fn version_is_offered_only_after_the_delay_since_it_was_first_seen() {
    let (_releases, url, dir) = server("2.1.286").await;
    let mut updates = watcher(url, DELAY, open_registry(&dir));

    assert_eq!(updates.check(NOW).await.unwrap(), None, "версия предложена в момент появления");
    assert_eq!(
        updates.check(NOW + DELAY_SECS - 1).await.unwrap(),
        None,
        "версия предложена за секунду до конца задержки"
    );
    assert_eq!(
        offered(updates.check(NOW + DELAY_SECS).await.unwrap()),
        Some("2.1.286".to_owned()),
        "версия не предложена по истечении задержки"
    );
}

#[tokio::test]
async fn zero_delay_offers_the_version_at_once() {
    let (_releases, mut updates, _dir) = stand("2.1.286").await;

    assert_eq!(
        offered(updates.check(NOW).await.unwrap()),
        Some("2.1.286".to_owned()),
        "при нулевой задержке версия не предложена сразу"
    );
}

#[tokio::test]
async fn first_seen_time_survives_a_server_restart() {
    let (_releases, url, dir) = server("2.1.286").await;
    let mut before = watcher(url.clone(), DELAY, open_registry(&dir));
    assert_eq!(before.check(NOW).await.unwrap(), None);
    drop(before);

    let mut after = watcher(url, DELAY, open_registry(&dir));

    assert_eq!(
        offered(after.check(NOW + DELAY_SECS).await.unwrap()),
        Some("2.1.286".to_owned()),
        "после перезапуска отсчёт задержки начался заново"
    );
}

#[tokio::test]
async fn newer_version_counts_its_own_delay_and_the_withdrawn_one_is_not_offered() {
    let (releases, url, dir) = server("2.1.286").await;
    let mut updates = watcher(url, DELAY, open_registry(&dir));
    assert_eq!(updates.check(NOW).await.unwrap(), None);

    releases.put("/stable", "2.1.287\n");
    let newer_seen = NOW + 3600;
    assert_eq!(updates.check(newer_seen).await.unwrap(), None);
    assert_eq!(
        updates.check(NOW + DELAY_SECS).await.unwrap(),
        None,
        "новая версия предложена по отсчёту прежней"
    );
    assert_eq!(
        offered(updates.check(newer_seen + DELAY_SECS).await.unwrap()),
        Some("2.1.287".to_owned()),
        "новая версия не предложена по своему отсчёту"
    );

    releases.put("/stable", "2.1.285\n");
    assert_eq!(
        updates.check(newer_seen + 2 * DELAY_SECS).await.unwrap(),
        None,
        "версия, снятая из канала, предложена"
    );
}

#[tokio::test]
async fn version_dropped_from_the_channel_before_the_delay_is_never_offered() {
    let (releases, url, dir) = server("2.1.287").await;
    let mut updates = watcher(url, DELAY, open_registry(&dir));
    assert_eq!(updates.check(NOW).await.unwrap(), None);

    releases.put("/stable", "2.1.286\n");
    assert_eq!(updates.check(NOW + DELAY_SECS).await.unwrap(), None, "снятая версия предложена");
}
