//! Управление orbit session: указания server и состояние сессии.

use serde::{Deserialize, Serialize};

use super::runner::Assignment;
use crate::agent::Agent;
use crate::fingerprint::Fingerprint;
use crate::protocol::{Error, McpCatalog, RunnerHost};
use crate::session_state::SessionState;

/// Запрос server к session: указание. Ответ — [`ControlResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Назначить runner (`None` — снять назначение) и сказать, что делать с агентом.
    AssignRunner { runner: Option<AssignedRunner>, agent: AgentRun },
    /// Перезапустить агента с продолжением разговора, когда закончится основной ход. Если предыдущий `Resume` ещё
    /// ждёт конца хода, агент перезапускается сразу, ход обрывается.
    Resume,
    /// Новая версия агента: сколько ждёт обновление, решает session. Артефакты по платформам, подписанный выпуск
    /// проверил server.
    UpdateAgent(AgentRelease),
    /// Сводка проектных MCP-серверов runner целиком; пустая снимает прежнюю.
    McpCatalog(McpCatalog),
}

/// Выпуск агента, который server предлагает session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRelease {
    /// Агент, которому предназначен выпуск.
    pub agent: Agent,
    pub version: String,
    /// Артефакты по платформам.
    pub artifacts: Vec<Artifact>,
}

/// Артефакт выпуска для одной платформы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub platform: Platform,
    /// Откуда session загружает артефакт; принимается только `https://`.
    pub url: String,
    pub digest: Digest,
    pub format: ArtifactFormat,
}

/// Платформа, для которой собран артефакт.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    LinuxX64,
    LinuxArm64,
}

impl Platform {
    /// Платформа этого процесса; `None` — архитектура без артефактов.
    #[must_use]
    pub fn current() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Self::LinuxX64),
            "aarch64" => Some(Self::LinuxArm64),
            _ => None,
        }
    }
}

/// Контрольная сумма артефакта: шестнадцатеричная строка из строчных цифр.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Digest {
    Sha256(String),
    Sha512(String),
}

/// Как устроен загруженный артефакт.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactFormat {
    /// Один исполняемый файл.
    Binary,
    /// Архив `tar.gz` с деревом агента.
    TarGz,
}

/// Что делать с агентом при указании `AssignRunner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRun {
    /// Не трогать: так server сообщает назначение после подключения session.
    Keep,
    /// Остановить планово и не запускать, пока server не назначит runner заново: runner пропал надолго или
    /// назначения нет. Без runner в указании агент останавливается всегда.
    Stop,
    /// Остановить планово и сразу запустить заново на этом runner; `resume` — с продолжением разговора.
    Restart { resume: bool },
}

/// Runner, назначенный session, номер этой пары и сведения о runner для правил агента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignedRunner {
    pub runner: Fingerprint,
    pub assignment: Assignment,
    pub name: String,
    pub os: String,
    /// Путь проекта, hostname, `/etc/os-release` и `$HOME` runner: session воспроизводит их для агента.
    pub host: RunnerHost,
    /// Имена переменных окружения, которые runner даёт командам (слои `all`, родителя и свой); значений нет.
    pub env_names: Vec<String>,
}

pub type ControlResponse = Result<(), Error>;

/// Окно лимита подписки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Длина окна, секунды; `None` — сервис её не называет.
    pub duration: Option<u64>,
    /// Модель или семейство моделей; `None` — вся подписка.
    pub scope: Option<String>,
    /// Доля использованного, от 0 до 1.
    pub used: f32,
    /// Когда окно сбросится, секунды Unix.
    pub resets_at: Option<u64>,
}

/// Чем занят агент сверх основного хода: сколько запросов подагентов и сжатий истории идёт сейчас.
/// Счётчики, а не флаги: подагенты работают параллельно.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub subagents: u32,
    pub compactions: u32,
}

/// Вид причины состояния сессии: по нему server и worker решают, что делать, не разбирая текст.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cause {
    /// Агент запущен, сервис ещё не принимал учётные данные.
    AwaitingFirstResponse,
    /// Агент остановлен планово (перезапуск через `resume`, обновление) и поднимется снова.
    Restarting,
    /// Сервис отверг учётные данные или их нет: пользователь подключается к агенту и входит.
    LoginRequired,
    RunnerOffline,
    /// Агент остановлен и ждёт назначения runner.
    RunnerRequired,
    NoConnection,
    /// Лимит подписки исчерпан; момент сброса — в `limit_reset`, если сервис его сообщил.
    LimitReached,
    /// Основной ход закончился ошибкой, которую агент не повторяет.
    RequestFailed,
    ContextOverflow,
    /// Процесс агента завершается снова и снова сразу после запуска: session перестала перезапускать его сама.
    AgentExited,
    /// Процесс агента завершился сам (например, пользователь набрал `/exit`), session запускает его заново с
    /// продолжением разговора.
    AgentRestarted,
}

/// Состояние сессии для server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: SessionState,
    /// Почему такое состояние, словами; видно человеку в TUI и в `session_status`.
    pub reason: Option<String>,
    /// Почему такое состояние, видом; `None` — состояние штатное.
    pub cause: Option<Cause>,
    /// Когда сбросится исчерпанный лимит подписки, секунды Unix; только при `cause == LimitReached`.
    pub limit_reset: Option<u64>,
    pub runner: Option<Fingerprint>,
    pub activity: Activity,
    /// Агент запущен в pty: worker можно пускать в терминал.
    pub agent_running: bool,
    /// Окна лимитов подписки; пусто, пока сервис их не сообщил.
    pub limits: Vec<UsageWindow>,
    /// Перезапуск агента затребован и ждёт конца основного хода.
    pub restart_pending: bool,
}

/// Запросы session к server по внутреннему слушателю. Ответ на `Hello` и `Status` — `Result<(), Error>`,
/// поток [`DocsEvent`] на `Docs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionRequest {
    /// Первое сообщение после подключения: полное состояние.
    Hello(Status),
    /// Новое состояние при каждой смене.
    Status(Status),
    /// Подписка на документы: снимок, затем изменения.
    Docs,
}

/// Общий документ для правил Iapetus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub name: String,
    pub content: String,
}

/// Ответы на подписку `Docs`: снимок, затем изменения до закрытия соединения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocsEvent {
    Snapshot(Vec<Document>),
    Updated(Document),
    Removed { name: String },
}
