//! Профиль Claude Code.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{ConfigFile, Setup};
use crate::session::mcp;

/// Наибольшая длина полного имени MCP-инструмента.
pub(super) const MCP_NAME_LIMIT: usize = 64;

/// Переменная каталога конфигурации: без неё `.claude.json` лежит в `~` вне тома.
pub(super) const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

pub(super) fn config_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

/// Версия из вывода `claude --version`: первое слово.
pub(super) fn installed_version(output: &str) -> Option<String> {
    output.split_whitespace().next().map(str::to_owned)
}

/// Временный файл Claude Code: `<имя>.tmp.<pid>.<12 hex>`, то есть имя оканчивается на `\.tmp\.\d+\.[0-9a-f]{12}`.
/// Любое другое имя с `.tmp.` — обычный файл: принятый за временный, он не дошёл бы до runner.
pub(in crate::session) fn is_temp_name(name: &str) -> bool {
    // Справа налево: hex, pid, `tmp` и то, что перед `.tmp` (может быть пустым, но точка перед `tmp` нужна).
    let mut parts = name.rsplitn(4, '.');
    let (Some(hex), Some(pid), Some("tmp"), Some(_)) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    hex.len() == 12
        && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
}

/// `managed-mcp.json` даёт Claude Code исключительное управление MCP: загружаются только перечисленные в нём
/// серверы, `.mcp.json` проекта и `--mcp-config` не действуют: проектные серверы агент получает через
/// `iapetus-project`. Файл принадлежит root: агент его не изменит.
pub(super) fn files(setup: &Setup, project: &Path) -> Vec<ConfigFile> {
    let config_dir = config_dir(&setup.user.state_root);
    let mut servers = json!({
        mcp::SERVER_NAME: { "type": "http", "url": setup.project_mcp_url() },
    });
    if let Some(mcp) = &setup.mcp {
        servers["iapetus"] = json!({
            "type": "http",
            "url": mcp.url,
            "headers": { "Authorization": mcp.authorization() },
        });
    }
    vec![
        ConfigFile {
            path: setup.etc.join("claude-code/managed-mcp.json"),
            content: format!("{:#}\n", json!({ "mcpServers": servers })),
            mode: 0o644,
            owned_by_user: false,
        },
        ConfigFile {
            path: config_dir.join(".claude.json"),
            content: merged(&config_dir.join(".claude.json"), &first_run_state(project)),
            mode: 0o600,
            owned_by_user: true,
        },
        ConfigFile {
            path: config_dir.join("settings.json"),
            content: merged(&config_dir.join("settings.json"), &unattended_settings()),
            mode: 0o600,
            owned_by_user: true,
        },
    ]
}

/// Ключи `.claude.json`, после которых Claude Code не показывает вступление, подсказки и вопрос о доверии каталогу
/// (без них он ещё и проверяет связь с `api.anthropic.com` и выходит, если её нет). Доверие по предкам каталога
/// (`"/"`) Claude Code принимает только вне репозитория: в репозитории и в его worktree поиск останавливается на
/// корне, поэтому доверие записано и по точному пути `mount`. Эта же запись разрешает `@`-импорты `CLAUDE.md` вне
/// проекта (`../`, `~/`, абсолютные): без неё Claude Code раскрывает только импорты внутри проекта.
fn first_run_state(mount: &Path) -> Value {
    json!({
        "hasCompletedOnboarding": true,
        "hasSeenAutoDefaultNudge": true,
        "hasSeenAutoDefaultNotice": true,
        "projects": {
            "/": { "hasTrustDialogAccepted": true },
            mount.display().to_string(): {
                "hasTrustDialogAccepted": true,
                "hasClaudeMdExternalIncludesApproved": true,
            },
        },
    })
}

/// Настройки без вопросов о разрешениях: все действия агента всё равно уходят на runner.
fn unattended_settings() -> Value {
    json!({
        "permissions": { "defaultMode": "bypassPermissions" },
        "skipDangerousModePermissionPrompt": true,
    })
}

/// Содержимое файла `path` с вложенными в него ключами `keys`: чужие ключи (разговоры, токены, рабочие настройки),
/// которые Claude Code дописал в томе, остаются. Нет файла или он не разбирается как объект — только `keys`.
fn merged(path: &Path, keys: &Value) -> String {
    let existing = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()));
    format!("{:#}\n", merge(existing, keys))
}

fn merge(base: Value, keys: &Value) -> Value {
    let (Value::Object(mut base), Value::Object(keys)) = (base, keys.clone()) else {
        return keys.clone();
    };
    for (name, value) in keys {
        let current = base.remove(&name).unwrap_or(Value::Null);
        let combined = if current.is_object() && value.is_object() {
            merge(current, &value)
        } else {
            value
        };
        base.insert(name, combined);
    }
    Value::Object(base)
}

/// Имя каталога разговоров проекта в `projects/`: каждый знак пути, кроме латинской буквы и цифры, — `-` (по одному на
/// единицу UTF-16, как у Claude Code). `None` — путь длиннее предела, после которого Claude Code добавляет хеш.
fn project_slug(project: &Path) -> Option<String> {
    let slug: String = project
        .to_string_lossy()
        .encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect();
    (slug.len() <= MAX_SLUG).then_some(slug)
}

/// Наибольшая длина имени каталога проекта без хеша.
const MAX_SLUG: usize = 200;

/// Есть ли в каталоге проекта хоть один файл разговора. Путь длиннее предела (имя с хешем неизвестно) и ошибка чтения,
/// кроме отсутствия каталога, считаются наличием разговора: `--continue` сам скажет, что продолжать нечего.
pub(super) fn has_conversation(config_dir: &Path, project: &Path) -> bool {
    let Some(slug) = project_slug(project) else {
        return true;
    };
    let entries = match std::fs::read_dir(config_dir.join("projects").join(slug)) {
        Ok(entries) => entries,
        Err(error) => return error.kind() != std::io::ErrorKind::NotFound,
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|extension| extension == "jsonl"))
}

/// Разговоры лежат в `<каталог конфигурации>/projects/<имя от пути проекта>`: перенос каталога под имя нового пути
/// даёт `--continue` те же разговоры. Если каталог нового пути уже есть, файлы переезжают в него, кроме совпадающих по
/// имени.
pub(super) fn relocate_conversations(config_dir: &Path, from: &Path, to: &Path) -> std::io::Result<()> {
    let (Some(from), Some(to)) = (project_slug(from), project_slug(to)) else {
        return Ok(());
    };
    let projects = config_dir.join("projects");
    let (from, to) = (projects.join(from), projects.join(to));
    if from == to || !from.is_dir() {
        return Ok(());
    }
    if !to.exists() {
        return std::fs::rename(from, to);
    }
    for entry in std::fs::read_dir(&from)? {
        let name = entry?.file_name();
        if !to.join(&name).exists() {
            std::fs::rename(from.join(&name), to.join(&name))?;
        }
    }
    Ok(())
}
