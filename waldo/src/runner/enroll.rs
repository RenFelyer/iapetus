//! Личность runner и его вход на orbit server. Каталог личности — ключ узла, сертификат и `orbit_key` — занимается
//! при запуске; регистрация (`iapetus-enroll`) идёт при первом подключении, а продление и переподключение ведёт
//! `nerve::connect::server::Node`.

#[cfg(test)]
#[path = "../../tests/runner/enroll.rs"]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use nerve::connect;
use nerve::connect::server::{self, ConnectError, Node, Role, Server};
use nerve::fingerprint::Fingerprint;
use nerve::protocol::{EnrollRequest, Error, RegisterRequest, RegisterResponse};
use russh::keys::PublicKey;
use tokio::sync::watch;

use super::Args;
use super::identity::{self, Identity, Lock};

/// Узел с личностью: каталог занят (у runner), `orbit_key` известен, к server узел ещё не подключён.
pub(super) struct Enrolled {
    dir: PathBuf,
    lock: Lock,
    pub(super) identity: Arc<Identity>,
    orbit_key: String,
    pub(super) name: String,
}

fn node_name(args: &Args) -> Result<String> {
    match &args.name {
        Some(name) => Ok(name.clone()),
        None => connect::default_name(),
    }
}

/// Занимает каталог личности runner с рабочим каталогом `workdir`; к server не подключается.
pub(super) fn enroll(args: &Args, workdir: &Path) -> Result<Enrolled> {
    let name = node_name(args)?;
    let dir = match &args.identity_dir {
        Some(dir) => dir.clone(),
        None => identity::locate(&identity::state_dir()?, workdir, &name)?,
    };
    open(args, name, dir)
}

/// Личность в `dir`. Каталог занимается блокировкой, и `orbit_key` хранится в нём: из настроек — сохраняется, без них —
/// берётся сохранённый.
fn open(args: &Args, name: String, dir: PathBuf) -> Result<Enrolled> {
    let lock = identity::lock(&dir)?;
    let identity = Arc::new(Identity::load_or_create(&dir)?);
    let orbit_key = match &args.orbit_key {
        Some(key) => {
            identity.store_orbit_key(key.expose())?;
            key.expose().to_owned()
        }
        None => identity
            .orbit_key()?
            .context("orbit_key не задан и не сохранён в каталоге личности: задайте --orbit-key")?,
    };
    Ok(Enrolled {
        dir,
        lock,
        identity,
        orbit_key,
        name,
    })
}

/// Хост из адреса `хост:порт`: под этим именем server носит хост-сертификат.
fn host_of(address: &str) -> &str {
    address.rsplit_once(':').map_or(address, |(host, _)| host)
}

fn registration_error(error: &Error) -> anyhow::Error {
    match error {
        Error::Access => anyhow::anyhow!("server не принял orbit_key"),
        Error::Conflict => anyhow::anyhow!("fingerprint или ключ узла уже принадлежат другому узлу"),
        other => anyhow::anyhow!("регистрация отклонена: {other}"),
    }
}

impl Enrolled {
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(super) fn orbit_key(&self) -> &str {
        &self.orbit_key
    }

    /// Узел для канала управления. Без сертификата сначала регистрирует runner через `iapetus-enroll`.
    pub(super) async fn node(&self, args: &Args, parent: Option<Fingerprint>) -> Result<Node> {
        let ca =
            PublicKey::read_openssh_file(&args.orbit_ca).with_context(|| format!("ключ CA {} не прочитан", args.orbit_ca.display()))?;
        let server = Server {
            address: args.orbit_url.clone(),
            host: host_of(&args.orbit_url).to_owned(),
            ca,
        };
        if self.identity.certificate().is_none() {
            self.register(&server, parent).await?;
        }
        Ok(Node::new(
            server,
            self.identity.private_key(),
            self.orbit_key.clone(),
            Role::Runner {
                fingerprint: self.identity.fingerprint().clone(),
            },
            self.identity.certificate(),
        ))
    }

    async fn register(&self, server: &Server, parent: Option<Fingerprint>) -> Result<()> {
        let request = RegisterRequest {
            orbit_key: self.orbit_key.clone(),
            fingerprint: self.identity.fingerprint().clone(),
            public_key: self.identity.public_key()?,
            name: self.name.clone(),
            os: std::env::consts::OS.to_owned(),
            parent,
        };
        let response: RegisterResponse = server::enroll(server, &self.orbit_key, &EnrollRequest::Register(request))
            .await
            .map_err(|error| match error {
                ConnectError::Rejected | ConnectError::Denied => anyhow::anyhow!("server не принял orbit_key"),
                ConnectError::Other(error) => error.context(format!("регистрация на {} не удалась", server.address)),
            })?;
        let reply = response.map_err(|e| registration_error(&e))?;
        self.identity.store_certificate(&reply.certificate)?;
        tracing::info!(fingerprint = %self.identity.fingerprint(), name = %self.name, "узел зарегистрирован");
        Ok(())
    }

    /// Сохраняет в каталоге личности каждый сертификат, который `node` получает при продлении.
    pub(super) async fn store_certificates(&self, mut certificates: watch::Receiver<Option<russh::keys::Certificate>>) -> Result<()> {
        while certificates.changed().await.is_ok() {
            let Some(certificate) = certificates.borrow_and_update().clone() else {
                continue;
            };
            let text = certificate.to_openssh().context("сертификат не записывается")?;
            self.identity.store_certificate(&text)?;
        }
        bail!("узел перестал выдавать сертификаты")
    }

    /// Удаляет личность клона, который перестал существовать: она больше не понадобится.
    pub(super) fn discard_identity(self) -> Result<()> {
        identity::discard(&self.dir, &self.identity, self.lock)
    }
}
