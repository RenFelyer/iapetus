//! Профиль агента: всё, чем Claude Code, Codex CLI и Antigravity CLI отличаются внутри одной коробки.
//!
//! Три слоя перехвата одни для всех. Профиль задаёт только данные: какое API говорит агент и куда его слать,
//! как агент пишет файлы, какие файлы первого запуска и учётные данные ему нужны, какие переменные окружения
//! направляют его на шлюз и подменённую оболочку, каких файлов проекта агент не видит.

mod antigravity;
mod claude;
mod codex;

use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::Context;
use nerve::agent::{AGENT_ROOT, Agent};
use pty_process::Pty;

use super::fs;
use super::gateway::Api;
use super::host::Environment;
use super::launch::{self, AgentUser, AsOwner};
use super::update::{Install, UpdateProfile};
use crate::session::shell::{self, Calls};

/// Переменные каталогов XDG и подкаталог состояния агента для каждой.
const XDG_DIRS: [(&str, &str); 4] = [
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
];

/// Подменённая оболочка: ссылка на `orbit`, `argv[0]` которой оканчивается на `bash`.
const SHELL: &str = "/opt/iapetus/bash";

/// Профиль агента, которого запускает коробка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Profile(pub(super) Agent);

impl std::str::FromStr for Profile {
    type Err = nerve::agent::UnknownAgent;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

/// Что профиль объявляет об изоляции агента; применяют шаблон контейнера, FUSE и запуск.
#[derive(Debug, Clone)]
pub(super) struct Isolation {
    /// Переменные, которые направляют состояние агента (свои и каталоги XDG) в его каталог тома.
    pub state_env: Vec<(String, String)>,
    /// Имена в `$HOME` (относительно корня состояния), которые ведут в каталог состояния: агент без переменной
    /// состояния ищет его в `$HOME`.
    pub home_links: Vec<String>,
    /// Пути проекта, скрытые в FUSE.
    pub hidden_paths: Vec<String>,
    /// Временные файлы агента до `rename`: пишутся локально, на runner не идут.
    pub temp_name: Option<fn(&str) -> bool>,
    /// API подписки, куда шлюз пересылает запросы агента.
    pub upstream: &'static str,
    /// Программа агента в `PATH` образа session.
    pub program: &'static str,
}

/// Что профилю нужно знать о сессии.
#[derive(Debug, Clone)]
pub(super) struct Setup {
    /// Адрес шлюза: агент получает его как базовый адрес своего API.
    pub gateway: SocketAddr,
    /// Сокет подменённой оболочки.
    pub shell_socket: PathBuf,
    /// Каталог системных настроек (`/etc`): сюда кладутся ограничения, которые агент не изменит.
    pub etc: PathBuf,
    /// MCP-сервер Iapetus для агента; без него агент видит только проектный MCP session.
    pub mcp: Option<Mcp>,
    /// Порт проектного MCP session на `127.0.0.1`: агент получает его всегда.
    pub project_mcp_port: u16,
    pub user: AgentUser,
}

/// MCP-сервер orbit server.
#[derive(Debug, Clone)]
pub(super) struct Mcp {
    pub url: String,
    pub token: McpToken,
}

impl Setup {
    /// Адрес проектного MCP session.
    fn project_mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.project_mcp_port, super::mcp::PATH)
    }
}

impl Mcp {
    /// Значение заголовка `Authorization`, которым агент входит на сервер.
    fn authorization(&self) -> String {
        format!("Bearer {}", self.token.0)
    }
}

/// Токен MCP сессии. В журнал и `Debug` не попадает.
#[derive(Clone)]
pub(crate) struct McpToken(String);

impl std::str::FromStr for McpToken {
    type Err = std::convert::Infallible;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Ok(Self(token.to_owned()))
    }
}

impl std::fmt::Debug for McpToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpToken(***)")
    }
}

/// Файл, который сессия кладёт до запуска агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConfigFile {
    pub path: PathBuf,
    pub content: String,
    pub mode: u32,
    /// Владелец — пользователь агента; иначе root, и агент файл не изменит.
    pub owned_by_user: bool,
}

impl Profile {
    /// API, на котором говорит агент: от него зависит разбор ответов шлюзом.
    pub(super) fn gateway_api(self) -> Api {
        match self.0 {
            Agent::ClaudeCode => Api::Messages,
            Agent::Codex => Api::Responses,
            Agent::Antigravity => Api::Gemini,
        }
    }

    /// Как агент пишет файлы.
    pub(super) fn fuse_profile(self) -> fs::Profile {
        match self.0 {
            Agent::ClaudeCode => fs::Profile::Rename,
            Agent::Codex | Agent::Antigravity => fs::Profile::Flush,
        }
    }

    /// Программа агента в `PATH` образа session.
    fn program(self) -> &'static str {
        match self.0 {
            Agent::ClaudeCode => "claude",
            Agent::Codex => "codex",
            Agent::Antigravity => "agy",
        }
    }

    /// Наибольшая длина полного имени MCP-инструмента, которую принимает агент.
    pub(super) fn mcp_name_limit(self) -> usize {
        match self.0 {
            Agent::ClaudeCode => claude::MCP_NAME_LIMIT,
            Agent::Codex => codex::MCP_NAME_LIMIT,
            Agent::Antigravity => antigravity::MCP_NAME_LIMIT,
        }
    }

    /// Изоляция агента: данные, которые применяют FUSE, запуск и шаблон контейнера.
    pub(super) fn isolation(self) -> Isolation {
        let state_dir = format!("{AGENT_ROOT}/{}", self.0.state_dir());
        let mut state_env = match self.0 {
            Agent::ClaudeCode => vec![(claude::CONFIG_DIR_ENV.to_owned(), state_dir.clone())],
            Agent::Codex => vec![(codex::HOME_ENV.to_owned(), state_dir.clone())],
            Agent::Antigravity => Vec::new(),
        };
        // Каталоги XDG агента — в его состоянии: иначе он пишет в `$HOME` runner (файл ярлыка Claude Code в
        // `~/.local/share`, распакованный `rg` Antigravity в `~/.cache`).
        state_env.extend(
            XDG_DIRS
                .iter()
                .map(|(variable, name)| ((*variable).to_owned(), format!("{state_dir}/xdg/{name}"))),
        );
        let home_links = match self.0 {
            Agent::ClaudeCode | Agent::Codex => Vec::new(),
            Agent::Antigravity => vec![self.0.state_dir().to_owned()],
        };
        let temp_name: Option<fn(&str) -> bool> = match self.0 {
            Agent::ClaudeCode => Some(claude::is_temp_name),
            Agent::Codex | Agent::Antigravity => None,
        };
        Isolation {
            state_env,
            home_links,
            hidden_paths: self.hidden_paths(),
            temp_name,
            upstream: self.upstream(),
            program: self.program(),
        }
    }

    /// Как обновлять агента; `None` — session его не обновляет.
    pub(super) fn updates(self) -> Option<UpdateProfile> {
        match self.0 {
            Agent::ClaudeCode => Some(UpdateProfile {
                installed_version: claude::installed_version,
                install: Install::ReplaceFile,
            }),
            Agent::Codex | Agent::Antigravity => None,
        }
    }

    /// Адрес API подписки, куда шлюз пересылает запросы агента.
    fn upstream(self) -> &'static str {
        match self.0 {
            Agent::ClaudeCode => "https://api.anthropic.com",
            Agent::Codex => "https://chatgpt.com",
            // Code Assist — API входа аккаунтом Google.
            Agent::Antigravity => "https://daily-cloudcode-pa.googleapis.com",
        }
    }

    /// Переменные окружения агента.
    pub(super) fn env(self, setup: &Setup) -> Vec<(String, String)> {
        let gateway = format!("http://{}", setup.gateway);
        let shell_socket = (shell::SOCKET_ENV.to_owned(), setup.shell_socket.display().to_string());
        match self.0 {
            Agent::ClaudeCode => vec![
                ("ANTHROPIC_BASE_URL".to_owned(), gateway),
                ("CLAUDE_CODE_SHELL".to_owned(), SHELL.to_owned()),
                calls(Calls::Claude),
                // Остаются только запросы к API через шлюз и WebFetch: без bootstrap, телеметрии и проверок флагов,
                // которые иначе шли бы с контейнера прямо на `api.anthropic.com`.
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(), "1".to_owned()),
                shell_socket,
            ],
            // Оболочку Codex берёт из /etc/passwd: пользователь агента создан с оболочкой `SHELL`.
            Agent::Codex => vec![shell_socket, calls(Calls::Plain)],
            Agent::Antigravity => vec![
                ("CLOUD_CODE_URL".to_owned(), gateway),
                ("PATH".to_owned(), antigravity::PATH.to_owned()),
                shell_socket,
                calls(Calls::Plain),
            ],
        }
    }

    /// Файлы первого запуска и ограничений для проекта по пути `project`.
    pub(super) fn files(self, setup: &Setup, project: &Path) -> Vec<ConfigFile> {
        match self.0 {
            Agent::ClaudeCode => claude::files(setup, project),
            Agent::Codex => codex::files(setup, project),
            Agent::Antigravity => antigravity::files(setup, project),
        }
    }

    /// Каталог состояния агента.
    fn state_dir(self, setup: &Setup) -> PathBuf {
        setup.user.state_root.join(self.0.state_dir())
    }

    /// Переносит разговоры проекта `from` под путь `to`, чтобы продолжение разговора нашло их. Агент, который
    /// привязывает разговоры не к пути проекта, ничего не переносит. Вызывается от имени пользователя агента.
    fn relocate_conversations(self, state_dir: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
        match self.0 {
            Agent::ClaudeCode => claude::relocate_conversations(state_dir, from, to),
            Agent::Codex | Agent::Antigravity => Ok(()),
        }
    }

    /// Глобальный файл правил Iapetus вне проекта и его начало: место, которое агент перечитывает и которое не
    /// попадает в рабочий каталог.
    pub(super) fn rules_file(self, setup: &Setup) -> (PathBuf, &'static str) {
        match self.0 {
            Agent::ClaudeCode => (setup.etc.join("claude-code/.claude/rules/iapetus.md"), ""),
            Agent::Codex => (setup.user.state_root.join(".codex/AGENTS.md"), ""),
            Agent::Antigravity => (
                setup.user.state_root.join(".gemini/config/rules/iapetus.md"),
                "---\ntrigger: always_on\n---\n\n",
            ),
        }
    }

    /// Команда запуска, которая продолжает последний разговор тома: флаг или подкоманда сразу после программы.
    pub(super) fn resume_command(self, command: &[String]) -> Vec<String> {
        let Some((program, rest)) = command.split_first() else {
            return Vec::new();
        };
        let resume: &[&str] = match self.0 {
            Agent::ClaudeCode => &["--continue"],
            Agent::Codex => &["resume", "--last"],
            Agent::Antigravity => &["-c"],
        };
        let mut resumed = vec![program.clone()];
        resumed.extend(resume.iter().map(|word| (*word).to_owned()));
        resumed.extend(rest.iter().cloned());
        resumed
    }

    /// Есть ли сохранённый разговор проекта `project`, который продолжение может открыть. Агент, у которого наличие
    /// разговора проверить нечем, считается имеющим его: запуск остаётся с продолжением. Вызывается от имени
    /// пользователя агента.
    fn has_conversation(self, state_dir: &Path, project: &Path) -> bool {
        match self.0 {
            Agent::ClaudeCode => claude::has_conversation(state_dir, project),
            Agent::Codex | Agent::Antigravity => true,
        }
    }

    /// Пути относительно рабочего каталога, которых агент не видит в FUSE: MCP-конфиг проекта у агента без
    /// системного запрета на MCP-серверы проекта.
    fn hidden_paths(self) -> Vec<String> {
        match self.0 {
            Agent::ClaudeCode | Agent::Codex => Vec::new(),
            Agent::Antigravity => vec![antigravity::PROJECT_MCP_CONFIG.to_owned()],
        }
    }
}

/// Файл в каталоге состояния агента с путём проекта, под которым агент работал в последний раз.
const LAST_PROJECT: &str = "iapetus-project";

/// Кладёт файлы профиля, переносит разговоры под путь проекта и запускает `command` от пользователя агента
/// (`orbit drop-exec`) в pty: рабочий каталог — проект, `HOME` — как у runner. С `resume` команда продолжает разговор,
/// если он сохранён для проекта; без разговора агент запускается без продолжения.
pub(super) fn start(
    agent: Profile,
    setup: &Setup,
    environment: &Environment,
    command: &[String],
    resume: bool,
) -> anyhow::Result<(tokio::process::Child, Pty)> {
    prepare(agent, setup, &environment.project).context("профиль агента не подготовлен")?;
    if let Err(error) = follow_project(agent, setup, &environment.project) {
        tracing::warn!(error = %format!("{error:#}"), "разговоры агента не перенесены под путь проекта");
    }
    let command = if resume && has_conversation(agent, setup, &environment.project) {
        agent.resume_command(command)
    } else {
        command.to_vec()
    };
    let orbit = std::env::current_exe().context("путь к исполняемому файлу orbit не определён")?;
    let env = launch_environment(agent, setup, std::env::vars());
    launch::spawn(&orbit, &setup.user, &environment.home, &environment.project, &env, &command).context("агент не запущен")
}

/// Есть ли у агента разговор для `project`. Каталог состояния читается от имени пользователя агента; ошибка входа
/// в его права считается наличием разговора, как и неизвестный ответ профиля.
fn has_conversation(agent: Profile, setup: &Setup, project: &Path) -> bool {
    let Ok(_as_owner) = AsOwner::enter(setup.user.uid, setup.user.gid) else {
        return true;
    };
    agent.has_conversation(&agent.state_dir(setup), project)
}

/// Агент остановлен, а проект теперь по другому пути: разговоры, которые агент хранит под путём проекта, переезжают
/// под новый, и `resume` их находит. Последний путь запоминается в каталоге состояния.
fn follow_project(agent: Profile, setup: &Setup, project: &Path) -> anyhow::Result<()> {
    let state_dir = agent.state_dir(setup);
    let marker = state_dir.join(LAST_PROJECT);
    // Каталог состояния и файлы в нём принадлежат агенту: у session нет `CAP_DAC_OVERRIDE`, и закрытый файл она не
    // прочитала бы.
    let previous = {
        let _as_owner = AsOwner::enter(setup.user.uid, setup.user.gid)?;
        std::fs::read_to_string(&marker).ok().map(PathBuf::from)
    };
    if previous.as_deref() == Some(project) {
        return Ok(());
    }
    if let Some(previous) = previous {
        let _as_owner = AsOwner::enter(setup.user.uid, setup.user.gid)?;
        agent
            .relocate_conversations(&state_dir, &previous, project)
            .with_context(|| format!("разговоры {} не перенесены в {}", previous.display(), project.display()))?;
    }
    write_file(&marker, project.to_string_lossy().as_bytes(), 0o600, Some(&setup.user))
}

/// Окружение процесса агента: наследуемое из `inherited`, переменные состояния и окружение профиля. Учётных данных
/// агента здесь нет: он входит сам, они лежат в томе состояния.
pub(super) fn launch_environment(
    agent: Profile,
    setup: &Setup,
    inherited: impl Iterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut profile = agent.isolation().state_env;
    profile.extend(agent.env(setup));
    launch::agent_environment(inherited, &profile)
}

/// Вид вызовов оболочки, который разбирает подменённая оболочка агента.
fn calls(kind: Calls) -> (String, String) {
    (shell::CALLS_ENV.to_owned(), kind.value().to_owned())
}

/// Кладёт файлы профиля.
pub(super) fn prepare(agent: Profile, setup: &Setup, project: &Path) -> anyhow::Result<()> {
    for file in agent.files(setup, project) {
        let owner = file.owned_by_user.then_some(&setup.user);
        write_file(&file.path, file.content.as_bytes(), file.mode, owner)
            .with_context(|| format!("файл профиля {} не записан", file.path.display()))?;
    }
    Ok(())
}

/// Записывает файл заново с режимом `mode`. Файл пользователя агента создаётся от его имени (`AsOwner`: у session нет
/// `CAP_CHOWN` и `CAP_DAC_OVERRIDE`, каталог пользователя ей закрыт), недостающие каталоги — тоже его. Файл
/// создаётся закрытым (0600) и получает `mode` после записи, чтобы учётные данные не лежали с широкими правами.
pub(super) fn write_file(path: &Path, content: &[u8], mode: u32, owner: Option<&AgentUser>) -> anyhow::Result<()> {
    let parent = path.parent().context("у файла нет каталога")?;
    let _as_owner = owner.map(|user| AsOwner::enter(user.uid, user.gid)).transpose()?;
    // Каталоги пользователя закрыты для остальных; каталоги root (`/etc/claude-code`) агент читает.
    let dir_mode = if owner.is_some() { 0o700 } else { 0o755 };
    std::fs::DirBuilder::new().recursive(true).mode(dir_mode).create(parent)?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(content)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Строка TOML в двойных кавычках.
fn toml_string(text: &str) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
#[path = "../../tests/session/agent.rs"]
mod tests;
