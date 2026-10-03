//! Правила Iapetus для агента: сведения о runner, смысл `ECANCELED` и документы server.
//!
//! Правила лежат одним файлом в глобальном месте инструкций агента, вне рабочего каталога: файл в проекте модель
//! приняла бы за код. Файл в `/etc` принадлежит root, файл в домашнем каталоге — пользователю агента; он заменяется
//! целиком при смене документов или runner.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use nerve::fingerprint::Fingerprint;
use nerve::protocol::{AssignedRunner, DocsEvent, McpCatalog};

use super::agent::{self, Profile, Setup};
use super::host;
use super::launch::AsOwner;

const ECANCELED: &str = "Запись файла может закончиться ошибкой `ECANCELED`: файл на runner изменился после того, \
как ты его прочитал. Перечитай файл и повтори правку.";

const SEARCH: &str = "Команды выполняются на runner, и `rg` (ripgrep) там может не быть: для поиска используй `grep` и `find`.";

const OUTSIDE_PROJECT: &str = "Встроенные инструменты чтения и поиска видят файловую систему контейнера, а не runner: вне проекта \
они покажут не те файлы. Файлы вне проекта читай через команды оболочки (`cat`, `grep`).";

/// То же, когда `$HOME` runner отдан файловой системой: `{home}` — его путь.
fn outside_project_with_home(home: &str) -> String {
    format!(
        "Файлы в `$HOME` runner (`{home}`) встроенные инструменты чтения и поиска видят так же, как на runner. Вне `$HOME` \
         и проекта они видят файловую систему контейнера, а не runner: такие файлы читай через команды оболочки (`cat`, \
         `grep`). Каталог `.claude` в `$HOME` runner недоступен."
    )
}

/// Файл правил агента сессии. Копии разделяют состояние: канал управления меняет его, а запись идёт из любой.
#[derive(Clone)]
pub(super) struct Rules {
    agent: Profile,
    setup: Setup,
    state: Arc<Mutex<State>>,
}

struct State {
    runner: Option<Fingerprint>,
    /// Сведения о runner от server; до первого указания `AssignRunner` их нет.
    details: Option<Details>,
    docs: BTreeMap<String, String>,
    /// Инструкции проектных MCP-серверов: имя сервера и текст, в порядке сводки.
    mcp: Vec<(String, String)>,
    /// Что лежит в файле: неизменившийся текст не пишется заново.
    written: Option<String>,
}

struct Details {
    name: String,
    os: String,
    /// Путь проекта: он же рабочий каталог агента.
    project: String,
    /// `$HOME` runner, если агент видит его файловой системой.
    home: Option<String>,
    env_names: Vec<String>,
}

impl Rules {
    pub(super) fn new(agent: Profile, setup: Setup, runner: Option<Fingerprint>) -> Self {
        let state = State {
            runner,
            details: None,
            docs: BTreeMap::new(),
            mcp: Vec::new(),
            written: None,
        };
        Self {
            agent,
            setup,
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Записывает файл правил, если его текст изменился.
    pub(super) fn write(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (path, header) = self.agent.rules_file(&self.setup);
        let content = format!("{header}{}", render(&state));
        if state.written.as_ref() == Some(&content) {
            return Ok(());
        }
        // Файл в домашнем каталоге пользователя агента пишется от его имени: каталоги правил в `~`
        // создаются сразу его и закрытыми, а root'у (без `CAP_DAC_OVERRIDE`) они не открылись бы.
        let owner = path.starts_with(&self.setup.user.state_root).then_some(&self.setup.user);
        let _as_owner = owner.map(|user| AsOwner::enter(user.uid, user.gid)).transpose()?;
        let name = path.file_name().context("у файла правил нет имени")?;
        let mut temporary = path.clone();
        temporary.set_file_name(format!(".{}.tmp", name.to_string_lossy()));
        agent::write_file(&temporary, content.as_bytes(), 0o644, owner)?;
        std::fs::rename(&temporary, &path).with_context(|| format!("файл правил {} не заменён", path.display()))?;
        state.written = Some(content);
        Ok(())
    }

    /// Применяет событие подписки `Docs` и обновляет файл.
    pub(super) fn apply(&self, event: DocsEvent) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match event {
                DocsEvent::Snapshot(documents) => {
                    state.docs = documents.into_iter().map(|document| (document.name, document.content)).collect();
                }
                DocsEvent::Updated(document) => {
                    state.docs.insert(document.name, document.content);
                }
                DocsEvent::Removed { name } => {
                    state.docs.remove(&name);
                }
            }
        }
        self.write()
    }

    /// Заменяет инструкции проектных MCP-серверов теми, что в сводке, и обновляет файл; пустая сводка убирает их.
    pub(super) fn set_mcp(&self, catalog: &McpCatalog) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.mcp = catalog
                .servers
                .iter()
                .filter_map(|server| {
                    let text = server.instructions.as_deref()?.trim();
                    (!text.is_empty()).then(|| (server.name.clone(), text.to_owned()))
                })
                .collect();
        }
        self.write()
    }

    /// Запоминает назначенный runner и обновляет файл.
    pub(super) fn assign(&self, assigned: Option<&AssignedRunner>) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.runner = assigned.map(|assigned| assigned.runner.clone());
            state.details = assigned.map(|assigned| {
                let user = &self.setup.user;
                let home = host::resolve(&user.state_root, user.uid, &assigned.host)
                    .ok()
                    .filter(|environment| environment.home_fs)
                    .map(|environment| environment.home.display().to_string());
                Details {
                    name: assigned.name.clone(),
                    os: assigned.os.clone(),
                    project: assigned.host.project.clone(),
                    home,
                    env_names: assigned.env_names.clone(),
                }
            });
        }
        self.write()
    }
}

fn render(state: &State) -> String {
    let runner = match (&state.runner, &state.details) {
        (Some(runner), Some(details)) => {
            let variables = if details.env_names.is_empty() {
                "Переменных окружения runner нет.".to_owned()
            } else {
                format!("Переменные окружения runner: {}.", details.env_names.join(", "))
            };
            format!(
                "Файлы и команды выполняются на runner `{}` (`{runner}`), ОС: {}. Проект лежит в `{}`: это рабочий \
                 каталог и путь проекта на runner. {variables}",
                details.name, details.os, details.project
            )
        }
        (Some(runner), None) => format!("Файлы и команды выполняются на runner `{runner}`."),
        (None, _) => "Runner не назначен: файлы и команды недоступны, пока server его не назначит.".to_owned(),
    };
    let outside = match state.details.as_ref().and_then(|details| details.home.as_deref()) {
        Some(home) if state.runner.is_some() => outside_project_with_home(home),
        _ => OUTSIDE_PROJECT.to_owned(),
    };
    let mut sections = vec![
        "# Правила Iapetus".to_owned(),
        format!("## Runner\n\n{runner}"),
        format!("## ECANCELED\n\n{ECANCELED}"),
        format!("## Поиск\n\n{SEARCH}"),
        format!("## Файлы вне проекта\n\n{outside}"),
    ];
    for (name, content) in &state.docs {
        sections.push(format!("## Документ server: {name}\n\n{}", content.trim_end()));
    }
    for (name, instructions) in &state.mcp {
        sections.push(format!("## MCP-сервер `{name}`\n\n{instructions}"));
    }
    let mut text = sections.join("\n\n");
    text.push('\n');
    text
}

#[cfg(test)]
#[path = "../../tests/session/rules.rs"]
mod tests;
