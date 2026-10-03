//! Жизненный цикл контейнеров orbit session: server создаёт, запускает, восстанавливает и удаляет их через
//! свой прокси Docker.
//!
//! Контейнер появляется только по запросу worker ([`Lifecycle::start`]). У него свой том состояния агента: том
//! переживает контейнер, поэтому новый контейнер на том же томе с `IAPETUS_RESUME=1` продолжает разговор. Агент,
//! размер `/work` и fingerprint сессии лежат в метках тома и после потери контейнера читаются оттуда.
//! Контейнер, которого нет, server восстанавливает сам ([`Lifecycle::restore_missing`]); из `failure` сессию
//! поднимает только запрос worker ([`Lifecycle::restore_failed`]).
//!
//! Секреты идут в окружении контейнера (`*_CONTENT`: значения, не пути к файлам) — другого канала без `exec` и
//! bind нет. Учётных данных агента среди них нет: он входит сам, учётные данные живут в томе состояния.
//! Контейнеру достаются его секрет для входа на внутренний слушатель server, ключ и хост-сертификат канала
//! данных от CA server и открытый ключ CA; каждый запуск выдаёт новый секрет, прежний перестаёт действовать.
//!
//! Журнал контейнера (его stdout и stderr) пропадает вместе с контейнером, поэтому перед удалением server сохраняет
//! его в `session-logs/` каталога состояния: хранятся последние [`KEPT_LOGS`] файлов, у каждого — хвост не больше
//! [`MAX_LOG_BYTES`].

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use nerve::fingerprint::Fingerprint;
use nerve::protocol::Agent;
use nerve::session_state::SessionState;
use nerve::ssh::DATA_HOST;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};

use super::docker::{self, Docker, Limits, SessionSpec, Template};
use super::mcp::Tokens;
use super::registry::{Registry, SessionSecret};
use super::ssh::ca::{CertificateAuthority, Validity};

#[cfg(test)]
#[path = "../../tests/server/lifecycle.rs"]
mod tests;

const CONTAINER_PREFIX: &str = "iapetus-session-";
const VOLUME_PREFIX: &str = "iapetus-state-";
const SESSION_LABEL: &str = "iapetus.session";
const AGENT_LABEL: &str = "iapetus.agent";
const WORK_LABEL: &str = "iapetus.work";

/// Срок хост-сертификата канала данных: он живёт столько же, сколько контейнер, а ключ уходит вместе с ним.
const HOST_CERTIFICATE_LIFETIME: Duration = Duration::from_hours(24 * 365);
/// Сколько контейнер получает на завершение, прежде чем его убьют.
const STOP_TIMEOUT_SECONDS: u32 = 10;
/// Каталог журналов удалённых контейнеров в каталоге состояния.
const LOGS_DIR: &str = "session-logs";
/// Сколько файлов журналов остаётся в каталоге: старшие удаляются.
const KEPT_LOGS: usize = 20;
/// Наибольший размер одного журнала, байты: сохраняется хвост.
const MAX_LOG_BYTES: usize = 16 << 20;
/// Сколько последних строк журнала просить у Docker.
const LOG_TAIL_LINES: u32 = 100_000;
/// Как часто server ищет сессии, потерявшие контейнер.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Образ orbit session по digest (`репозиторий@sha256:…`); без него server контейнеров не поднимает
    #[arg(long, env = "IAPETUS_SESSION_IMAGE")]
    session_image: Option<String>,

    /// Адрес внутреннего слушателя server (`хост:порт`), по которому session входит к нему
    #[arg(long, env = "IAPETUS_SESSION_ORBIT_URL", requires = "session_image")]
    session_orbit_url: Option<String>,

    /// Адрес MCP-сервера server (`http://хост:порт/mcp`), по которому агент session ходит к инструментам Iapetus
    #[arg(long, env = "IAPETUS_SESSION_MCP_URL", requires = "session_image")]
    session_mcp_url: Option<String>,

    /// Сокет Docker, к которому ходит прокси server
    #[arg(long, env = "IAPETUS_DOCKER_SOCKET", default_value = "/var/run/docker.sock")]
    docker_socket: PathBuf,

    /// Пользовательская сеть Docker, в которую попадают контейнеры session
    #[arg(long, env = "IAPETUS_SESSION_NETWORK", default_value = "iapetus-sessions")]
    session_network: String,

    /// Профиль `AppArmor` контейнера session, загруженный на хосте
    #[arg(long, env = "IAPETUS_SESSION_APPARMOR")]
    session_apparmor: Option<String>,

    /// Память контейнера session, байты
    #[arg(long, env = "IAPETUS_SESSION_MEMORY", default_value_t = 4 << 30)]
    session_memory: i64,

    /// CPU контейнера session, миллиардные доли ядра
    #[arg(long, env = "IAPETUS_SESSION_NANO_CPUS", default_value_t = 2_000_000_000)]
    session_nano_cpus: i64,

    /// Число процессов контейнера session
    #[arg(long, env = "IAPETUS_SESSION_PIDS", default_value_t = 512)]
    session_pids: i64,

    /// Наибольший и умолчательный размер tmpfs `/work`, байты
    #[arg(long, env = "IAPETUS_SESSION_WORK_MAX", default_value_t = 1 << 30)]
    session_work_max: i64,
}

/// Запускает прокси Docker на сокете в каталоге состояния и цикл восстановления; `None`, если образ не задан.
pub(crate) fn start(
    args: Args,
    data_dir: &Path,
    registry: Arc<Mutex<Registry>>,
    ca: Arc<CertificateAuthority>,
    mcp_tokens: Arc<Tokens>,
) -> Result<Option<Arc<Lifecycle>>> {
    let Some(image) = args.session_image else {
        tracing::info!("образ session не задан: server контейнеров не поднимает");
        return Ok(None);
    };
    let orbit_url = args.session_orbit_url.context("IAPETUS_SESSION_ORBIT_URL не задан")?;
    let mcp_url = args.session_mcp_url.context("IAPETUS_SESSION_MCP_URL не задан")?;
    let limits = Limits {
        memory: args.session_memory,
        nano_cpus: args.session_nano_cpus,
        pids: args.session_pids,
        work: args.session_work_max,
    };
    let template = Template::new(vec![image.clone()], args.session_network, limits, args.session_apparmor)?;
    let proxy_socket = data_dir.join("docker-proxy.sock");
    let listener = docker::bind(&proxy_socket).with_context(|| format!("сокет прокси Docker {}", proxy_socket.display()))?;
    tokio::spawn(docker::serve(listener, args.docker_socket, template.clone()));
    let settings = Settings {
        image,
        orbit_url,
        mcp_url,
        mcp_tokens,
        work_max: args.session_work_max,
        logs_dir: data_dir.join(LOGS_DIR),
    };
    let lifecycle = Arc::new(Lifecycle::new(Docker::new(proxy_socket), template, registry, ca, settings));
    tokio::spawn(Arc::clone(&lifecycle).keep_containers());
    Ok(Some(lifecycle))
}

#[derive(Debug)]
pub(crate) struct Settings {
    pub image: String,
    /// Адрес внутреннего слушателя server для session.
    pub orbit_url: String,
    /// Адрес MCP-сервера server для агента session.
    pub mcp_url: String,
    /// Подпись токенов MCP: у каждой сессии свой токен.
    pub mcp_tokens: Arc<Tokens>,
    /// Размер `/work`, когда запрос его не назвал, и верхняя граница шаблона.
    pub work_max: i64,
    /// Куда сохраняются журналы удалённых контейнеров.
    pub logs_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Lifecycle {
    docker: Docker,
    template: Template,
    registry: Arc<Mutex<Registry>>,
    ca: Arc<CertificateAuthority>,
    settings: Settings,
    /// Сессии, над которыми сейчас идёт операция: вторая операция над той же сессией отказывает.
    busy: Mutex<HashSet<Fingerprint>>,
}

/// Операция над сессией; пока жив, другие операции над ней отказывают.
struct Claim<'a> {
    lifecycle: &'a Lifecycle,
    session: Fingerprint,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.lifecycle
            .busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

fn container_name(session: &Fingerprint) -> String {
    format!("{CONTAINER_PREFIX}{session}")
}

fn volume_name(session: &Fingerprint) -> String {
    format!("{VOLUME_PREFIX}{session}")
}

/// Агент и размер `/work`, которые сессия получила при создании.
fn read_labels(labels: &BTreeMap<String, String>) -> Result<(Agent, i64)> {
    let agent = labels
        .get(AGENT_LABEL)
        .context("в метках тома нет агента")?
        .parse()
        .context("в метках тома неизвестный агент")?;
    let work = labels
        .get(WORK_LABEL)
        .and_then(|work| work.parse().ok())
        .context("в метках тома нет размера /work")?;
    Ok((agent, work))
}

/// Пишет журнал `session` в `dir` как `<fingerprint>-<секунды Unix>.log` с правами `0600` и оставляет в каталоге не
/// больше `keep` самых новых файлов. Журнал длиннее `limit` байт усекается с начала до границы строки, перед ним
/// ставится строка-пометка.
fn store_log(dir: &Path, session: &Fingerprint, log: &[u8], at: u64, keep: usize, limit: usize) -> std::io::Result<PathBuf> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let path = dir.join(format!("{session}-{at}.log"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    let cut = log.len().saturating_sub(limit);
    if cut > 0 {
        let tail = &log[cut..];
        let line_start = tail.iter().position(|byte| *byte == b'\n').map_or(0, |newline| newline + 1);
        writeln!(file, "[журнал усечён: начало до {} байт отброшено]", cut + line_start)?;
        file.write_all(&tail[line_start..])?;
    } else {
        file.write_all(log)?;
    }
    drop(file);
    prune_logs(dir, keep)?;
    Ok(path)
}

/// Удаляет самые старые журналы, пока их больше `keep`; возраст — секунды в имени файла.
fn prune_logs(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut logs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let at = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".log"))
            .and_then(|name| name.rsplit_once('-'))
            .and_then(|(_, seconds)| seconds.parse::<u64>().ok());
        if let Some(at) = at {
            logs.push((at, path));
        }
    }
    logs.sort();
    let extra = logs.len().saturating_sub(keep);
    for (_, path) in logs.into_iter().take(extra) {
        fs::remove_file(path)?;
    }
    Ok(())
}

impl Lifecycle {
    pub(crate) fn new(
        docker: Docker,
        template: Template,
        registry: Arc<Mutex<Registry>>,
        ca: Arc<CertificateAuthority>,
        settings: Settings,
    ) -> Self {
        Self {
            docker,
            template,
            registry,
            ca,
            settings,
            busy: Mutex::default(),
        }
    }

    /// Поднимает новую сессию из реестра: том, контейнер, запуск. `work_bytes` — размер `/work`, без него —
    /// наибольший из разрешённых.
    pub(crate) async fn start(&self, session: &Fingerprint, agent: Agent, runner: &Fingerprint, work_bytes: Option<i64>) -> Result<()> {
        let _claim = self.claim(session)?;
        self.registry()
            .session(session)
            .with_context(|| format!("сессия {session} не найдена"))?;
        let work_bytes = work_bytes.unwrap_or(self.settings.work_max);
        ensure!(
            (1..=self.settings.work_max).contains(&work_bytes),
            "размер /work {work_bytes} вне 1..={}",
            self.settings.work_max
        );
        let labels = BTreeMap::from([
            (SESSION_LABEL.to_owned(), session.to_string()),
            (AGENT_LABEL.to_owned(), agent.id().to_owned()),
            (WORK_LABEL.to_owned(), work_bytes.to_string()),
        ]);
        self.docker.create_volume(&volume_name(session), &labels).await?;
        self.launch(session, agent, runner, work_bytes, false).await
    }

    /// Поднимает сессию из `failure` по запросу worker: новый контейнер на том же томе с `resume`, состояние `waiting`.
    pub(crate) async fn restore_failed(&self, session: &Fingerprint) -> Result<()> {
        let _claim = self.claim(session)?;
        let info = self
            .registry()
            .session(session)
            .with_context(|| format!("сессия {session} не найдена"))?;
        ensure!(info.state == SessionState::Failure, "сессия {session} не в failure");
        self.recreate(session).await
    }

    /// Завершает сессию: останавливает и удаляет контейнер и том, затем запись в реестре.
    pub(crate) async fn remove(&self, session: &Fingerprint) -> Result<()> {
        let _claim = self.claim(session)?;
        let name = container_name(session);
        self.docker.stop_container(&name, STOP_TIMEOUT_SECONDS).await?;
        self.save_logs(session).await;
        self.docker.remove_container(&name).await?;
        self.docker.remove_volume(&volume_name(session)).await?;
        self.registry()
            .remove_session(session)
            .with_context(|| format!("сессия {session} не удалена из реестра"))
    }

    /// Восстанавливает сессии, у которых пропал контейнер, кроме сессий в `failure`: те ждут запроса worker.
    pub(crate) async fn restore_missing(&self) -> Result<()> {
        let sessions = self.registry().sessions().context("сессии не прочитаны")?;
        let present = self.docker.container_names().await?;
        for info in sessions {
            let session = info.fingerprint;
            if info.state == SessionState::Failure || present.contains(&container_name(&session)) {
                continue;
            }
            let Ok(_claim) = self.claim(&session) else { continue };
            // Операция могла закончиться между списком и захватом.
            if self.docker.container_names().await?.contains(&container_name(&session)) {
                continue;
            }
            if self.docker.volume_labels(&volume_name(&session)).await?.is_none() {
                tracing::debug!(%session, "у сессии нет тома: контейнер ещё не создавался");
                continue;
            }
            match self.recreate(&session).await {
                Ok(()) => tracing::info!(%session, "контейнер сессии восстановлен"),
                Err(error) => tracing::warn!(%session, error = %format!("{error:#}"), "контейнер сессии не восстановлен"),
            }
        }
        Ok(())
    }

    /// Не даёт сессиям остаться без контейнера, пока работает server.
    async fn keep_containers(self: Arc<Self>) -> ! {
        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(error) = self.restore_missing().await {
                tracing::warn!(error = %format!("{error:#}"), "контейнеры сессий не сверены");
            }
        }
    }

    /// Новый контейнер на томе сессии для её runner из реестра; прежний, если остался, удаляется.
    async fn recreate(&self, session: &Fingerprint) -> Result<()> {
        let labels = self
            .docker
            .volume_labels(&volume_name(session))
            .await?
            .with_context(|| format!("тома сессии {session} нет: разговор не продолжить"))?;
        let (agent, work_bytes) = read_labels(&labels)?;
        let runner = self
            .registry()
            .session(session)
            .with_context(|| format!("сессия {session} не найдена"))?
            .runner
            .with_context(|| format!("сессии {session} не назначен runner"))?;
        self.save_logs(session).await;
        self.docker.remove_container(&container_name(session)).await?;
        // Новый контейнер начинает с `waiting`, пока агент не получит первый ответ: состояние прежнего
        // контейнера сбрасывается заранее.
        self.registry()
            .reset_session_state(session)
            .with_context(|| format!("состояние сессии {session} не сброшено"))?;
        self.launch(session, agent, &runner, work_bytes, true).await
    }

    /// Сохраняет журнал контейнера сессии перед его удалением. Сбой остаётся в журнале server и удалению не мешает.
    async fn save_logs(&self, session: &Fingerprint) {
        let name = container_name(session);
        let log = match self.docker.container_logs(&name, LOG_TAIL_LINES).await {
            Ok(Some(log)) => log,
            Ok(None) => return,
            Err(error) => return tracing::warn!(%session, error = %format!("{error:#}"), "журнал контейнера session не прочитан"),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
        match store_log(&self.settings.logs_dir, session, &log, now, KEPT_LOGS, MAX_LOG_BYTES) {
            Ok(path) => tracing::info!(%session, path = %path.display(), bytes = log.len(), "журнал контейнера session сохранён"),
            Err(error) => tracing::warn!(%session, %error, "журнал контейнера session не сохранён"),
        }
    }

    async fn launch(&self, session: &Fingerprint, agent: Agent, runner: &Fingerprint, work_bytes: i64, resume: bool) -> Result<()> {
        let secret = self
            .registry()
            .issue_session_secret(session)
            .with_context(|| format!("секрет сессии {session} не выдан"))?;
        let env = self.environment(session, agent, runner, &secret, resume)?;
        let spec = SessionSpec {
            image: &self.settings.image,
            labels: BTreeMap::from([(SESSION_LABEL.to_owned(), session.to_string())]),
            agent,
            env,
            volume: &volume_name(session),
            work_bytes,
        };
        let name = container_name(session);
        self.docker.create_container(&name, &self.template.create_request(&spec)).await?;
        self.docker.start_container(&name).await
    }

    /// Окружение контейнера: только то, что относится к этой сессии.
    fn environment(
        &self,
        session: &Fingerprint,
        agent: Agent,
        runner: &Fingerprint,
        secret: &SessionSecret,
        resume: bool,
    ) -> Result<Vec<String>> {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let host_certificate = self.ca.issue_host(
            host_key.public_key(),
            session.as_str(),
            &[DATA_HOST],
            Validity::from_now(HOST_CERTIFICATE_LIFETIME),
        )?;
        let mut env = vec![
            format!("IAPETUS_SESSION={session}"),
            format!("IAPETUS_RUNNER={runner}"),
            format!("IAPETUS_ORBIT_URL={}", self.settings.orbit_url),
            format!("IAPETUS_MCP_URL={}", self.settings.mcp_url),
            format!("IAPETUS_MCP_TOKEN={}", self.settings.mcp_tokens.issue(session)),
            format!("IAPETUS_SESSION_SECRET_CONTENT={}", secret.expose()),
            format!("IAPETUS_CA_PUBLIC_KEY_CONTENT={}", self.ca.public_key().to_openssh()?),
            format!("IAPETUS_DATA_HOST_KEY_CONTENT={}", host_key.to_openssh(LineEnding::LF)?.as_str()),
            format!("IAPETUS_DATA_HOST_CERT_CONTENT={}", host_certificate.to_openssh()?),
            format!("IAPETUS_AGENT={}", agent.id()),
            "IAPETUS_START_AGENT=true".to_owned(),
        ];
        if resume {
            env.push("IAPETUS_RESUME=1".to_owned());
        }
        Ok(env)
    }

    fn claim(&self, session: &Fingerprint) -> Result<Claim<'_>> {
        if !self.busy.lock().unwrap_or_else(PoisonError::into_inner).insert(session.clone()) {
            bail!("с сессией {session} уже идёт другая операция");
        }
        Ok(Claim {
            lifecycle: self,
            session: session.clone(),
        })
    }

    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
