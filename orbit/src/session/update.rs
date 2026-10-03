//! Обновление агента по указанию server. Бинарник загружается и сверяется по sha256 сразу, при работающем агенте;
//! заменяется после плановой остановки агента, когда к session не подключён ни один worker и основной ход закончен.
//! Условие проверяют дважды: Updater перед отправкой команды и `launch` перед остановкой агента. Предложение остаётся
//! ожидающим, пока оно не выполнено, не стало ненужным (установленная версия не старше предложенной) и не отвергнуто
//! (неверная sha256, негодный бинарник, неудавшаяся замена); сбой сети или загрузки до остановки агента повторяется
//! с растущей паузой.

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use nerve::agent::Agent;
use nerve::protocol::{AgentRelease, ArtifactFormat, Digest as ArtifactDigest, Platform};
use nerve::{hex, release};
use sha2::{Digest, Sha256};
use tempfile::TempPath;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot, watch};

use super::launch::AgentCommand;

/// Наибольший бинарник, который session примет: предел на случай подмены ответа.
const MAX_BINARY: u64 = 1 << 30;

/// Сколько ждать `--version` агента.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Сколько молчит загрузка, прежде чем её бросят.
const DOWNLOAD_IDLE: Duration = Duration::from_secs(60);

/// Первая пауза перед повтором после сбоя; каждая следующая вдвое длиннее, но не длиннее `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_hours(1);

/// Пауза после `attempt`-го сбоя подряд (с нуля): 60 с, 120 с, 240 с и так до часа.
fn retry_pause(attempt: u32) -> Duration {
    RETRY_FIRST.saturating_mul(2u32.saturating_pow(attempt)).min(RETRY_MAX)
}

/// Как профиль агента обновляется.
#[derive(Debug, Clone, Copy)]
pub(super) struct UpdateProfile {
    /// Версия из вывода `--version` агента.
    pub(super) installed_version: fn(&str) -> Option<String>,
    pub(super) install: Install,
}

/// Как загруженный артефакт встаёт на место установленного.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Install {
    /// Артефакт — исполняемый файл, он заменяет файл агента одним `rename`.
    ReplaceFile,
}

/// Принятое указание server: версия, адрес и контрольная сумма бинарника этой платформы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Offer {
    pub(super) version: String,
    pub(super) url: String,
    pub(super) sha256: String,
}

impl Offer {
    /// Выбирает из выпуска артефакт этой платформы. Принимается один исполняемый файл с sha256 по `https`.
    pub(super) fn choose(release: &AgentRelease) -> anyhow::Result<Self> {
        let version = &release.version;
        ensure!(release::parse_version(version).is_some(), "версия {version:?} не годится");
        let platform = Platform::current().with_context(|| format!("для архитектуры {} нет артефакта", std::env::consts::ARCH))?;
        let found = release
            .artifacts
            .iter()
            .find(|candidate| candidate.platform == platform)
            .with_context(|| format!("платформы {platform:?} нет в указании"))?;
        ensure!(
            found.format == ArtifactFormat::Binary,
            "формат {:?} не поддерживается",
            found.format
        );
        let ArtifactDigest::Sha256(sha256) = &found.digest else {
            bail!("формат не поддерживается: контрольная сумма не sha256");
        };
        ensure!(release::is_sha256(sha256), "контрольная сумма не sha256");
        ensure!(found.url.starts_with("https://"), "адрес артефакта {:?} не https", found.url);
        Ok(Self {
            version: version.clone(),
            url: found.url.clone(),
            sha256: sha256.clone(),
        })
    }
}

/// Загруженный и сверённый бинарник рядом с установленным: замена — одно `rename`. Без замены файл удаляется.
#[derive(Debug)]
pub(super) struct Staged {
    version: String,
    file: TempPath,
    target: PathBuf,
}

impl Staged {
    /// Бинарник `file` версии `version`, который встанет на место `target` одной заменой; `file` лежит в том же
    /// каталоге.
    pub(super) fn new(version: String, file: TempPath, target: PathBuf) -> Self {
        Self { version, file, target }
    }

    /// Атомарно ставит бинарник на место установленного.
    fn install(self) -> Result<(), String> {
        let Self { version, file, target } = self;
        file.persist(&target).map_err(|error| format!("версия {version}: {}", error.error))
    }
}

/// Ответ `launch` на указание обновить.
#[derive(Debug)]
pub(super) enum UpdateReply {
    Installed,
    /// Зритель или ход помешали остановить агента; бинарник возвращается, чтобы не загружать его заново.
    Busy(Staged),
    Failed(String),
}

/// Указание заменить бинарник и ответ на него.
#[derive(Debug)]
pub(super) struct UpdateRequest {
    staged: Staged,
    reply: oneshot::Sender<UpdateReply>,
}

impl UpdateRequest {
    pub(super) fn new(staged: Staged) -> (Self, oneshot::Receiver<UpdateReply>) {
        let (reply, receiver) = oneshot::channel();
        (Self { staged, reply }, receiver)
    }

    /// Заменяет бинарник и сообщает итог. Ждать некому, если обновление уже отменено, — это не ошибка.
    pub(super) fn install(self) {
        let reply = match self.staged.install() {
            Ok(()) => UpdateReply::Installed,
            Err(error) => UpdateReply::Failed(error),
        };
        drop(self.reply.send(reply));
    }

    /// Отказывает: агента сейчас останавливать нельзя.
    pub(super) fn refuse(self) {
        drop(self.reply.send(UpdateReply::Busy(self.staged)));
    }
}

/// Чем кончилась попытка обновления, если не успехом.
enum Failure {
    /// Предложение отвергнуто: повтор ничего не изменит.
    Rejected(anyhow::Error),
    /// Сбой, который может пройти: предложение остаётся, попытка повторяется.
    Transient(anyhow::Error),
}

fn transient(error: impl Into<anyhow::Error>) -> Failure {
    Failure::Transient(error.into())
}

fn rejected(error: impl Into<anyhow::Error>) -> Failure {
    Failure::Rejected(error.into())
}

/// Что нужно, чтобы выполнить обновление.
pub(super) struct Updater {
    /// Агент, которого обновляют: в журнале.
    pub(super) agent: Agent,
    /// Ожидающее обновление: указание server кладёт сюда новое, выполненное или отвергнутое Updater снимает.
    pub(super) offers: watch::Sender<Option<Offer>>,
    /// Сколько worker смотрят терминал агента.
    pub(super) viewers: watch::Receiver<usize>,
    /// Основной ход агента идёт.
    pub(super) turn: watch::Receiver<bool>,
    pub(super) commands: mpsc::Sender<AgentCommand>,
    pub(super) profile: UpdateProfile,
    /// Программа агента как в его команде и `PATH`, по которому её ищет агент.
    pub(super) program: String,
    pub(super) search_path: Option<OsString>,
    /// uid владельца каталога, где лежит заменяемый бинарник: root.
    pub(super) owner: u32,
}

impl Updater {
    /// Выполняет ожидающие обновления одно за другим; новое указание отменяет текущее. Не возвращается.
    pub(super) async fn run(self) {
        let mut offers = self.offers.subscribe();
        loop {
            let offer = {
                let Ok(pending) = offers.wait_for(Option::is_some).await else {
                    return std::future::pending().await;
                };
                pending.clone()
            };
            let Some(offer) = offer else { continue };
            tokio::select! {
                () = self.work(&offer) => self.finish(&offer),
                _ = offers.changed() => {}
            }
        }
    }

    /// Снимает обновление с ожидания, если за время работы оно не заменено новым.
    fn finish(&self, offer: &Offer) {
        self.offers.send_if_modified(|pending| {
            let done = pending.as_ref() == Some(offer);
            if done {
                *pending = None;
            }
            done
        });
    }

    /// Выполняет обновление в два шага — выбор файла с проверкой версии, затем загрузка и замена, — и каждый шаг повторяет
    /// после временного сбоя, пока не выйдет или предложение не будет отвергнуто.
    async fn work(&self, offer: &Offer) {
        let Some(Some(target)) = self.retrying(offer, || self.prepare(offer).boxed()).await else {
            return;
        };
        self.retrying(offer, || self.install(offer, &target).boxed()).await;
    }

    /// Повторяет `step` с растущей паузой после временного сбоя; `None` — предложение отвергнуто.
    async fn retrying<'a, T>(&self, offer: &Offer, mut step: impl FnMut() -> BoxFuture<'a, Result<T, Failure>>) -> Option<T> {
        let mut attempt = 0;
        loop {
            match step().await {
                Ok(done) => return Some(done),
                Err(Failure::Rejected(error)) => {
                    tracing::error!(agent = self.agent.id(), version = %offer.version, error = %format!("{error:#}"), "агент не обновлён");
                    return None;
                }
                Err(Failure::Transient(error)) => {
                    let pause = retry_pause(attempt);
                    tracing::warn!(agent = self.agent.id(), version = %offer.version, error = %format!("{error:#}"), ?pause, "агент не обновлён, повтор");
                    tokio::time::sleep(pause).await;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    /// Заменяемый файл, если предложенная версия новее установленной; `None` — обновлять нечего.
    async fn prepare(&self, offer: &Offer) -> Result<Option<PathBuf>, Failure> {
        let program = resolve_program(&self.program, self.search_path.as_deref())
            .with_context(|| format!("программа {} не найдена в PATH", self.program))
            .map_err(rejected)?;
        let target = replacement_target(&program, self.owner).map_err(rejected)?;
        Ok(self.is_newer(offer, &target).await?.then_some(target))
    }

    /// Новее ли предложенная версия установленной в `target`: версия читается из бинарника при каждом вызове.
    async fn is_newer(&self, offer: &Offer, target: &Path) -> Result<bool, Failure> {
        let offered = release::parse_version(&offer.version)
            .context("версия предложения не разобрана")
            .map_err(rejected)?;
        let installed = installed_version(target, self.profile.installed_version).await.map_err(transient)?;
        let installed = release::parse_version(&installed)
            .with_context(|| format!("--version назвал версию {installed:?}"))
            .map_err(rejected)?;
        if offered <= installed {
            tracing::debug!(agent = self.agent.id(), version = %offer.version, "установленный агент не старше предложенного");
        }
        Ok(offered > installed)
    }

    /// Загружает бинарник и заменяет им `target`, когда агента можно остановить.
    async fn install(&self, offer: &Offer, target: &Path) -> Result<(), Failure> {
        let mut staged = self.download(offer, target).await?;
        loop {
            self.wait_idle().await;
            // Пока ждали простоя, установленный бинарник мог смениться: агента останавливают, только если замена нужна.
            if !self.is_newer(offer, target).await? {
                return Ok(());
            }
            let (request, reply) = UpdateRequest::new(staged);
            self.commands
                .send(AgentCommand::Update(request))
                .await
                .map_err(|_| transient(anyhow::anyhow!("указание агенту не принято")))?;
            match reply.await {
                Ok(UpdateReply::Installed) => {
                    tracing::info!(agent = self.agent.id(), version = %offer.version, "агент обновлён");
                    return Ok(());
                }
                Ok(UpdateReply::Busy(back)) => staged = back,
                // Агент уже перезапущен со старым бинарником; повтор загрузил бы файл и остановил агента снова. Предложение
                // снимается, server пришлёт его при следующем подключении session.
                Ok(UpdateReply::Failed(error)) => return Err(rejected(anyhow::anyhow!("бинарник не заменён: {error}"))),
                Err(_) => return Err(transient(anyhow::anyhow!("агент не ответил на указание"))),
            }
        }
    }

    /// Ждёт, пока нет зрителей терминала и основной ход закончен.
    async fn wait_idle(&self) {
        let mut viewers = self.viewers.clone();
        let mut turn = self.turn.clone();
        loop {
            if *viewers.borrow_and_update() == 0 && !*turn.borrow_and_update() {
                return;
            }
            let closed = tokio::select! {
                changed = viewers.changed() => changed.is_err(),
                changed = turn.changed() => changed.is_err(),
            };
            if closed {
                return std::future::pending().await;
            }
        }
    }

    /// Загружает бинарник во временный файл рядом с `target` и сверяет sha256. Файл с неверной суммой или
    /// недогруженный удаляется.
    async fn download(&self, offer: &Offer, target: &Path) -> Result<Staged, Failure> {
        let directory = target.parent().context("у бинарника агента нет каталога").map_err(rejected)?;
        let url = &offer.url;
        let (file, path) = tempfile::Builder::new()
            .prefix(".agent-update-")
            .tempfile_in(directory)
            .with_context(|| format!("временный файл в {} не создан", directory.display()))
            .map_err(transient)?
            .into_parts();
        let mut file = tokio::fs::File::from_std(file);
        let client = reqwest::Client::builder().read_timeout(DOWNLOAD_IDLE).build().map_err(transient)?;
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("загрузка {url}"))
            .map_err(transient)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("загрузка {url} прервана"))
            .map_err(transient)?
        {
            size += chunk.len() as u64;
            if size > MAX_BINARY {
                return Err(rejected(anyhow::anyhow!("бинарник больше {MAX_BINARY} байт")));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(transient)?;
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(transient)?;
        file.sync_all().await.map_err(transient)?;
        let actual = hex::encode(&hasher.finalize());
        if actual != offer.sha256 {
            return Err(rejected(anyhow::anyhow!("sha256 бинарника {actual} не совпал с {}", offer.sha256)));
        }
        match self.profile.install {
            Install::ReplaceFile => Ok(Staged::new(offer.version.clone(), path, target.to_owned())),
        }
    }
}

/// Файл, который запустил бы агент: команда с путём — он сам, иначе первый исполняемый файл с таким именем в `search_path`.
fn resolve_program(program: &str, search_path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::split_paths(search_path?)
        .map(|directory| directory.join(program))
        .find(|candidate| std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0))
}

/// Файл, который заменяет обновление. Ссылка разрешается, заменяется её цель: обычный файл на месте ссылки агент
/// запускал бы мимо настоящего бинарника. Цель — исполняемый обычный файл в каталоге `owner`, закрытом для записи
/// другим: иначе замена открыла бы агенту подмену бинарника.
fn replacement_target(program: &Path, owner: u32) -> anyhow::Result<PathBuf> {
    let real = std::fs::canonicalize(program).with_context(|| format!("{} не разрешён", program.display()))?;
    let meta = std::fs::metadata(&real)?;
    ensure!(meta.is_file(), "{} не обычный файл", real.display());
    ensure!(meta.permissions().mode() & 0o111 != 0, "{} не исполняемый", real.display());
    let directory = real.parent().context("у бинарника нет каталога")?;
    let directory_meta = std::fs::metadata(directory)?;
    if directory_meta.uid() != owner || directory_meta.mode() & 0o022 != 0 {
        bail!("каталог {} не принадлежит root или открыт для записи другим", directory.display());
    }
    Ok(real)
}

/// Установленная версия: `parse` разбирает вывод `--version` агента.
async fn installed_version(binary: &Path, parse: fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let output = tokio::time::timeout(VERSION_TIMEOUT, tokio::process::Command::new(binary).arg("--version").output())
        .await
        .context("--version не ответил")?
        .with_context(|| format!("{} не запущен", binary.display()))?;
    ensure!(output.status.success(), "--version завершился с ошибкой: {}", output.status);
    parse(&String::from_utf8_lossy(&output.stdout)).context("--version не назвал версию")
}

#[cfg(test)]
#[path = "../../tests/session/update.rs"]
mod tests;
