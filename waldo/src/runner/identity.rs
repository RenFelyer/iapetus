//! Личность узла waldo: fingerprint, ключ Ed25519 и сертификат, выданный server.
//!
//! Всё лежит в одном каталоге, свой у каждого рабочего каталога: занятый каталог второму процессу не достаётся
//! (`flock`). fingerprint (uuid4) и ключ создаются при первом запуске и больше не меняются: повреждённый файл —
//! ошибка, а не повод создать узел заново. Закрытый ключ не покидает машину: server получает открытый ключ и подпись
//! запроса продления. Здесь же — `orbit_key`, ключ доступа к server: он хранится в каталоге, а из окружения и
//! аргументов процесса убирается перезапуском, иначе его видели бы команды runner.

#[cfg(test)]
#[path = "../../tests/runner/identity.rs"]
mod tests;

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use nerve::fingerprint::Fingerprint;
use nerve::protocol::Author;
use russh::keys::ssh_key::{Algorithm, LineEnding};
use russh::keys::{Certificate, PrivateKey};
use rustix::fs::{CWD, FlockOperation, RenameFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

const FINGERPRINT_FILE: &str = "fingerprint";
const KEY_FILE: &str = "node_key";
const CERTIFICATE_FILE: &str = "node_key-cert.pub";
const ORBIT_KEY_FILE: &str = "orbit_key";
const AUTHOR_FILE: &str = "author";

pub(super) struct Identity {
    dir: PathBuf,
    fingerprint: Fingerprint,
    key: PrivateKey,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("dir", &self.dir)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Читает личность из `dir`; чего нет — создаёт. Каталог создаётся с правами `0700`, ключ — `0600`.
    /// Ключ с правами шире `0600` отвергается.
    pub(super) fn load_or_create(dir: &Path) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("каталог личности {}", dir.display()))?;
        let fingerprint = load_or_create_file(&dir.join(FINGERPRINT_FILE), || Ok(new_fingerprint()?.to_string()))?;
        let fingerprint = Fingerprint::from_str(fingerprint.trim())
            .with_context(|| format!("файл {}: не fingerprint", dir.join(FINGERPRINT_FILE).display()))?;
        let key_path = dir.join(KEY_FILE);
        load_or_create_file(&key_path, || {
            let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
            Ok(key.to_openssh(LineEnding::LF)?.to_string())
        })?;
        let mode = fs::metadata(&key_path)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "ключ узла {} доступен группе или остальным (права {:o}), нужны 0600",
                key_path.display(),
                mode & 0o777
            );
        }
        let key = PrivateKey::read_openssh_file(&key_path).with_context(|| format!("ключ узла {}", key_path.display()))?;
        if key.algorithm() != Algorithm::Ed25519 {
            bail!("ключ узла {} не Ed25519", key_path.display());
        }
        Ok(Self {
            dir: dir.to_owned(),
            fingerprint,
            key,
        })
    }

    pub(super) fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// Закрытый ключ для входа по SSH-сертификату; наружу не отдаётся, живёт только в памяти соединения.
    pub(super) fn private_key(&self) -> PrivateKey {
        self.key.clone()
    }

    /// Открытый ключ в формате OpenSSH для регистрации.
    pub(super) fn public_key(&self) -> Result<String> {
        Ok(self.key.public_key().to_openssh()?)
    }

    /// Сохраняет сертификат от server. Чужой сертификат — на другой ключ или с другим `key_id` —
    /// не принимается: он бы заменил рабочий.
    pub(super) fn store_certificate(&self, openssh: &str) -> Result<()> {
        let certificate = Certificate::from_openssh(openssh).context("сертификат от server не разбирается")?;
        if certificate.public_key() != self.key.public_key().key_data() {
            bail!("сертификат от server выдан не на ключ этого узла");
        }
        if certificate.key_id() != self.fingerprint.as_str() {
            bail!("key_id сертификата от server не равен fingerprint узла");
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.write_all(openssh.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(CERTIFICATE_FILE))
            .map_err(|e| e.error)
            .context("сертификат не записан")?;
        Ok(())
    }

    /// Удаляет файлы личности; каталог остаётся.
    pub(super) fn remove(&self) -> Result<()> {
        for name in [AUTHOR_FILE, ORBIT_KEY_FILE, CERTIFICATE_FILE, KEY_FILE, FINGERPRINT_FILE] {
            let path = self.dir.join(name);
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("файл личности {} не удалён", path.display()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Запоминает `orbit_key` (файл `0600`): он нужен при регистрации и каждом продлении сертификата.
    pub(super) fn store_orbit_key(&self, orbit_key: &str) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        file.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(orbit_key.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(ORBIT_KEY_FILE))
            .map_err(|e| e.error)
            .context("orbit_key не записан")?;
        Ok(())
    }

    /// Сохранённый `orbit_key`; `None`, если его нет. Файл с правами шире `0600` отвергается, как и ключ узла.
    /// Сохраняет автора коммитов клона: имя и почта строками. Строка не может быть многострочной.
    pub(super) fn store_author(&self, author: &Author) -> Result<()> {
        if author.name.contains('\n') || author.email.contains('\n') {
            bail!("имя или почта автора многострочные");
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        write!(file, "{}\n{}\n", author.name, author.email)?;
        file.as_file().sync_all()?;
        file.persist(self.dir.join(AUTHOR_FILE))
            .map_err(|e| e.error)
            .context("автор не записан")?;
        Ok(())
    }

    /// Автор последней сессии клона; `None`, если сессии не было.
    pub(super) fn author(&self) -> Result<Option<Author>> {
        let path = self.dir.join(AUTHOR_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("файл {}", path.display())),
        };
        let mut lines = text.lines();
        let (Some(name), Some(email)) = (lines.next(), lines.next()) else {
            bail!("файл {} не содержит имя и почту автора", path.display());
        };
        Ok(Some(Author {
            name: name.to_owned(),
            email: email.to_owned(),
        }))
    }

    pub(super) fn orbit_key(&self) -> Result<Option<String>> {
        let path = self.dir.join(ORBIT_KEY_FILE);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("файл {}", path.display())),
        };
        let mode = file.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "orbit_key {} доступен группе или остальным (права {:o}), нужны 0600",
                path.display(),
                mode & 0o777
            );
        }
        let text = io::read_to_string(file).with_context(|| format!("файл {}", path.display()))?;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }

    /// Сохранённый сертификат; `None`, если файла нет, он не читается или сертификат выдан на
    /// другой ключ: узлу всё равно нужен новый.
    pub(super) fn certificate(&self) -> Option<Certificate> {
        let text = fs::read_to_string(self.dir.join(CERTIFICATE_FILE)).ok()?;
        let certificate = Certificate::from_openssh(&text).ok()?;
        (certificate.public_key() == self.key.public_key().key_data()).then_some(certificate)
    }
}

/// Переменная окружения и флаг с `orbit_key`.
const ORBIT_KEY_ENV: &str = "IAPETUS_ORBIT_KEY";
const ORBIT_KEY_FLAG: &str = "--orbit-key";

/// Файл блокировки в каталоге личности.
const LOCK_FILE: &str = "lock";

/// Блокировка каталога личности: пока файл открыт, второй процесс с этим каталогом не запустится. Снимается, когда
/// файл закрыт, в том числе с гибелью процесса; командам runner открытый файл не наследуется.
#[derive(Debug)]
pub(super) struct Lock {
    _file: fs::File,
}

/// Каталог `iapetus` в каталоге состояния пользователя.
pub(super) fn state_dir() -> Result<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(&std::env::var_os("HOME").context("HOME не задан: укажите --identity-dir")?).join(".local/state"),
    };
    Ok(state.join("iapetus"))
}

/// Каталог личности runner по умолчанию в `state`: по каноническому рабочему каталогу. Имя узла в него не входит:
/// переименование личность не меняет. Имя каталога — последнее имя рабочего каталога, чтобы человек его узнал, и хеш
/// полного пути, который различает одноимённые каталоги.
///
/// Каталог `runner-<имя узла>`, общий для runner одного пользователя, забирает себе первый runner с этим именем, у
/// которого своей личности ещё нет: перенос атомарен, поэтому личность не достанется двоим, а остальные
/// зарегистрируются заново. Копия дала бы двум runner один fingerprint и ключ.
pub(super) fn locate(state: &Path, workdir: &Path, name: &str) -> Result<PathBuf> {
    let hash = Sha256::digest(workdir.as_os_str().as_encoded_bytes());
    let label: String = workdir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .take(32)
        .collect();
    let dir = state.join("runner").join(format!("{label}-{}", nerve::hex::encode(&hash[..8])));
    let legacy = state.join(format!("runner-{name}"));
    if dir.symlink_metadata().is_err() && legacy.is_dir() {
        let parent = dir.parent().context("у каталога личности нет родителя")?;
        create_private_dir(parent)?;
        match rustix::fs::renameat_with(CWD, &legacy, CWD, &dir, RenameFlags::NOREPLACE) {
            Ok(()) => tracing::info!(from = %legacy.display(), to = %dir.display(), "личность прежней версии перенесена"),
            // Другой узел успел забрать прежнюю личность или создать эту.
            Err(Errno::NOENT | Errno::EXIST) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("личность {} не перенесена в {}", legacy.display(), dir.display()));
            }
        }
    }
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("каталог {} не создан", dir.display()))
}

/// Занимает каталог личности; занятый другим процессом — отказ.
pub(super) fn lock(dir: &Path) -> Result<Lock> {
    create_private_dir(dir)?;
    let path = dir.join(LOCK_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("файл блокировки {}", path.display()))?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Lock { _file: file }),
        Err(Errno::WOULDBLOCK) => bail!(
            "каталог личности {} занят другим процессом waldo runner: у каждого runner свой рабочий каталог; \
             другой каталог личности задаёт --identity-dir",
            dir.display()
        ),
        Err(e) => Err(e).with_context(|| format!("каталог личности {} не заблокирован", dir.display())),
    }
}

/// Удаляет личность клона, который перестал существовать: её файлы и каталог, если в нём больше ничего нет. Чужое
/// содержимое каталога не трогается.
pub(super) fn discard(dir: &Path, identity: &Identity, lock: Lock) -> Result<()> {
    identity.remove()?;
    match fs::remove_file(dir.join(LOCK_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context("файл блокировки личности не удалён"),
        _ => {}
    }
    drop(lock);
    fs::remove_dir(dir).with_context(|| format!("каталог личности {} не удалён", dir.display()))
}

/// Есть ли `orbit_key` в окружении или аргументах процесса.
pub(super) fn orbit_key_in_process() -> bool {
    std::env::var_os(ORBIT_KEY_ENV).is_some() || without_orbit_key(std::env::args_os()).len() != std::env::args_os().len()
}

/// Аргументы без `--orbit-key` и его значения; после `--` ничего не убирается.
fn without_orbit_key(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut kept = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            kept.push(arg);
            kept.extend(args);
            break;
        }
        if arg == ORBIT_KEY_FLAG {
            args.next();
        } else if !arg.as_encoded_bytes().starts_with(format!("{ORBIT_KEY_FLAG}=").as_bytes()) {
            kept.push(arg);
        }
    }
    kept
}

/// Перезапускает процесс тем же исполняемым файлом с теми же аргументами, но без `orbit_key` в окружении и
/// аргументах: ключ к этому моменту сохранён в каталоге личности. Окружение и аргументы, с которыми процесс запущен,
/// читаются из `/proc/<pid>/environ` и `cmdline` до его конца: `remove_var` их не меняет, а `execve` заменяет.
/// Возвращается только ошибка.
pub(super) fn exec_without_orbit_key() -> anyhow::Error {
    let mut args = without_orbit_key(std::env::args_os()).into_iter();
    let program = args.next().unwrap_or_default();
    let error = std::process::Command::new("/proc/self/exe")
        .arg0(program)
        .args(args)
        .env_remove(ORBIT_KEY_ENV)
        .exec();
    anyhow::Error::new(error).context("процесс не перезапущен без orbit_key")
}

/// uuid4: 122 случайных бита, версия 4 и вариант RFC 4122.
fn new_fingerprint() -> Result<Fingerprint> {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat();
    let text = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
    Ok(Fingerprint::from_str(&text)?)
}

/// Текст файла; если файла нет, создаёт его (`0600`) из `make` и возвращает созданный текст.
/// Файл появляется целиком и сразу: недописанного ключа или fingerprint не бывает. Если другой
/// процесс успел создать файл первым, берётся его содержимое.
fn load_or_create_file(path: &Path, make: impl FnOnce() -> Result<String>) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => return Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("файл {}", path.display())),
    }
    let text = make()?;
    let dir = path.parent().context("у файла личности нет каталога")?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => Ok(text),
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
            fs::read_to_string(path).with_context(|| format!("файл {}", path.display()))
        }
        Err(e) => Err(e.error).with_context(|| format!("файл {} не создан", path.display())),
    }
}
