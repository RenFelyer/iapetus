//! Регистрация и занятость каталога личности без сети; секрет `orbit_key` вне журнала.

use std::fs;
use std::time::{Duration, SystemTime};

use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::ssh_key::{Algorithm, PrivateKey};

use super::*;

fn args_without_network(orbit_key: Option<&str>, dir: &Path) -> Args {
    Args {
        workdir: PathBuf::from("."),
        env_file: PathBuf::from(".env"),
        orbit_url: "127.0.0.1:1".to_owned(),
        orbit_key: orbit_key.map(|key| key.parse().unwrap()),
        name: Some("node".to_owned()),
        identity_dir: Some(dir.to_owned()),
        orbit_ca: PathBuf::from("/nonexistent/ca.pub"),
        temporary: false,
        parent: None,
        supervised: None,
        home: true,
    }
}

/// Рабочий каталог runner в тестах с явным каталогом личности: на выбор каталога он тогда не влияет.
const WORKDIR: &str = "/srv/site";

/// Личность, уже зарегистрированная у server: сертификат на ключ узла и `orbit_key`. С ней runner занимает каталог без
/// сети.
fn registered(dir: &Path) -> Fingerprint {
    let identity = Identity::load_or_create(dir).unwrap();
    let ca = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key = PublicKey::from_openssh(&identity.public_key().unwrap()).unwrap();
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    let mut builder = Builder::new([0; 16], key.key_data().clone(), now - 60, now + 900).unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.key_id(identity.fingerprint().as_str()).unwrap();
    builder.valid_principal("iapetus").unwrap();
    identity
        .store_certificate(&builder.sign(&ca).unwrap().to_openssh().unwrap())
        .unwrap();
    identity.store_orbit_key("secret").unwrap();
    identity.fingerprint().clone()
}

/// Попытка занять каталог личности, который только что освободил этот же процесс; повторяется, пока каталог занят.
/// Другие тесты процесса порождают команды, и между `fork` и `exec` ребёнок держит копию открытого файла
/// блокировки: она закрывается с его `exec`, поэтому каталог освобождается не мгновенно.
fn when_released<T>(mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match attempt() {
            Err(e) if format!("{e:#}").contains("занят") && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            done => return done,
        }
    }
}

#[test]
fn second_runner_with_the_same_identity_dir_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("id");
    let fingerprint = registered(&dir);
    let args = args_without_network(None, &dir);
    let first = enroll(&args, Path::new(WORKDIR)).unwrap();
    assert_eq!(first.identity.fingerprint(), &fingerprint);

    // Блокировка — на открытом файле, а не на процессе: второй `enroll` здесь — то же, что второй процесс.
    let error = enroll(&args, Path::new(WORKDIR))
        .err()
        .expect("второй runner занял тот же каталог личности");
    assert!(format!("{error:#}").contains("занят"), "отказ не о занятом каталоге: {error:#}");

    drop(first);
    let again = when_released(|| enroll(&args, Path::new(WORKDIR))).unwrap();
    assert_eq!(
        again.identity.fingerprint(),
        &fingerprint,
        "после остановки первого runner каталог личности не освободился"
    );
}

#[test]
fn identity_of_a_finished_clone_is_removed_without_foreign_files() {
    let tmp = tempfile::tempdir().unwrap();
    let alone = tmp.path().join("alone");
    registered(&alone);
    let args = args_without_network(Some("secret"), &alone);
    let enrolled = enroll(&args, Path::new(WORKDIR)).unwrap();
    enrolled.discard_identity().unwrap();
    assert!(!alone.exists(), "каталог личности клона остался");

    let shared = tmp.path().join("shared");
    registered(&shared);
    fs::write(shared.join("notes.txt"), "чужое").unwrap();
    let enrolled = enroll(&args_without_network(None, &shared), Path::new(WORKDIR)).unwrap();
    assert!(enrolled.discard_identity().is_err(), "каталог с чужим файлом удалён без ошибки");
    assert_eq!(
        fs::read_dir(&shared).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>(),
        ["notes.txt"],
        "удалена не только личность клона"
    );
}

#[test]
fn orbit_key_is_hidden_in_debug_output() {
    let args = args_without_network(Some("secret-part"), Path::new("/x"));
    let text = format!("{args:?}");
    assert!(!text.contains("secret-part"), "orbit_key попал в Debug: {text}");
}
