//! Политика доверия к сертификатам SSH: пользовательским (их принимают jump и сессия) и хост-сертификатам
//! (их проверяет runner).
//!
//! `russh` сверяет подпись сертификата с ключом из самого сертификата и окно
//! `valid_after`/`valid_before`. Доверие к CA, тип, принципалы и critical options проверяются здесь;
//! у пользовательских сертификатов окно ещё и не больше `MAX_CERTIFICATE_WINDOW`.

use russh::keys::ssh_key::certificate::CertType;
use russh::keys::ssh_key::{Fingerprint, HashAlg};
use russh::keys::{Certificate, PublicKey};

use super::MAX_CERTIFICATE_WINDOW;

#[derive(Debug)]
pub struct Trust {
    ca: Vec<Fingerprint>,
}

impl Trust {
    pub fn new(ca_keys: &[PublicKey]) -> Self {
        let ca = ca_keys.iter().map(|key| key.fingerprint(HashAlg::Sha256)).collect();
        Self { ca }
    }

    /// Причина отказа или `Ok`. `login` — имя входа из запроса, `now` — секунды Unix.
    pub fn check_user_cert(&self, cert: &Certificate, login: &str, now: u64) -> Result<(), &'static str> {
        self.check_window_and_signature(cert, now)?;
        if cert.cert_type() != CertType::User {
            return Err("сертификат не пользовательский");
        }
        if cert.valid_before() - cert.valid_after() > MAX_CERTIFICATE_WINDOW.as_secs() {
            return Err("срок жизни больше допустимого");
        }
        if !cert.valid_principals().iter().any(|principal| principal == login) {
            return Err("имени входа нет среди принципалов");
        }
        if !cert.critical_options().is_empty() {
            return Err("есть critical options");
        }
        Ok(())
    }

    /// Причина отказа или `Ok`. `host` — имя, под которым подключились, `now` — секунды Unix.
    pub fn check_host_cert(&self, cert: &Certificate, host: &str, now: u64) -> Result<(), &'static str> {
        self.check_window_and_signature(cert, now)?;
        if cert.cert_type() != CertType::Host {
            return Err("сертификат не хост-сертификат");
        }
        if !cert.valid_principals().iter().any(|principal| principal == host) {
            return Err("имени хоста нет среди принципалов");
        }
        Ok(())
    }

    fn check_window_and_signature(&self, cert: &Certificate, now: u64) -> Result<(), &'static str> {
        if now < cert.valid_after() {
            return Err("срок ещё не наступил");
        }
        if now >= cert.valid_before() {
            return Err("срок истёк");
        }
        cert.validate_at(now, &self.ca).map_err(|_| "подпись неверна или CA не доверен")?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/ssh/trust.rs"]
mod tests;
