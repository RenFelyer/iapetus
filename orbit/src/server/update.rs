//! Слежение за выходом новых версий агентов: server раз в интервал читает версию канала агента, манифест выпуска и
//! подпись манифеста, проверяет подпись ключом поставщика и предлагает версию session этого агента. Контрольные
//! суммы берутся только из манифеста с верной подписью.
//!
//! Новая версия предлагается не сразу: server записывает в реестр, когда впервые увидел её в канале, и ждёт
//! задержку обновления — выпуск, который ломает совместимость, поставщик обычно снимает из канала за несколько
//! дней. У более новой версии отсчёт свой; снятая из канала версия не предлагается. Предложенная версия живёт в
//! памяти: после перезапуска server читает канал заново, а время первого появления берёт из реестра. Где искать выпуски агента и как читать его манифест, знает источник агента.

mod claude;
mod openpgp;

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use nerve::protocol::{Agent, AgentRelease};
use nerve::release;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

use self::openpgp::ReleaseKey;
use super::control::Control;
use super::registry::Registry;

/// Как часто server читает канал выпусков.
pub(super) const CHECK_INTERVAL: Duration = Duration::from_hours(1);

/// Сколько ждать ответа сервера выпусков.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Наибольший ответ сервера выпусков: манифест и подпись — килобайты.
const MAX_RESPONSE: usize = 1 << 20;

/// Текущее время в секундах Unix.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// Где агент публикует выпуски и как их читать.
#[derive(Debug)]
struct Source {
    /// Каталог выпусков: пути источника отсчитываются от него.
    base_url: String,
    /// Открытый ключ поставщика в ASCII-броне и отпечаток его первичного ключа.
    key: &'static str,
    fingerprint: &'static str,
    /// Путь файла с номером последней версии.
    channel: &'static str,
    /// Путь манифеста версии; подпись лежит рядом с суффиксом `.sig`.
    manifest: fn(&str) -> String,
    /// Выпуск из манифеста с проверенной подписью: манифест, версия, каталог выпусков.
    release: fn(&[u8], &str, &str) -> anyhow::Result<AgentRelease>,
}

/// Источник выпусков агента; `None` — server не следит за его версиями.
fn source(agent: Agent) -> Option<Source> {
    match agent {
        Agent::ClaudeCode => Some(claude::source()),
        Agent::Codex | Agent::Antigravity => None,
    }
}

/// Запускает по наблюдателю на каждого агента с источником, если слежение не выключено; выключенное не собирает
/// наблюдателей (и не требует ключей) и не обращается к сети.
pub(super) fn start_all(disabled: bool, delay: Duration, control: &Control, registry: &Arc<Mutex<Registry>>) -> anyhow::Result<()> {
    for agent in Agent::ALL {
        let (Some(source), Some(releases)) = (source(agent), control.agent_updates(agent)) else {
            continue;
        };
        Updates::start(disabled, agent, releases, || {
            Updates::new(agent, source, CHECK_INTERVAL, delay, Arc::clone(registry))
        })?;
    }
    Ok(())
}

/// Наблюдатель за выпусками одного агента.
#[derive(Debug)]
struct Updates {
    agent: Agent,
    client: reqwest::Client,
    source: Source,
    interval: Duration,
    /// Сколько версия должна пробыть в канале, прежде чем её предложат.
    delay: Duration,
    registry: Arc<Mutex<Registry>>,
    key: ReleaseKey,
    /// Самая новая из уже предложенных версий.
    offered: Option<[u64; 3]>,
}

impl Updates {
    /// Наблюдатель за выпусками `agent` из `source`, который проверяет их подписи раз в `interval` и предлагает
    /// версию через `delay` после того, как увидел её в канале.
    fn new(agent: Agent, mut source: Source, interval: Duration, delay: Duration, registry: Arc<Mutex<Registry>>) -> anyhow::Result<Self> {
        let trimmed = source.base_url.trim_end_matches('/').to_owned();
        source.base_url = trimmed;
        Ok(Self {
            agent,
            client: reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build()?,
            key: ReleaseKey::parse(source.key, source.fingerprint).context("вшитый ключ выпусков не разобран")?,
            source,
            interval,
            delay,
            registry,
            offered: None,
        })
    }

    /// Запускает слежение за `agent`, если оно не выключено.
    fn start(
        disabled: bool,
        agent: Agent,
        releases: watch::Sender<Option<AgentRelease>>,
        build: impl FnOnce() -> anyhow::Result<Self>,
    ) -> anyhow::Result<()> {
        if disabled {
            tracing::info!(agent = agent.id(), "слежение за версиями агента выключено");
            return Ok(());
        }
        tokio::spawn(build()?.run(releases));
        Ok(())
    }

    /// Проверяет выпуски каждый интервал и кладёт новую версию в `releases`. Не возвращается.
    #[expect(clippy::infinite_loop, reason = "слежение работает, пока работает server")]
    async fn run(mut self, releases: watch::Sender<Option<AgentRelease>>) -> Infallible {
        let agent = self.agent.id();
        loop {
            match self.check(unix_now()).await {
                Ok(Some(release)) => {
                    tracing::info!(agent, version = %release.version, "новая версия агента предложена session");
                    releases.send_replace(Some(release));
                }
                Ok(None) => tracing::debug!(agent, "новой версии агента нет"),
                Err(error) => tracing::warn!(agent, error = %format!("{error:#}"), "версия агента не предложена"),
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    /// Новая версия канала с проверенным манифестом; `None` — версия не новее предложенной или ещё не пробыла в
    /// канале задержку. `now` — секунды Unix.
    async fn check(&mut self, now: i64) -> anyhow::Result<Option<AgentRelease>> {
        let channel = self.source.channel;
        let latest = String::from_utf8(self.fetch(channel).await?).with_context(|| format!("канал {channel} не текст"))?;
        let version = latest.trim();
        let number = release::parse_version(version).with_context(|| format!("канал {channel} назвал версию {version:?}"))?;
        if self.offered.is_some_and(|offered| number <= offered) {
            return Ok(None);
        }

        let (first_seen, is_new) = self
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .version_first_seen(self.agent.id(), version, now)
            .context("время появления версии не записано")?;
        let offer_at = first_seen.saturating_add(i64::try_from(self.delay.as_secs()).unwrap_or(i64::MAX));
        if now < offer_at {
            if is_new {
                let after = OffsetDateTime::from_unix_timestamp(offer_at)
                    .ok()
                    .and_then(|time| time.format(&Rfc3339).ok())
                    .unwrap_or_else(|| offer_at.to_string());
                tracing::info!(
                    agent = self.agent.id(),
                    version,
                    %after,
                    "версия увидена, будет предложена после указанного времени"
                );
            }
            return Ok(None);
        }

        let path = (self.source.manifest)(version);
        let manifest = self.fetch(&path).await?;
        let signature = String::from_utf8(self.fetch(&format!("{path}.sig")).await?).context("подпись не текст")?;
        self.key.verify(&manifest, &signature).context("подпись манифеста не принята")?;
        let release = (self.source.release)(&manifest, version, &self.source.base_url)?;
        self.offered = Some(number);
        Ok(Some(release))
    }

    async fn fetch(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/{path}", self.source.base_url);
        let mut response = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("запрос {url}"))?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.with_context(|| format!("ответ {url} прерван"))? {
            body.extend(chunk);
            ensure!(body.len() <= MAX_RESPONSE, "ответ {url} больше {MAX_RESPONSE} байт");
        }
        Ok(body)
    }
}

#[cfg(test)]
#[path = "../../tests/server/update.rs"]
mod tests;
