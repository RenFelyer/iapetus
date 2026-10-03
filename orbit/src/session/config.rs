//! Настройки компонентов сессии из аргументов: профиль агента, файловая система, канал данных.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::ValueEnum;
use nerve::protocol::{self, Root};
use nerve::ssh::Trust;
use russh::keys::{Certificate, PrivateKey, PublicKey};
use tokio::sync::{mpsc, watch};

use super::agent::{Mcp, Setup, write_file};
use super::launch::AgentUser;
use super::terminal::Terminal;
use super::{Args, data, fs, ssh};

/// Сколько ждать ответа runner на один файловый запрос.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Как агент записывает файлы.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum AgentProfile {
    /// Временный файл и `rename` поверх цели
    Rename,
    /// На месте, запись уходит на runner на `flush`
    Flush,
}

impl From<AgentProfile> for fs::Profile {
    fn from(profile: AgentProfile) -> Self {
        match profile {
            AgentProfile::Rename => Self::Rename,
            AgentProfile::Flush => Self::Flush,
        }
    }
}

/// Кладёт переданные значениями секреты в файлы (`0600`, владелец — процесс session): server отдаёт их
/// окружением контейнера, а читаются они с tmpfs. Без значения файл остаётся как есть.
pub(super) fn write_secrets(files: &[(&std::path::Path, Option<&str>)]) -> anyhow::Result<()> {
    for (path, content) in files {
        let Some(content) = content else { continue };
        write_file(path, content.as_bytes(), 0o600, None).with_context(|| format!("секрет {} не записан", path.display()))?;
    }
    Ok(())
}

/// Секреты из аргументов в их файлы.
pub(super) fn install_secrets(args: &Args) -> anyhow::Result<()> {
    write_secrets(&[
        (&args.session_secret_file, args.session_secret_content.as_deref()),
        (&args.data_host_key, args.data_host_key_content.as_deref()),
        (&args.data_host_cert, args.data_host_cert_content.as_deref()),
        (&args.ca_public_key, args.ca_public_key_content.as_deref()),
    ])
}

/// Что профилю агента нужно знать о сессии.
pub(super) fn agent_setup(args: &Args, project_mcp_port: u16) -> Setup {
    Setup {
        gateway: args.gateway_listen,
        shell_socket: args.shell_socket.clone(),
        etc: PathBuf::from("/etc"),
        mcp: args
            .mcp_url
            .clone()
            .zip(args.mcp_token.clone())
            .map(|(url, token)| Mcp { url, token }),
        project_mcp_port,
        user: AgentUser {
            uid: args.shell_uid,
            gid: args.shell_gid,
            state_root: args.agent_home.clone(),
        },
    }
}

/// Файловая система проекта и окружение runner в контейнере.
pub(super) fn workspace(
    args: &Args,
    setup: &Setup,
    vfs: std::sync::Arc<fs::Vfs>,
    home_vfs: std::sync::Arc<fs::Vfs>,
) -> super::host::Workspace {
    super::host::Workspace::new(
        vfs,
        home_vfs,
        args.agent_home.clone(),
        setup.etc.clone(),
        args.shell_uid,
        args.agent.isolation().home_links,
    )
}

/// Записи `$HOME`, которые ФС `$HOME` отдаёт сама: имена из перечня изоляции профиля — ссылки в каталог состояния.
pub(super) fn home_links(names: &[String], state_root: &std::path::Path) -> Vec<fs::Symlink> {
    names
        .iter()
        .map(|name| fs::Symlink {
            name: name.clone(),
            target: state_root.join(name).display().to_string(),
        })
        .collect()
}

/// Настройки ФС сессии для каталога runner `root_kind`: профиль записи агента, владелец файлов, предел файла,
/// скрытые пути проекта и записи изоляции в `$HOME`.
pub(super) fn fuse_config(args: &Args, root_kind: Root) -> anyhow::Result<fs::Config> {
    let isolation = args.agent.isolation();
    let (hidden, links) = match root_kind {
        Root::Project => (isolation.hidden_paths, Vec::new()),
        Root::Home => (Vec::new(), home_links(&isolation.home_links, &args.agent_home)),
    };
    Ok(fs::Config {
        profile: args.agent_profile.map_or_else(|| args.agent.fuse_profile(), Into::into),
        owner: fs::Owner {
            uid: args.shell_uid,
            gid: args.shell_gid,
        },
        fingerprint: args.runner.clone(),
        root_kind,
        links,
        root: PathBuf::from("/"),
        cache_dir: args.cache_dir.clone(),
        max_file: max_file(args)?,
        request_timeout: REQUEST_TIMEOUT,
        hidden,
        temp_name: isolation.temp_name,
    })
}

/// Сервер канала данных: ключ и хост-сертификат сессии, CA, по которому проверяются сертификаты runner, и
/// назначенный runner, которого одного пускают.
pub(super) fn data_server(args: &Args) -> anyhow::Result<DataServer> {
    let ca = PublicKey::read_openssh_file(&args.ca_public_key).with_context(|| format!("ключ CA {}", args.ca_public_key.display()))?;
    let host_key =
        PrivateKey::read_openssh_file(&args.data_host_key).with_context(|| format!("ключ хоста {}", args.data_host_key.display()))?;
    let host_cert = Certificate::from_openssh(
        &std::fs::read_to_string(&args.data_host_cert).with_context(|| format!("хост-сертификат {}", args.data_host_cert.display()))?,
    )
    .with_context(|| format!("хост-сертификат {} не разбирается", args.data_host_cert.display()))?;
    let (data, link, events) = data::Data::new();
    data.assign(Some(args.runner.clone()));
    let (terminal, terminal_rx) = watch::channel(None);
    let server = ssh::Listener::new(data.clone(), Trust::new(&[ca]), ssh::config(host_key, host_cert), terminal_rx);
    Ok(DataServer {
        server,
        terminal,
        data,
        link,
        events,
    })
}

/// Канал данных: сервер, его состояние (им указывают назначение), текущее соединение и события.
pub(super) struct DataServer {
    pub(super) server: ssh::Listener,
    /// Слот терминала агента: сессия кладёт сюда терминал после запуска агента.
    pub(super) terminal: watch::Sender<Option<Terminal>>,
    pub(super) data: data::Data,
    pub(super) link: data::Link,
    pub(super) events: mpsc::UnboundedReceiver<data::Event>,
}

/// Наибольший файл сессии: из параметра или размер `/work`, но не больше предела сообщения канала данных — файл
/// идёт одним сообщением.
fn max_file(args: &Args) -> anyhow::Result<usize> {
    let max_file = match args.max_file {
        Some(max_file) => max_file,
        None => work_size(&args.cache_dir).with_context(|| format!("размер {} не определён", args.cache_dir.display()))?,
    };
    Ok(usize::try_from(max_file).unwrap_or(usize::MAX).min(protocol::MAX_FILE))
}

/// Размер файловой системы каталога кэша: больший файл в сессии не поместится.
fn work_size(dir: &std::path::Path) -> anyhow::Result<u64> {
    // Права каталога выставит ядро ФС; здесь он нужен, чтобы спросить его файловую систему.
    std::fs::create_dir_all(dir)?;
    let stat = rustix::fs::statvfs(dir)?;
    Ok(stat.f_blocks.saturating_mul(stat.f_frsize))
}

#[cfg(test)]
#[path = "../../tests/session/config.rs"]
mod tests;
