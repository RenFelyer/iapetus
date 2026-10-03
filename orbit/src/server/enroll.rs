//! Регистрация runner, продление их сертификатов и сертификаты waldo worker поверх реестра и центра
//! сертификации.
//!
//! Сертификат runner выдаётся на открытый ключ узла и даёт только право войти на SSH orbit server. Любой
//! сертификат — первый и продлённый, runner и worker — выдаётся только по действующему `orbit_key`: после выдачи
//! нового ключа узлы со старым теряют доступ при ближайшем продлении. Продление runner вдобавок подписано его
//! закрытым ключом: `orbit_key` без ключа узла чужой fingerprint не присвоит.

#[cfg(test)]
#[path = "../../tests/server/enroll.rs"]
mod tests;

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{
    RENEW_NAMESPACE, RegisterReply, RegisterRequest, RenewReply, RenewRequest, WorkerCertificateReply, WorkerCertificateRequest,
};
use nerve::ssh::CERTIFICATE_LIFETIME;
use russh::keys::PublicKey;
use russh::keys::ssh_key::{Algorithm, SshSig};

use super::registry::{self, CertificateRecord, NewNode, OrbitKey, Registry};
use super::ssh::ca::{CertificateAuthority, Validity};

/// Наибольшее расхождение времени узла и server в запросе продления, в любую сторону.
pub(super) const MAX_REQUEST_SKEW: Duration = Duration::from_mins(5);

#[derive(Debug)]
pub(super) enum Error {
    Registry(registry::Error),
    /// Открытый ключ не читается как ключ OpenSSH или не Ed25519.
    PublicKey,
    /// Подпись запроса продления не разобрана или не сходится с ключом узла.
    Signature,
    /// Время запроса продления слишком далеко от времени server.
    Stale,
    /// Сертификат не удалось выпустить или записать.
    Certificate(String),
    /// Имя worker не годится в принципал: пустое, со служебными символами или служебное (`iapetus-…`).
    WorkerName,
    /// Файл первого `orbit_key` не записан.
    FirstKeyFile(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(e) => write!(f, "реестр: {e}"),
            Self::PublicKey => f.write_str("открытый ключ узла не Ed25519 в формате OpenSSH"),
            Self::Signature => f.write_str("подпись запроса не сходится с ключом узла"),
            Self::Stale => f.write_str("время запроса слишком далеко от времени server"),
            Self::Certificate(reason) => write!(f, "сертификат не выпущен: {reason}"),
            Self::WorkerName => f.write_str("имя worker не годится для входа"),
            Self::FirstKeyFile(e) => write!(f, "файл первого orbit_key не записан: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<registry::Error> for Error {
    fn from(e: registry::Error) -> Self {
        Self::Registry(e)
    }
}

/// Открытый ключ Ed25519 из текста OpenSSH.
fn ed25519(text: &str) -> Result<PublicKey, Error> {
    let public_key = PublicKey::from_openssh(text).map_err(|_| Error::PublicKey)?;
    if public_key.algorithm() != Algorithm::Ed25519 {
        return Err(Error::PublicKey);
    }
    Ok(public_key)
}

/// Регистрирует runner по `orbit_key` и выдаёт ему сертификат. Повтор от того же узла (тот же
/// fingerprint и ключ) выдаёт новый сертификат.
pub(super) fn register(
    registry: &mut Registry,
    ca: &CertificateAuthority,
    request: &RegisterRequest,
    now: SystemTime,
) -> Result<RegisterReply, Error> {
    registry.check_orbit_key(&OrbitKey::new(request.orbit_key.clone()))?;
    let public_key = ed25519(&request.public_key)?;
    registry.register_node(&NewNode {
        fingerprint: request.fingerprint.clone(),
        public_key: public_key.clone(),
        name: request.name.clone(),
        os: request.os.clone(),
        parent: request.parent.clone(),
    })?;
    let certificate = issue(registry, ca, &request.fingerprint, &public_key, now)?;
    Ok(RegisterReply { certificate })
}

/// Продлевает сертификат runner, подписавшего запрос своим ключом и предъявившего действующий `orbit_key`.
/// Узел, отозванный из реестра, не продлевается; срок прежнего сертификата значения не имеет.
pub(super) fn renew(
    registry: &mut Registry,
    ca: &CertificateAuthority,
    request: &RenewRequest,
    now: SystemTime,
) -> Result<RenewReply, Error> {
    let sent = UNIX_EPOCH.checked_add(Duration::from_secs(request.timestamp)).ok_or(Error::Stale)?;
    let skew = now.duration_since(sent).unwrap_or_else(|ahead| ahead.duration());
    if skew > MAX_REQUEST_SKEW {
        return Err(Error::Stale);
    }
    let public_key = registry.node_public_key(&request.fingerprint)?;
    let signature = SshSig::from_pem(&request.signature).map_err(|_| Error::Signature)?;
    let payload = RenewRequest::payload(&request.fingerprint, request.timestamp);
    public_key
        .verify(RENEW_NAMESPACE, &payload, &signature)
        .map_err(|_| Error::Signature)?;
    registry.check_orbit_key(&OrbitKey::new(request.orbit_key.clone()))?;
    let certificate = issue(registry, ca, &request.fingerprint, &public_key, now)?;
    Ok(RenewReply { certificate })
}

/// Сертификат waldo worker на его одноразовый ключ: по действующему `orbit_key`, сроком как у runner, принципал —
/// имя worker, с `permit-pty`. В реестр ничего не пишется: личности у worker нет.
pub(super) fn worker_certificate(
    registry: &Registry,
    ca: &CertificateAuthority,
    request: &WorkerCertificateRequest,
    now: SystemTime,
) -> Result<WorkerCertificateReply, Error> {
    registry.check_orbit_key(&OrbitKey::new(request.orbit_key.clone()))?;
    let public_key = ed25519(&request.public_key)?;
    let name = request.name.as_str();
    let printable = name.bytes().all(|b| b.is_ascii_graphic() && b != b',');
    // Все служебные имена входа начинаются с `iapetus-`.
    if name.is_empty() || !printable || name.starts_with("iapetus-") {
        return Err(Error::WorkerName);
    }
    let validity = Validity {
        now,
        lifetime: CERTIFICATE_LIFETIME,
    };
    let certificate = ca
        .issue_worker(&public_key, name, validity)
        .map_err(|e| Error::Certificate(e.to_string()))?;
    let certificate = certificate.to_openssh().map_err(|e| Error::Certificate(e.to_string()))?;
    Ok(WorkerCertificateReply { certificate })
}

/// Первый `orbit_key` системы: если реестр пуст, выдаёт ключ и пишет его в файл `path` с правами
/// `0600`; человек переносит его в waldo worker. При непустом реестре ничего не делает. Возвращает, записан ли файл.
pub(super) fn write_first_orbit_key(registry: &mut Registry, path: &Path, now: SystemTime) -> Result<bool, Error> {
    let now = unix_seconds(now)?;
    let Some(key) = registry.issue_first_orbit_key(now)? else {
        return Ok(false);
    };
    let text = format!("{}\n", key.expose());
    let written = (|| -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    })();
    written.map_err(Error::FirstKeyFile)?;
    Ok(true)
}

/// Удаляет файл первого `orbit_key` после первого использования ключа; файла нет — ничего не делает.
pub(super) fn forget_first_orbit_key(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!(path = %path.display(), "файл первого orbit_key удалён после первого входа"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "файл первого orbit_key не удалён"),
    }
}

fn unix_seconds(now: SystemTime) -> Result<i64, Error> {
    let since = now.duration_since(UNIX_EPOCH).map_err(|e| Error::Certificate(e.to_string()))?;
    i64::try_from(since.as_secs()).map_err(|e| Error::Certificate(e.to_string()))
}

fn issue(
    registry: &mut Registry,
    ca: &CertificateAuthority,
    fingerprint: &Fingerprint,
    public_key: &PublicKey,
    now: SystemTime,
) -> Result<String, Error> {
    let validity = Validity {
        now,
        lifetime: CERTIFICATE_LIFETIME,
    };
    let certificate = ca
        .issue_node(public_key, fingerprint.as_str(), validity)
        .map_err(|e| Error::Certificate(e.to_string()))?;
    let issued_at = now.duration_since(UNIX_EPOCH).map_err(|e| Error::Certificate(e.to_string()))?;
    let record = CertificateRecord {
        issued_at: i64::try_from(issued_at.as_secs()).map_err(|e| Error::Certificate(e.to_string()))?,
        valid_before: i64::try_from(certificate.valid_before()).map_err(|e| Error::Certificate(e.to_string()))?,
    };
    registry.record_certificate(fingerprint, record)?;
    certificate.to_openssh().map_err(|e| Error::Certificate(e.to_string()))
}
