//! Обновление агента: загрузка со сверкой sha256, ожидание зрителей и конца хода, повторы, выбор бинарника.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::http::{StatusCode, Uri};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::Instant;

use nerve::protocol::Artifact;

use super::*;

const NEW_VERSION: &str = "2.1.286";
const NEW_BINARY: &[u8] = b"#!/bin/sh\necho '2.1.286 (Claude Code)'\n";

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap_or_else(|_| panic!("за 10 секунд не случилось: {what}"))
}

/// Ждёт, не случится ли что-нибудь ещё: загрузка идёт по настоящему сокету, паузе часов её не подчинить.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(&Sha256::digest(bytes))
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn script(version: &str) -> String {
    format!("#!/bin/sh\necho '{version} (Claude Code)'\n")
}

fn write_executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Сервер выпусков: на любой адрес бинарника этой платформы отвечает `status` и `NEW_BINARY`.
struct Releases {
    status: Mutex<StatusCode>,
    requests: Mutex<Vec<Instant>>,
}

impl Releases {
    async fn serve() -> (Arc<Self>, String) {
        let releases = Arc::new(Self {
            status: Mutex::new(StatusCode::OK),
            requests: Mutex::new(Vec::new()),
        });
        let suffix = "/claude".to_owned();
        let shared = Arc::clone(&releases);
        let app = Router::new().fallback(move |uri: Uri| {
            let found = uri.path().ends_with(&suffix);
            shared.requests.lock().unwrap_or_else(PoisonError::into_inner).push(Instant::now());
            let status = *shared.status.lock().unwrap_or_else(PoisonError::into_inner);
            async move {
                if found {
                    (status, NEW_BINARY)
                } else {
                    (StatusCode::NOT_FOUND, &b""[..])
                }
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (releases, url)
    }

    fn set_status(&self, status: StatusCode) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }

    fn requests(&self) -> Vec<Instant> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

struct Fixture {
    dir: TempDir,
    binary: PathBuf,
    releases: Arc<Releases>,
    url: String,
    offers: watch::Sender<Option<Offer>>,
    viewers: watch::Sender<usize>,
    turn: watch::Sender<bool>,
    commands: mpsc::Receiver<AgentCommand>,
}

/// Установленный `claude` — скрипт, печатающий версию `installed`; `Updater` не запущен.
async fn bare(installed: &str) -> (Fixture, Updater) {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("claude");
    write_executable(&binary, &script(installed));
    let offers = watch::channel(None).0;
    let (viewers, viewers_rx) = watch::channel(0);
    let (turn, turn_rx) = watch::channel(false);
    let (commands_tx, commands) = mpsc::channel(4);
    let (releases, url) = Releases::serve().await;
    let updater = Updater {
        agent: Agent::ClaudeCode,
        offers: offers.clone(),
        viewers: viewers_rx,
        turn: turn_rx,
        commands: commands_tx,
        profile: UpdateProfile {
            installed_version: |output| output.split_whitespace().next().map(str::to_owned),
            install: Install::ReplaceFile,
        },
        program: binary.display().to_string(),
        search_path: None,
        owner: uid(),
    };
    let fixture = Fixture {
        dir,
        binary,
        releases,
        url,
        offers,
        viewers,
        turn,
        commands,
    };
    (fixture, updater)
}

/// То же, но `Updater` уже работает и ждёт указания.
async fn fixture(installed: &str) -> Fixture {
    let (fixture, updater) = bare(installed).await;
    tokio::spawn(updater.run());
    fixture
}

/// Один проход обоих шагов без повторов.
async fn attempt(updater: &Updater, offer: &Offer) -> Result<(), Failure> {
    match updater.prepare(offer).await? {
        Some(target) => updater.install(offer, &target).await,
        None => Ok(()),
    }
}

fn offer(url: &str, version: &str, sha256: &str) -> Offer {
    Offer {
        version: version.to_owned(),
        url: format!("{url}/{version}/claude"),
        sha256: sha256.to_owned(),
    }
}

impl Fixture {
    fn offer(&self, sha256: &str) {
        self.offers.send_replace(Some(offer(&self.url, NEW_VERSION, sha256)));
    }

    async fn offer_is_cleared(&self) {
        within("снятие ожидающего обновления", self.offers.subscribe().wait_for(Option::is_none))
            .await
            .unwrap();
    }

    async fn request(&mut self) -> UpdateRequest {
        within("указание агенту", self.commands.recv())
            .await
            .expect("очередь указаний закрыта")
            .into_update()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl AgentCommand {
    fn into_update(self) -> UpdateRequest {
        match self {
            Self::Update(request) => request,
            other => panic!("ждали Update, пришло {other:?}"),
        }
    }
}

#[tokio::test]
async fn verified_binary_is_staged_next_to_the_installed_one_and_install_replaces_it_atomically() {
    let mut f = fixture("2.1.280").await;
    f.offer(&sha256(NEW_BINARY));

    let request = f.request().await;

    assert_eq!(
        installed_version(&f.binary, |output| output.split_whitespace().next().map(str::to_owned))
            .await
            .unwrap(),
        "2.1.280",
        "установленный бинарник тронут до остановки агента"
    );
    assert_eq!(f.files().len(), 2, "загруженный бинарник лежит не рядом с установленным");
    request.install();
    f.offer_is_cleared().await;
    assert_eq!(std::fs::read(&f.binary).unwrap(), NEW_BINARY, "бинарник не заменён загруженным");
    assert_eq!(f.files(), ["claude"], "после замены остался временный файл");
    assert_eq!(
        std::fs::metadata(&f.binary).unwrap().permissions().mode() & 0o777,
        0o755,
        "у нового бинарника не те права"
    );
}

#[tokio::test]
async fn wrong_sha256_removes_the_download_clears_the_offer_and_leaves_the_binary_and_the_agent_alone() {
    let mut f = fixture("2.1.280").await;
    f.offer(&sha256(b"other"));

    f.offer_is_cleared().await;

    assert!(f.commands.try_recv().is_err(), "агенту отправлено обновление с неверной суммой");
    assert_eq!(f.files(), ["claude"], "временный файл с неверной суммой не удалён");
    assert_eq!(
        installed_version(&f.binary, |output| output.split_whitespace().next().map(str::to_owned))
            .await
            .unwrap(),
        "2.1.280",
        "бинарник заменён"
    );
}

#[tokio::test]
async fn the_binary_downloads_at_once_and_only_the_replacement_waits_for_the_worker_and_the_turn() {
    let mut f = fixture("2.1.280").await;
    f.viewers.send_replace(1);
    f.turn.send_replace(true);
    f.offer(&sha256(NEW_BINARY));

    settle().await;
    assert!(f.commands.try_recv().is_err(), "замена началась при подключённом worker");
    assert_eq!(f.files().len(), 2, "бинарник не загружен, пока worker подключён");

    f.viewers.send_replace(0);
    settle().await;
    assert!(f.commands.try_recv().is_err(), "замена началась посреди хода");

    f.turn.send_replace(false);
    f.request().await;
}

#[tokio::test]
async fn refused_replacement_waits_again_and_resends_the_same_download() {
    let mut f = fixture("2.1.280").await;
    f.offer(&sha256(NEW_BINARY));

    f.request().await.refuse();
    f.turn.send_replace(true);
    settle().await;
    assert!(f.commands.try_recv().is_err(), "замена предложена снова посреди хода");
    f.turn.send_replace(false);

    f.request().await.install();
    assert_eq!(f.releases.requests().len(), 1, "после отказа бинарник загружен заново");
    f.offer_is_cleared().await;
}

#[tokio::test]
async fn versions_are_compared_as_numbers_and_only_a_newer_one_is_installed() {
    // 2.1.10 новее 2.1.9, хотя как строка меньше.
    let mut newer = fixture("2.1.9").await;
    newer.offers.send_replace(Some(offer(&newer.url, "2.1.10", &sha256(NEW_BINARY))));
    newer.request().await;

    // 2.1.1000 новее 2.1.286, хотя как строка меньше.
    for installed in ["2.1.286", "2.1.290", "2.1.1000"] {
        let mut f = fixture(installed).await;
        f.offer(&sha256(NEW_BINARY));

        f.offer_is_cleared().await;

        assert!(
            f.commands.try_recv().is_err(),
            "установленная {installed} не старше {NEW_VERSION}, а обновление запущено"
        );
        assert!(f.releases.requests().is_empty(), "бинарник загружен при установленной {installed}");
        assert_eq!(f.files(), ["claude"]);
    }
}

#[tokio::test]
async fn failures_that_may_pass_are_transient_and_the_rest_are_rejected() {
    let sum = sha256(NEW_BINARY);

    let (f, updater) = bare("2.1.280").await;
    f.releases.set_status(StatusCode::SERVICE_UNAVAILABLE);
    let result = attempt(&updater, &offer(&f.url, NEW_VERSION, &sum)).await;
    assert!(
        matches!(result, Err(Failure::Transient(_))),
        "ответ 5xx не считается временным сбоем"
    );
    assert_eq!(f.files(), ["claude"], "после сбоя загрузки остался временный файл");

    let (f, updater) = bare("2.1.280").await;
    write_executable(&f.binary, "#!/bin/sh\nexit 1\n");
    let result = attempt(&updater, &offer(&f.url, NEW_VERSION, &sum)).await;
    assert!(
        matches!(result, Err(Failure::Transient(_))),
        "сбой claude --version не считается временным"
    );

    let (f, updater) = bare("2.1.280").await;
    let result = attempt(&updater, &offer(&f.url, NEW_VERSION, &sha256(b"other"))).await;
    assert!(matches!(result, Err(Failure::Rejected(_))), "неверная sha256 не отвергнута");
    assert_eq!(f.files(), ["claude"]);
}

#[tokio::test]
async fn failed_replacement_is_rejected_once_and_does_not_download_again() {
    let mut f = fixture("2.1.280").await;
    f.offer(&sha256(NEW_BINARY));

    let request = f.request().await;
    // На месте бинарника каталог: `rename` файла на него не удаётся.
    std::fs::remove_file(&f.binary).unwrap();
    std::fs::create_dir(&f.binary).unwrap();
    request.install();

    f.offer_is_cleared().await;
    settle().await;
    assert!(f.commands.try_recv().is_err(), "после сбоя замены агента остановили снова");
    assert_eq!(f.releases.requests().len(), 1, "после сбоя замены бинарник загружен заново");
}

#[tokio::test]
async fn replacement_is_dropped_if_the_installed_binary_became_current_while_waiting_for_idle() {
    let mut f = fixture("2.1.280").await;
    f.viewers.send_replace(1);
    f.offer(&sha256(NEW_BINARY));
    while f.releases.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle().await;

    // Пока worker смотрит терминал, бинарник обновил кто-то другой.
    write_executable(&f.binary, &script("2.1.300"));
    f.viewers.send_replace(0);

    f.offer_is_cleared().await;
    assert!(
        f.commands.try_recv().is_err(),
        "агента остановили ради замены на более старую версию"
    );
    assert_eq!(f.files(), ["claude"], "загруженный бинарник не удалён");
    assert_eq!(
        installed_version(&f.binary, |output| output.split_whitespace().next().map(str::to_owned))
            .await
            .unwrap(),
        "2.1.300"
    );
}

#[test]
fn retry_pauses_double_from_a_minute_and_stop_at_an_hour() {
    let seconds: Vec<u64> = (0..10).map(|attempt| retry_pause(attempt).as_secs()).collect();
    assert_eq!(seconds, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600, 3600]);
    assert_eq!(retry_pause(u32::MAX), RETRY_MAX, "огромное число сбоев переполняет паузу");
}

/// Шаг-замыкание отмечает время каждого вызова на замороженных часах: паузы между вызовами точные.
#[tokio::test]
async fn transient_failures_are_retried_with_growing_pauses_until_the_step_succeeds() {
    const FAILURES: u32 = 9;
    let (f, updater) = bare("2.1.280").await;
    tokio::time::pause();
    let calls = Mutex::new(Vec::new());
    let result = updater
        .retrying(&offer(&f.url, NEW_VERSION, &sha256(NEW_BINARY)), || {
            let mut calls = calls.lock().unwrap_or_else(PoisonError::into_inner);
            calls.push(Instant::now());
            let outcome = if calls.len() <= FAILURES as usize {
                Err(Failure::Transient(anyhow::anyhow!("сеть недоступна")))
            } else {
                Ok(7)
            };
            async move { outcome }.boxed()
        })
        .await;

    assert_eq!(result, Some(7), "после успеха шага нет результата");
    let calls = calls.into_inner().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(calls.len(), FAILURES as usize + 1, "шаг вызван не по разу на сбой и успех");
    // Таймер tokio округляет пробуждение вверх до миллисекунды, поэтому допуск — одна миллисекунда.
    for (attempt, pair) in calls.windows(2).enumerate() {
        let expected = retry_pause(u32::try_from(attempt).unwrap());
        let gap = pair[1] - pair[0];
        assert!(
            (expected..=expected + Duration::from_millis(1)).contains(&gap),
            "пауза перед повтором {attempt} равна {gap:?}, ожидалась {expected:?}"
        );
    }
}

#[tokio::test]
async fn rejected_step_is_not_retried() {
    let (f, updater) = bare("2.1.280").await;
    tokio::time::pause();
    let calls = Mutex::new(0);
    let started = Instant::now();
    let result: Option<()> = updater
        .retrying(&offer(&f.url, NEW_VERSION, &sha256(NEW_BINARY)), || {
            let mut calls = calls.lock().unwrap_or_else(PoisonError::into_inner);
            *calls += 1;
            // Повтор отвергнутого шага получил бы успех: тест видит его по результату, а не зависает.
            let outcome = if *calls == 1 {
                Err(Failure::Rejected(anyhow::anyhow!("неверная сумма")))
            } else {
                Ok(())
            };
            async move { outcome }.boxed()
        })
        .await;

    assert!(result.is_none(), "отвергнутое предложение дало результат");
    assert_eq!(*calls.lock().unwrap_or_else(PoisonError::into_inner), 1, "отвергнутый шаг повторён");
    assert_eq!(started.elapsed(), Duration::ZERO, "после отказа была пауза");
}

#[tokio::test]
async fn pending_offer_survives_a_failure_and_is_cleared_only_by_the_outcome() {
    let mut f = fixture("2.1.280").await;
    f.releases.set_status(StatusCode::SERVICE_UNAVAILABLE);
    f.offer(&sha256(NEW_BINARY));
    while f.releases.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    settle().await;
    assert!(f.offers.borrow().is_some(), "предложение снято после сбоя загрузки");
    assert!(f.commands.try_recv().is_err());
}

fn symlink_fixture() -> (TempDir, TempDir, PathBuf, PathBuf) {
    let real_dir = tempfile::tempdir().unwrap();
    let link_dir = tempfile::tempdir().unwrap();
    let real = real_dir.path().join("claude-real");
    write_executable(&real, &script("2.1.280"));
    let link = link_dir.path().join("claude");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    (real_dir, link_dir, real, link)
}

#[test]
fn symlink_is_resolved_and_its_target_is_what_gets_replaced() {
    let (_real_dir, _link_dir, real, link) = symlink_fixture();

    let target = replacement_target(&link, uid()).unwrap();

    assert_eq!(target, std::fs::canonicalize(&real).unwrap(), "заменяется не цель ссылки");
}

#[tokio::test]
async fn replacing_through_a_symlink_keeps_the_link_and_replaces_the_target() {
    let (real_dir, _link_dir, real, link) = symlink_fixture();
    let (mut f, mut updater) = bare("2.1.280").await;
    updater.program = link.display().to_string();
    let offer = offer(&f.url, NEW_VERSION, &sha256(NEW_BINARY));
    let attempt = attempt(&updater, &offer);
    let replace = async {
        let request = f.commands.recv().await.unwrap().into_update();
        request.install();
    };

    let (result, ()) = tokio::join!(attempt, replace);

    assert!(result.is_ok());
    assert!(
        std::fs::symlink_metadata(&link).unwrap().is_symlink(),
        "ссылка затёрта обычным файлом"
    );
    assert_eq!(std::fs::read(&real).unwrap(), NEW_BINARY, "цель ссылки не заменена");
    assert_eq!(
        std::fs::read_dir(real_dir.path()).unwrap().count(),
        1,
        "рядом с целью остался временный файл"
    );
}

#[test]
fn binary_in_a_directory_of_another_owner_or_open_for_writing_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("claude");
    write_executable(&binary, &script("2.1.280"));

    assert!(replacement_target(&binary, uid()).is_ok());
    assert!(replacement_target(&binary, uid() + 1).is_err(), "каталог чужого владельца принят");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        replacement_target(&binary, uid()).is_err(),
        "каталог, открытый для записи всем, принят"
    );
}

#[test]
fn something_that_is_not_an_executable_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "x").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();

    assert!(replacement_target(&plain, uid()).is_err(), "неисполняемый файл принят");
    assert!(replacement_target(&nested, uid()).is_err(), "каталог принят");
    assert!(
        replacement_target(&dir.path().join("absent"), uid()).is_err(),
        "несуществующий путь принят"
    );
}

#[test]
fn program_is_found_the_way_the_agent_finds_it() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    // В первом каталоге файл без права исполнения: оболочка его пропускает, а с ней и агент.
    std::fs::write(first.path().join("claude"), "x").unwrap();
    write_executable(&second.path().join("claude"), &script("2.1.280"));
    let path = std::env::join_paths([first.path(), second.path()]).unwrap();

    assert_eq!(resolve_program("claude", Some(&path)), Some(second.path().join("claude")));
    assert_eq!(resolve_program("claude", None), None);
    assert_eq!(
        resolve_program("/opt/claude", None),
        Some(PathBuf::from("/opt/claude")),
        "путь в команде ищется в PATH"
    );
}

fn release(artifact: Artifact) -> AgentRelease {
    AgentRelease {
        agent: Agent::ClaudeCode,
        version: "2.1.286".to_owned(),
        artifacts: vec![
            Artifact {
                platform: if artifact.platform == Platform::LinuxX64 {
                    Platform::LinuxArm64
                } else {
                    Platform::LinuxX64
                },
                url: "https://example.org/other".to_owned(),
                digest: ArtifactDigest::Sha256("1".repeat(64)),
                format: ArtifactFormat::Binary,
            },
            artifact,
        ],
    }
}

fn ours(url: &str, digest: ArtifactDigest, format: ArtifactFormat) -> Artifact {
    Artifact {
        platform: Platform::current().unwrap(),
        url: url.to_owned(),
        digest,
        format,
    }
}

#[test]
fn offer_takes_the_artifact_of_this_platform_and_refuses_what_is_unsafe_or_unsupported() {
    let good = "ab".repeat(32);
    let url = "https://example.org/2.1.286/claude";
    let binary = |digest| ours(url, digest, ArtifactFormat::Binary);

    let chosen = Offer::choose(&release(binary(ArtifactDigest::Sha256(good.clone())))).unwrap();
    assert_eq!(
        (chosen.sha256.as_str(), chosen.url.as_str()),
        (good.as_str(), url),
        "взят артефакт другой платформы"
    );

    let mut elsewhere = release(binary(ArtifactDigest::Sha256(good.clone())));
    elsewhere.artifacts.remove(1);
    assert!(
        Offer::choose(&elsewhere).is_err(),
        "артефакта этой платформы нет, а указание принято"
    );

    for version in ["../2.1.286", "2.1", "2.1.286/x", "", "2.1.x"] {
        let mut release = release(binary(ArtifactDigest::Sha256(good.clone())));
        release.version = version.to_owned();
        assert!(Offer::choose(&release).is_err(), "версия {version:?} принята");
    }
    for bad in ["ab", &"AB".repeat(32)] {
        let release = release(binary(ArtifactDigest::Sha256(bad.to_owned())));
        assert!(Offer::choose(&release).is_err(), "принята контрольная сумма {bad:?}");
    }
    for url in ["http://example.org/claude", "ftp://example.org/claude", "example.org/claude"] {
        let release = release(ours(url, ArtifactDigest::Sha256(good.clone()), ArtifactFormat::Binary));
        assert!(Offer::choose(&release).is_err(), "принят адрес {url:?}");
    }
    let tar = release(ours(url, ArtifactDigest::Sha256(good.clone()), ArtifactFormat::TarGz));
    assert!(Offer::choose(&tar).is_err(), "принят архив, который session не умеет ставить");
    let sha512 = release(binary(ArtifactDigest::Sha512("ab".repeat(64))));
    assert!(Offer::choose(&sha512).is_err(), "принята sha512, которую session не умеет сверять");
    let short = release(binary(ArtifactDigest::Sha512(good)));
    assert!(Offer::choose(&short).is_err(), "sha512 принята как sha256");
}
