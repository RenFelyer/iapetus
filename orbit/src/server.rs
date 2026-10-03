//! orbit server — запускается в Docker и ждёт внешних подключений.
//!
//! Хранит токены, реестр и закрытый ключ центра сертификации SSH. Принимает узлы по SSH: канал управления
//! (`control`) и jump (`ssh`) на внешнем слушателе, session — на внутреннем.

mod control;
mod docker;
mod enroll;
mod lifecycle;
mod mcp;
mod registry;
mod ssh;
#[cfg(test)]
#[path = "../tests/server/testing.rs"]
mod testing;
mod update;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use nerve::process::shutdown_signal;
use nerve::protocol::Author;
use nerve::ssh::Trust;
use russh::keys::{Algorithm, PrivateKey};
use tokio::net::TcpListener;

use self::control::Control;
use self::ssh::ca::{CertificateAuthority, Validity};
use self::ssh::routes::registry_routes;
use self::ssh::{Gate, Jump, Side};

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Каталог состояния: база SQLite, ключ шифрования токенов, закрытый и открытый (`ssh-ca.pub`) ключи центра сертификации SSH, файл
    /// первого `orbit_key`, документы в `docs/`
    #[arg(long, env = "IAPETUS_DATA_DIR")]
    data_dir: PathBuf,

    /// Внешний слушатель SSH для runner и worker: адрес сети Tailscale или `ZeroTier`, не `0.0.0.0`: на всех
    /// интерфейсах порт достался бы и контейнерам session через адрес моста Docker
    #[arg(long, env = "IAPETUS_LISTEN")]
    listen: SocketAddr,

    /// Внутренний слушатель SSH для session: адрес сети Docker, наружу порт не публикуется
    #[arg(long, env = "IAPETUS_INTERNAL_LISTEN")]
    internal_listen: SocketAddr,

    /// Внутренний слушатель MCP для агентов session (HTTP): адрес сети Docker, наружу порт не публикуется
    #[arg(long, env = "IAPETUS_MCP_LISTEN")]
    mcp_listen: SocketAddr,

    /// Имена, под которыми к server подключаются: принципалы его хост-сертификата, клиент сверяет с ними хост из
    /// `orbit_url`
    #[arg(long = "host", env = "IAPETUS_HOST", value_delimiter = ',', default_value = "orbit-server")]
    hosts: Vec<String>,

    /// Автор коммитов на время сессии, пока worker не назвал другого
    #[arg(long, env = "IAPETUS_AUTHOR_NAME", default_value = "Iapetus")]
    author_name: String,

    #[arg(long, env = "IAPETUS_AUTHOR_EMAIL", default_value = "iapetus@localhost")]
    author_email: String,

    /// Не следить за выходом новых версий агентов: server не читает серверы выпусков и session не получают предложений
    #[arg(long, env = "IAPETUS_NO_AGENT_UPDATE_CHECK", value_parser = clap::builder::BoolishValueParser::new())]
    no_agent_update_check: bool,

    /// Сколько секунд версия агента должна пробыть в канале выпусков, прежде чем server предложит её session (по
    /// умолчанию 3 суток): плохой выпуск поставщик обычно снимает за несколько дней; 0 — предлагать сразу
    #[arg(long, env = "IAPETUS_UPDATE_DELAY", default_value_t = 3 * 24 * 3600)]
    update_delay: u64,

    #[command(flatten)]
    lifecycle: lifecycle::Args,
}

/// Файл первого `orbit_key` в каталоге состояния, рядом с ключами server.
const FIRST_ORBIT_KEY_FILE: &str = "orbit_key";

/// Открытый ключ CA в каталоге состояния: его кладут узлам как `orbit_ca`.
const CA_PUBLIC_KEY_FILE: &str = "ssh-ca.pub";

/// Хост-сертификат server действует год от запуска: ключ хоста создаётся при каждом запуске, а клиенты верят
/// подписи центра сертификации, а не самому ключу.
const HOST_CERTIFICATE_LIFETIME: Duration = Duration::from_hours(24 * 365);

pub(crate) async fn run(args: Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        !args.listen.ip().is_unspecified(),
        "внешний слушатель {} занял бы все интерфейсы, и порт достался бы контейнерам session: задайте адрес сети узлов",
        args.listen
    );
    tracing::info!(listen = %args.listen, internal_listen = %args.internal_listen, data_dir = %args.data_dir.display(), "orbit server запущен");
    let data = &args.data_dir;
    std::fs::create_dir_all(data).with_context(|| format!("каталог состояния {} не создан", data.display()))?;
    let cipher = registry::TokenCipher::load_or_create(&data.join("registry.key"))?;
    let mut registry = registry::Registry::open(&data.join("registry.db"), cipher)?;
    let ca = Arc::new(CertificateAuthority::load_or_create(&data.join("ssh-ca"))?);
    // Открытый ключ CA нужен каждому узлу как `orbit_ca`: без файла его неоткуда взять, а закрытый ключ отдавать нельзя.
    let ca_public = data.join(CA_PUBLIC_KEY_FILE);
    std::fs::write(&ca_public, format!("{}\n", ca.public_key().to_openssh()?))
        .with_context(|| format!("открытый ключ CA {} не записан", ca_public.display()))?;

    let first_key = data.join(FIRST_ORBIT_KEY_FILE);
    if enroll::write_first_orbit_key(&mut registry, &first_key, SystemTime::now())? {
        tracing::info!(path = %first_key.display(), "реестр пуст: первый orbit_key записан в файл");
    }
    let registry = Arc::new(Mutex::new(registry));

    let author = Author {
        name: args.author_name,
        email: args.author_email,
    };
    let docs_dir = data.join("docs");
    std::fs::create_dir_all(&docs_dir).with_context(|| format!("каталог документов {} не создан", docs_dir.display()))?;
    let mcp_tokens = Arc::new(mcp::Tokens::load_or_create(&data.join("mcp.key"))?);
    let lifecycle = lifecycle::start(
        args.lifecycle,
        data,
        Arc::clone(&registry),
        Arc::clone(&ca),
        Arc::clone(&mcp_tokens),
    )?;
    let control =
        Arc::new(Control::new(Arc::clone(&registry), Arc::clone(&ca), Some(first_key), author, docs_dir).with_lifecycle(lifecycle));
    update::start_all(
        args.no_agent_update_check,
        Duration::from_secs(args.update_delay),
        &control,
        &registry,
    )?;
    let routes = {
        let (for_addr, for_agent) = (Arc::clone(&control), Arc::clone(&control));
        registry_routes(
            registry,
            move |session| for_addr.session_addr(session),
            move |session| for_agent.agent_running(session),
        )
    };
    let jump = Arc::new(Jump::new(Arc::clone(&control), routes));
    let trust = Arc::new(Trust::new(&[ca.public_key()]));

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let hosts: Vec<&str> = args.hosts.iter().map(String::as_str).collect();
    let host_cert = ca.issue_host(
        host_key.public_key(),
        "orbit-server",
        &hosts,
        Validity::from_now(HOST_CERTIFICATE_LIFETIME),
    )?;

    let mut listeners = tokio::task::JoinSet::new();
    for (side, addr) in [(Side::External, args.listen), (Side::Internal, args.internal_listen)] {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("не удалось занять адрес SSH {addr}"))?;
        let gate = Gate::new(side, Arc::clone(&control), Arc::clone(&trust), Arc::clone(&jump));
        listeners.spawn(ssh::serve(gate, listener, ssh::config(host_key.clone(), host_cert.clone(), side)));
    }
    let mcp_listener = TcpListener::bind(args.mcp_listen)
        .await
        .with_context(|| format!("не удалось занять адрес MCP {}", args.mcp_listen))?;
    let mut mcp_server = tokio::spawn(mcp::serve(mcp_listener, control, mcp_tokens));
    tracing::info!("orbit server принимает узлы");

    tokio::select! {
        done = listeners.join_next() => match done {
            Some(Ok(Err(error))) => return Err(error).context("слушатель SSH остановился"),
            Some(Err(error)) => anyhow::bail!("слушатель SSH упал: {error}"),
            _ => anyhow::bail!("слушатель SSH остановился"),
        },
        done = &mut mcp_server => anyhow::bail!("MCP-сервер остановился: {done:?}"),
        signal = shutdown_signal() => signal?,
    }
    tracing::info!("orbit server остановлен");
    Ok(())
}
