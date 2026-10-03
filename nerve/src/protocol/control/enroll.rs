//! Вход узла: регистрация и продление сертификата.

use std::fmt;

use serde::{Deserialize, Serialize};

use russh::keys::PrivateKey;
use russh::keys::ssh_key::{HashAlg, LineEnding, SshSig};

use super::worker::WorkerCertificateRequest;
use crate::fingerprint::Fingerprint;
use crate::protocol::Error;

/// Запросы соединения `iapetus-enroll`: узел без действующего сертификата. Других запросов у входа нет.
/// Ответ — [`RegisterResponse`] на `Register`, [`RenewResponse`] на `Renew`, [`WorkerCertificateResponse`] на
/// `WorkerCertificate`; после ответа server закрывает соединение.
///
/// [`WorkerCertificateResponse`]: super::WorkerCertificateResponse
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnrollRequest {
    Register(RegisterRequest),
    Renew(RenewRequest),
    WorkerCertificate(WorkerCertificateRequest),
}

/// Первая регистрация waldo runner. waldo worker не регистрируется: личности у него нет.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Секрет действующего `orbit_key`.
    pub orbit_key: String,
    /// uuid4, созданный самим узлом при первом запуске.
    pub fingerprint: Fingerprint,
    /// Открытый ключ Ed25519 узла в формате OpenSSH; закрытая часть остаётся на узле.
    pub public_key: String,
    pub name: String,
    pub os: String,
    /// У клона — fingerprint родителя.
    pub parent: Option<Fingerprint>,
}

impl fmt::Debug for RegisterRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisterRequest")
            .field("orbit_key", &"<скрыт>")
            .field("fingerprint", &self.fingerprint)
            .field("public_key", &self.public_key)
            .field("name", &self.name)
            .field("os", &self.os)
            .field("parent", &self.parent)
            .finish()
    }
}

/// Ответ server на регистрацию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterReply {
    /// Сертификат SSH на ключ узла в формате OpenSSH, срок — 15 минут.
    pub certificate: String,
}

pub type RegisterResponse = Result<RegisterReply, Error>;

/// Пространство имён подписи запроса продления: подпись под другим назначением не годится.
pub const RENEW_NAMESPACE: &str = "iapetus-renew@v1";

/// Продление сертификата. Владение узлом доказывает подпись его ключа, право на сертификат —
/// действующий `orbit_key`: после выдачи нового узлы со старым продлить сертификат не могут.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewRequest {
    /// Секрет действующего `orbit_key`.
    pub orbit_key: String,
    pub fingerprint: Fingerprint,
    /// Секунды Unix на узле: server отвергает слишком старый и слишком далёкий запрос, поэтому
    /// перехваченный запрос не продлевает сертификат надолго.
    pub timestamp: u64,
    /// Подпись SSH (`SshSig`, [`RENEW_NAMESPACE`]) над [`RenewRequest::payload`].
    pub signature: Vec<u8>,
}

impl fmt::Debug for RenewRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenewRequest")
            .field("orbit_key", &"<скрыт>")
            .field("fingerprint", &self.fingerprint)
            .field("timestamp", &self.timestamp)
            .finish_non_exhaustive()
    }
}

impl RenewRequest {
    /// Запрос продления, подписанный ключом узла.
    pub fn signed(
        key: &PrivateKey,
        fingerprint: &Fingerprint,
        orbit_key: &str,
        timestamp: u64,
    ) -> Result<Self, russh::keys::ssh_key::Error> {
        let payload = Self::payload(fingerprint, timestamp);
        let signature = SshSig::sign(key, RENEW_NAMESPACE, HashAlg::Sha256, &payload)?;
        Ok(Self {
            orbit_key: orbit_key.to_owned(),
            fingerprint: fingerprint.clone(),
            timestamp,
            signature: signature.to_pem(LineEnding::LF)?.into_bytes(),
        })
    }

    /// Байты, которые подписывает узел: fingerprint и время запроса.
    pub fn payload(fingerprint: &Fingerprint, timestamp: u64) -> Vec<u8> {
        format!("{fingerprint}\n{timestamp}").into_bytes()
    }
}

/// Ответ server на продление.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewReply {
    /// Новый сертификат на тот же ключ в формате OpenSSH.
    pub certificate: String,
}

pub type RenewResponse = Result<RenewReply, Error>;
