//! Профиль Codex CLI.

use std::path::Path;

use super::{ConfigFile, Setup, toml_string};
use crate::session::mcp;

/// Наибольшая длина полного имени MCP-инструмента.
pub(super) const MCP_NAME_LIMIT: usize = 64;

/// Переменная каталога состояния: без неё Codex ищет его в `$HOME`.
pub(super) const HOME_ENV: &str = "CODEX_HOME";

/// Модель, с которой работает коробка.
const MODEL: &str = "gpt-5.5";

pub(super) fn files(setup: &Setup, project: &Path) -> Vec<ConfigFile> {
    vec![
        ConfigFile {
            path: setup.user.state_root.join(".codex/config.toml"),
            content: config(setup, project),
            mode: 0o600,
            owned_by_user: true,
        },
        ConfigFile {
            path: setup.etc.join("codex/requirements.toml"),
            content: requirements(setup),
            mode: 0o644,
            owned_by_user: false,
        },
    ]
}

/// `config.toml`: свой провайдер на шлюзе и всё, что убирает экраны первого запуска и вопросы.
///
/// Провайдер называется не `OpenAI`: под этим именем Codex включает удалённое сжатие, на которое шлюз не отвечает.
/// Служебные запросы подписки (аккаунт, рабочее пространство, каталог моделей) идут мимо шлюза на `chatgpt.com`:
/// TUI принимает адрес рабочего пространства только по HTTPS, а шлюз говорит HTTP. Оболочку Codex берёт из
/// /etc/passwd, а снимок оболочки (`shell_snapshot`) подменённую оболочку обошёл бы.
fn config(setup: &Setup, project: &Path) -> String {
    let mount = toml_string(&project.display().to_string());
    format!(
        r#"model = "{MODEL}"
model_provider = "iapetus"
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

[features]
shell_snapshot = false

[model_providers.iapetus]
name = "iapetus"
base_url = "http://{gateway}/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false

[projects.{mount}]
trust_level = "trusted"

[notice.model_migrations]
"gpt-5.5" = "gpt-6-sol"
{mcp}"#,
        gateway = setup.gateway,
        mcp = mcp_server(setup),
    )
}

/// Таблицы MCP-серверов в `config.toml`: проектный MCP session и, если есть, сервер Iapetus.
fn mcp_server(setup: &Setup) -> String {
    let iapetus = setup.mcp.as_ref().map(|mcp| {
        format!(
            "\n[mcp_servers.iapetus]\nurl = {}\nhttp_headers = {{ Authorization = {} }}\n",
            toml_string(&mcp.url),
            toml_string(&mcp.authorization()),
        )
    });
    format!(
        "\n[mcp_servers.{}]\nurl = {}\n{}",
        mcp::SERVER_NAME,
        toml_string(&setup.project_mcp_url()),
        iapetus.unwrap_or_default()
    )
}

/// MCP-серверы проекта в контейнере не запускаются: Codex включает только сервер из списка, у которого совпали
/// имя и адрес. В списке проектный MCP session и сервер Iapetus. Файл принадлежит root: агент не заменит адрес
/// сервера.
fn requirements(setup: &Setup) -> String {
    let iapetus = setup
        .mcp
        .as_ref()
        .map(|mcp| format!("\n[mcp_servers.iapetus]\nidentity = {{ url = {} }}\n", toml_string(&mcp.url)));
    format!(
        "[mcp_servers.{}]\nidentity = {{ url = {} }}\n{}",
        mcp::SERVER_NAME,
        toml_string(&setup.project_mcp_url()),
        iapetus.unwrap_or_default()
    )
}
