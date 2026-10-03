//! Профиль Antigravity CLI (`agy`).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{ConfigFile, Setup};
use crate::session::mcp;

/// Наибольшая длина полного имени MCP-инструмента.
pub(super) const MCP_NAME_LIMIT: usize = 64;

/// `PATH` агента. agy ищет `bash` по `PATH`, а `$SHELL` не смотрит, поэтому первым стоит `/opt/iapetus/path` с
/// единственной ссылкой `bash` на подменённую оболочку — раньше `/usr/bin`, где лежит настоящий bash.
pub(super) const PATH: &str = "/opt/iapetus/path:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// MCP-конфиг проекта относительно рабочего каталога. Системного запрета у agy нет, поэтому файл прячет FUSE.
pub(super) const PROJECT_MCP_CONFIG: &str = ".agents/mcp_config.json";

/// Состояние и настройки agy.
fn state_dir(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli")
}

pub(super) fn files(setup: &Setup, project: &Path) -> Vec<ConfigFile> {
    let home = &setup.user.state_root;
    vec![
        user_json(state_dir(home).join("cache/onboarding.json"), &onboarding()),
        user_json(state_dir(home).join("settings.json"), &settings(project)),
        user_json(home.join(".gemini/config/mcp_config.json"), &global_mcp(setup)),
    ]
}

fn user_json(path: PathBuf, value: &serde_json::Value) -> ConfigFile {
    ConfigFile {
        path,
        content: format!("{value:#}\n"),
        mode: 0o600,
        owned_by_user: true,
    }
}

/// Три флага, без которых agy показывает экраны первого запуска: цветовую схему и условия использования.
fn onboarding() -> serde_json::Value {
    json!({
        "consumerOnboardingComplete": true,
        "enterpriseOnboardingComplete": true,
        "onboardingComplete": true,
    })
}

/// Доверие рабочему каталогу (без него agy спрашивает о доверии) и работа без подтверждений: команды и правки агент
/// делает сам, а `always-proceed` убирает вопрос перед каждой из них и перед просмотром артефактов.
fn settings(project: &Path) -> serde_json::Value {
    json!({
        "trustedWorkspaces": [project.display().to_string()],
        "colorScheme": "dark",
        "showTips": false,
        "showFeedbackSurvey": false,
        "toolPermission": "always-proceed",
        "artifactReviewPolicy": "always-proceed",
    })
}

/// Глобальный MCP-конфиг пишется заново при каждом старте: в нём проектный MCP session и, если есть, сервер
/// Iapetus. Адрес сервера agy читает из `serverUrl`.
fn global_mcp(setup: &Setup) -> serde_json::Value {
    let mut servers = json!({
        mcp::SERVER_NAME: { "serverUrl": setup.project_mcp_url() },
    });
    if let Some(mcp) = &setup.mcp {
        servers["iapetus"] = json!({
            "serverUrl": mcp.url,
            "headers": { "Authorization": mcp.authorization() },
        });
    }
    json!({ "mcpServers": servers })
}
