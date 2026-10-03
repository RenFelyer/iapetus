//! Центр сертификации: подпись OpenSSH-сертификатов пользователей, узлов и хостов.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::ssh_key::{HashAlg, LineEnding};
use russh::keys::{Algorithm, Certificate, PrivateKey, PublicKey};

/// Расширение сертификата, которое разрешает pty в сессии.
const PERMIT_PTY: &str = "permit-pty";

use nerve::ssh::{CLOCK_SKEW, CONTROL_LOGIN, DATA_LOGIN, JUMP_LOGIN, WORKER_KEY_ID};

/// Момент выдачи и срок жизни сертификата.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Validity {
    pub now: SystemTime,
    pub lifetime: Duration,
}

impl Validity {
    pub(crate) fn from_now(lifetime: Duration) -> Self {
        Self {
            now: SystemTime::now(),
            lifetime,
        }
    }
}

/// Закрытый ключ центра сертификации. В `Debug` показывается только отпечаток.
pub(crate) struct CertificateAuthority {
    key: PrivateKey,
}

impl fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificateAuthority")
            .field("fingerprint", &self.key.fingerprint(HashAlg::Sha256).to_string())
            .finish()
    }
}

impl CertificateAuthority {
    /// Читает ключ из файла; файл, доступный группе или остальным, отвергается.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let mode = std::fs::metadata(path)
            .with_context(|| format!("ключ CA {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "ключ CA {} доступен группе или остальным (права {:o}), нужны 0600",
                path.display(),
                mode & 0o777
            );
        }
        let key = PrivateKey::read_openssh_file(path).with_context(|| format!("ключ CA {}", path.display()))?;
        Ok(Self { key })
    }

    /// Читает ключ или создаёт новый Ed25519 с правами `0600`, если файла нет.
    pub(crate) fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let pem = key.to_openssh(LineEnding::LF)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("ключ CA {}", path.display()))?;
        file.write_all(pem.as_bytes())?;
        Ok(Self { key })
    }

    pub(crate) fn public_key(&self) -> PublicKey {
        self.key.public_key().clone()
    }

    /// Пользовательский сертификат с заданными принципалами.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "нетестовый код выдаёт только сертификаты узлов; произвольные принципалы задают только тесты"
        )
    )]
    pub(crate) fn issue_user(&self, subject: &PublicKey, key_id: &str, principals: &[&str], validity: Validity) -> Result<Certificate> {
        self.sign(subject, CertType::User, key_id, principals, validity)
    }

    /// Сертификат waldo runner: `key_id` — fingerprint узла, принципалы — имена входа [`CONTROL_LOGIN`]
    /// (канал управления) и [`JUMP_LOGIN`] (jump) на SSH orbit server и [`DATA_LOGIN`] на сервер данных сессии.
    /// Сертификат даёт право только войти; куда вести дальше, решает server по реестру, а пускать ли
    /// дальше, решает сессия.
    pub(crate) fn issue_node(&self, subject: &PublicKey, fingerprint: &str, validity: Validity) -> Result<Certificate> {
        self.sign(
            subject,
            CertType::User,
            fingerprint,
            &[CONTROL_LOGIN, JUMP_LOGIN, DATA_LOGIN],
            validity,
        )
    }

    /// Сертификат waldo worker: `key_id` — [`WORKER_KEY_ID`], принципалы — [`CONTROL_LOGIN`], [`JUMP_LOGIN`] и
    /// имя worker (вход в сессию); разрешён pty. Личности у worker нет: ключ создан на один его запуск.
    pub(crate) fn issue_worker(&self, subject: &PublicKey, name: &str, validity: Validity) -> Result<Certificate> {
        self.sign_with(
            subject,
            CertType::User,
            WORKER_KEY_ID,
            &[CONTROL_LOGIN, JUMP_LOGIN, name],
            validity,
            &[PERMIT_PTY],
        )
    }

    /// Хост-сертификат: принципалы — имена, под которыми к хосту подключаются.
    pub(crate) fn issue_host(&self, subject: &PublicKey, key_id: &str, hostnames: &[&str], validity: Validity) -> Result<Certificate> {
        self.sign(subject, CertType::Host, key_id, hostnames, validity)
    }

    fn sign(&self, subject: &PublicKey, cert_type: CertType, key_id: &str, principals: &[&str], validity: Validity) -> Result<Certificate> {
        self.sign_with(subject, cert_type, key_id, principals, validity, &[])
    }

    fn sign_with(
        &self,
        subject: &PublicKey,
        cert_type: CertType,
        key_id: &str,
        principals: &[&str],
        validity: Validity,
        extensions: &[&str],
    ) -> Result<Certificate> {
        let now = validity.now.duration_since(UNIX_EPOCH)?;
        let valid_after = now.saturating_sub(CLOCK_SKEW).as_secs();
        let valid_before = (now + validity.lifetime).as_secs();
        let nonce: [u8; 16] = rand::random();
        let mut builder = Builder::new(nonce, subject.key_data().clone(), valid_after, valid_before)?;
        builder.cert_type(cert_type)?;
        builder.key_id(key_id)?;
        for principal in principals {
            builder.valid_principal(*principal)?;
        }
        for extension in extensions {
            builder.extension(*extension, "")?;
        }
        Ok(builder.sign(&self.key)?)
    }
}
