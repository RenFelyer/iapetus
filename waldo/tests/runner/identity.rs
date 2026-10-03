//! Личность на настоящих файлах во временном каталоге. Время задаётся параметром, паузы не нужны.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve::connect::certificate::renewal_due;
use russh::keys::ssh_key::certificate::{Builder, CertType};
use tempfile::TempDir;

use super::*;

const MINUTE: u64 = 60;

fn unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000 + seconds)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

fn open(dir: &TempDir) -> Identity {
    Identity::load_or_create(&dir.path().join("id")).unwrap()
}

/// Сертификат, каким его выдаёт server: окно `valid_after`..`valid_before` с минутой допуска.
fn certificate_text(subject: &PrivateKey, key_id: &str, issued: u64, lifetime: u64) -> String {
    let ca = random_key();
    let mut builder = Builder::new(
        [0u8; 16],
        subject.public_key().key_data().clone(),
        unix(at(issued)) - MINUTE,
        unix(at(issued)) + lifetime,
    )
    .unwrap();
    builder.cert_type(CertType::User).unwrap();
    builder.key_id(key_id).unwrap();
    builder.valid_principal("iapetus").unwrap();
    builder.sign(&ca).unwrap().to_openssh().unwrap()
}

/// Сертификат на ключ самой личности на 15 минут от `issued`.
fn store(identity: &Identity, issued: u64) {
    let text = certificate_text(&identity.key, identity.fingerprint.as_str(), issued, 15 * MINUTE);
    identity.store_certificate(&text).unwrap();
}

// ---------------------------------------------------------------- создание и загрузка

#[test]
fn first_start_creates_a_uuid4_fingerprint_and_a_private_key_that_survive_restart() {
    let dir = TempDir::new().unwrap();
    let first = open(&dir);

    let text = first.fingerprint().as_str();
    let parts: Vec<_> = text.split('-').collect();
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        [8, 4, 4, 4, 12],
        "не uuid: {text}"
    );
    assert!(parts[2].starts_with('4'), "версия uuid не 4: {text}");
    assert!(
        "89ab".contains(parts[3].chars().next().unwrap()),
        "вариант uuid не RFC 4122: {text}"
    );
    assert_eq!(mode(&dir.path().join("id").join(KEY_FILE)), 0o600, "права ключа узла");
    assert_eq!(mode(&dir.path().join("id")), 0o700, "права каталога личности");

    let second = open(&dir);
    assert_eq!(second.fingerprint(), first.fingerprint(), "после перезапуска fingerprint другой");
    assert_eq!(
        second.public_key().unwrap(),
        first.public_key().unwrap(),
        "после перезапуска ключ другой"
    );
}

#[test]
fn key_file_readable_by_group_is_refused() {
    let dir = TempDir::new().unwrap();
    open(&dir);
    fs::set_permissions(dir.path().join("id").join(KEY_FILE), fs::Permissions::from_mode(0o640)).unwrap();

    assert!(
        Identity::load_or_create(&dir.path().join("id")).is_err(),
        "ключ с правами 640 принят"
    );
}

#[test]
fn key_file_readable_by_others_is_refused() {
    let dir = TempDir::new().unwrap();
    open(&dir);
    fs::set_permissions(dir.path().join("id").join(KEY_FILE), fs::Permissions::from_mode(0o604)).unwrap();

    assert!(
        Identity::load_or_create(&dir.path().join("id")).is_err(),
        "ключ с правами 604 принят"
    );
}

#[test]
fn corrupted_key_file_is_refused_and_left_alone() {
    let dir = TempDir::new().unwrap();
    open(&dir);
    let path = dir.path().join("id").join(KEY_FILE);
    fs::write(&path, "обрывок").unwrap();

    assert!(
        Identity::load_or_create(&dir.path().join("id")).is_err(),
        "повреждённый ключ принят"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "обрывок", "повреждённый ключ заменён новым");
}

#[test]
fn corrupted_fingerprint_file_is_refused_and_left_alone() {
    let dir = TempDir::new().unwrap();
    open(&dir);
    let path = dir.path().join("id").join(FINGERPRINT_FILE);
    for broken in ["", "с пробелом и юникодом", "all"] {
        fs::write(&path, broken).unwrap();

        assert!(
            Identity::load_or_create(&dir.path().join("id")).is_err(),
            "fingerprint {broken:?} принят"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "поломанный fingerprint заменён новым");
    }
}

#[test]
fn missing_fingerprint_is_created_without_touching_the_key() {
    let dir = TempDir::new().unwrap();
    let before = open(&dir).public_key().unwrap();
    fs::remove_file(dir.path().join("id").join(FINGERPRINT_FILE)).unwrap();

    let after = open(&dir);

    assert_eq!(after.public_key().unwrap(), before, "ключ пересоздан вместе с fingerprint");
}

// ---------------------------------------------------------------- сертификат и продление

#[test]
fn certificate_for_another_key_is_ignored() {
    let dir = TempDir::new().unwrap();
    let identity = open(&dir);
    let foreign = certificate_text(&random_key(), identity.fingerprint().as_str(), 0, 15 * MINUTE);
    fs::write(dir.path().join("id").join(CERTIFICATE_FILE), foreign).unwrap();

    assert!(identity.certificate().is_none(), "принят сертификат на чужой ключ");
    assert!(renewal_due(identity.certificate().as_ref(), at(0)));
}

#[test]
fn certificate_from_server_for_another_key_is_not_stored() {
    let dir = TempDir::new().unwrap();
    let identity = open(&dir);
    store(&identity, 0);
    let foreign = certificate_text(&random_key(), identity.fingerprint().as_str(), 10 * MINUTE, 15 * MINUTE);

    assert!(identity.store_certificate(&foreign).is_err(), "сертификат на чужой ключ сохранён");
    assert!(
        !renewal_due(identity.certificate().as_ref(), at(MINUTE)),
        "рабочий сертификат заменён чужим"
    );
}

#[test]
fn certificate_from_server_with_another_key_id_is_not_stored() {
    let dir = TempDir::new().unwrap();
    let identity = open(&dir);
    let other_id = certificate_text(&identity.key, "другой-узел", 0, 15 * MINUTE);

    assert!(identity.store_certificate(&other_id).is_err(), "сертификат с чужим key_id сохранён");
    assert!(identity.certificate().is_none());
}

#[test]
fn garbage_from_server_is_not_stored() {
    let dir = TempDir::new().unwrap();
    let identity = open(&dir);
    store(&identity, 0);

    assert!(identity.store_certificate("мусор").is_err());
    assert!(identity.certificate().is_some(), "рабочий сертификат потерян из-за мусора");
}

#[test]
fn orbit_key_is_kept_in_a_private_file_and_replaced_by_a_new_one() {
    let dir = TempDir::new().unwrap();
    assert_eq!(open(&dir).orbit_key().unwrap(), None);
    open(&dir).store_orbit_key("secret").unwrap();
    assert_eq!(mode(&dir.path().join("id").join(ORBIT_KEY_FILE)), 0o600, "права файла orbit_key");
    assert_eq!(open(&dir).orbit_key().unwrap().as_deref(), Some("secret"));
    open(&dir).store_orbit_key("newer").unwrap();
    assert_eq!(
        open(&dir).orbit_key().unwrap().as_deref(),
        Some("newer"),
        "новый orbit_key не заменил прежний"
    );
}

#[test]
fn orbit_key_file_readable_by_group_or_others_is_refused() {
    for loose in [0o640, 0o604] {
        let dir = TempDir::new().unwrap();
        open(&dir).store_orbit_key("secret").unwrap();
        fs::set_permissions(dir.path().join("id").join(ORBIT_KEY_FILE), fs::Permissions::from_mode(loose)).unwrap();
        assert!(open(&dir).orbit_key().is_err(), "orbit_key с правами {loose:o} принят");
    }
}

/// Каталог состояния пользователя и рабочие каталоги двух runner с одинаковым последним именем.
struct Machine {
    _tmp: TempDir,
    state: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let (state, a, b) = (root.join("state/iapetus"), root.join("srv/a"), root.join("home/alice/a"));
        for dir in [&a, &b] {
            fs::create_dir_all(dir).unwrap();
        }
        Self { _tmp: tmp, state, a, b }
    }

    /// Fingerprint runner, который запускается здесь в `workdir` с этим именем.
    #[track_caller]
    fn fingerprint(&self, workdir: &Path, name: &str) -> Fingerprint {
        let dir = locate(&self.state, workdir, name).unwrap();
        Identity::load_or_create(&dir).unwrap().fingerprint().clone()
    }

    /// Личность в каталоге `runner-<имя>`, общем для runner одного пользователя.
    fn legacy(&self, name: &str) -> (PathBuf, Fingerprint) {
        let dir = self.state.join(format!("runner-{name}"));
        let fingerprint = Identity::load_or_create(&dir).unwrap().fingerprint().clone();
        (dir, fingerprint)
    }
}

#[test]
fn runners_in_different_workdirs_get_different_identities() {
    let machine = Machine::new();
    // Одинаковое последнее имя каталога не сводит личности вместе: различает полный путь.
    assert_eq!(machine.a.file_name(), machine.b.file_name());
    assert_ne!(
        machine.fingerprint(&machine.a, "alice"),
        machine.fingerprint(&machine.b, "alice"),
        "два runner одного пользователя с разными рабочими каталогами получили одну личность"
    );
    assert_eq!(
        machine.fingerprint(&machine.a, "alice"),
        machine.fingerprint(&machine.a, "alice"),
        "перезапуск runner в том же каталоге сменил личность"
    );
}

#[test]
fn legacy_identity_goes_to_exactly_one_runner_of_its_name() {
    let machine = Machine::new();
    let (legacy_dir, legacy) = machine.legacy("alice");
    let (_, other) = machine.legacy("bob");

    assert_eq!(
        machine.fingerprint(&machine.a, "alice"),
        legacy,
        "первый runner не получил прежнюю личность"
    );
    assert!(!legacy_dir.exists(), "прежняя личность осталась и на старом месте");
    let second = machine.fingerprint(&machine.b, "alice");
    assert_ne!(second, legacy, "прежняя личность досталась второму runner");
    assert_ne!(second, other, "runner получил прежнюю личность другого имени");
}

#[test]
fn orbit_key_is_dropped_from_the_arguments_in_both_forms() {
    let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
    let restart = without_orbit_key(args(&[
        "waldo",
        "runner",
        "--orbit-key",
        "s:j",
        "/srv/site",
        "--orbit-key=s:j",
        "--name",
        "x",
    ]));
    assert_eq!(
        restart,
        args(&["waldo", "runner", "/srv/site", "--name", "x"]),
        "orbit_key остался в аргументах перезапуска"
    );
    // После `--` — значения, а не флаги: их убирать нельзя.
    assert_eq!(
        without_orbit_key(args(&["waldo", "runner", "--", "--orbit-key", "x"])),
        args(&["waldo", "runner", "--", "--orbit-key", "x"]),
        "аргументы после -- изменены"
    );
}
