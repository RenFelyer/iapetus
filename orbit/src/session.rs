//! orbit session — ждёт, пока её разбудит waldo runner: его назначает server по запросу waldo worker.
//!
//! Перехватывает действия агента в контейнере (файлы через FUSE, команды через оболочку, запросы к API через
//! шлюз) и отдаёт их runner по каналу данных.

mod agent;
mod config;
mod control;
mod data;
mod events;
mod exec;
mod fs;
mod gateway;
mod host;
mod launch;
mod mcp;
mod rules;
mod shell;
mod ssh;
mod state;
mod terminal;
mod update;

pub(crate) use launch::{DropExecArgs, drop_and_exec};
pub(crate) use shell::{invoked_as_shell, run as run_shell};

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agent::{Profile, Setup};
use anyhow::Context;
use config::{AgentProfile, DataServer, agent_setup, data_server, fuse_config, install_secrets};
use control::Link;
use events::{advance_epoch_at_end_of_turn, log_events, preload_files};
use gateway::{Api, Config};
use host::Workspace;
use nerve::connect::server::Server;
use nerve::fingerprint::Fingerprint;
use nerve::process::shutdown_signal;
use nerve::protocol::Root;
use nerve::release;
use russh::keys::PublicKey;
use shell::Owner;
use state::{AgentEvent, Verdict, follow};
use terminal::Terminal;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Адрес внутреннего SSH-слушателя orbit server, `хост:порт`: канал управления; данные сессии идут по каналу данных
    #[arg(long, env = "IAPETUS_ORBIT_URL")]
    orbit_url: String,

    /// Fingerprint сессии: под ним она входит на server (`session-<fp>`)
    #[arg(long, env = "IAPETUS_SESSION")]
    session: Fingerprint,

    /// Файл с секретом сессии, который выдал server при создании
    #[arg(long, env = "IAPETUS_SESSION_SECRET_FILE", default_value = "/run/secrets/session-secret")]
    session_secret_file: PathBuf,

    /// Секрет сессии значением: при старте записывается в `--session-secret-file`
    #[arg(long, env = "IAPETUS_SESSION_SECRET_CONTENT", hide_env_values = true)]
    session_secret_content: Option<String>,

    /// Адрес MCP-сервера orbit server для агента (`http://хост:порт/mcp`); без него агент не получает MCP вовсе
    #[arg(long, env = "IAPETUS_MCP_URL")]
    mcp_url: Option<String>,

    /// Токен MCP этой сессии: отдельный от её секрета, агент его видит
    #[arg(long, env = "IAPETUS_MCP_TOKEN", requires = "mcp_url")]
    mcp_token: Option<agent::McpToken>,

    /// Каталог кэш-файлов FUSE (tmpfs)
    #[arg(long, env = "IAPETUS_CACHE_DIR", default_value = "/work")]
    cache_dir: PathBuf,

    /// Наибольший файл в байтах; без него — размер файловой системы каталога кэша (tmpfs `/work`)
    #[arg(long, env = "IAPETUS_MAX_FILE")]
    max_file: Option<u64>,

    /// Агент коробки: от него зависят файлы первого запуска, окружение, API шлюза и адрес upstream
    #[arg(long, env = "IAPETUS_AGENT")]
    agent: Profile,

    /// Как агент записывает файлы; без значения — как пишет выбранный агент
    #[arg(long, env = "IAPETUS_AGENT_PROFILE", value_enum)]
    agent_profile: Option<AgentProfile>,

    /// Fingerprint назначенного runner: его одного пускают в канал данных, от него зависят имена кэш-файлов
    #[arg(long, env = "IAPETUS_RUNNER")]
    runner: Fingerprint,

    /// Адрес SSH-сервера канала данных: сюда runner приходит через jump orbit server
    #[arg(long, env = "IAPETUS_DATA_LISTEN", default_value = "0.0.0.0:2222")]
    data_listen: SocketAddr,

    /// Закрытый ключ хоста канала данных (OpenSSH)
    #[arg(long, env = "IAPETUS_DATA_HOST_KEY", default_value = "/run/secrets/data-host-key")]
    data_host_key: PathBuf,

    /// Ключ хоста значением: при старте записывается в `--data-host-key`
    #[arg(long, env = "IAPETUS_DATA_HOST_KEY_CONTENT", hide_env_values = true)]
    data_host_key_content: Option<String>,

    /// Хост-сертификат сессии на этот ключ, подписанный центром сертификации orbit server
    #[arg(long, env = "IAPETUS_DATA_HOST_CERT", default_value = "/run/secrets/data-host-cert")]
    data_host_cert: PathBuf,

    /// Хост-сертификат значением: при старте записывается в `--data-host-cert`
    #[arg(long, env = "IAPETUS_DATA_HOST_CERT_CONTENT", hide_env_values = true)]
    data_host_cert_content: Option<String>,

    /// Открытый ключ центра сертификации orbit server: по нему проверяются сертификаты runner
    #[arg(long, env = "IAPETUS_CA_PUBLIC_KEY", default_value = "/run/secrets/ca.pub")]
    ca_public_key: PathBuf,

    /// Открытый ключ CA значением: при старте записывается в `--ca-public-key`
    #[arg(long, env = "IAPETUS_CA_PUBLIC_KEY_CONTENT", hide_env_values = true)]
    ca_public_key_content: Option<String>,

    /// Продолжить последний разговор агента (новый контейнер на прежнем томе): к команде агента добавляется его resume
    #[arg(long, env = "IAPETUS_RESUME", value_parser = clap::builder::BoolishValueParser::new())]
    resume: bool,

    /// Домашний каталог пользователя агента: корень состояния агента
    #[arg(long, env = "IAPETUS_AGENT_HOME", default_value = nerve::agent::AGENT_ROOT)]
    agent_home: PathBuf,

    /// Адрес шлюза: его агент получает как базовый адрес своего API
    #[arg(long, env = "IAPETUS_GATEWAY_LISTEN", default_value = "127.0.0.1:8080")]
    gateway_listen: SocketAddr,

    /// Какое API говорит агент: от него зависит разбор ответов шлюзом; без значения — API выбранного агента
    #[arg(long, env = "IAPETUS_GATEWAY_API", value_enum)]
    gateway_api: Option<Api>,

    /// Адрес API агента, куда шлюз пересылает запросы; без значения — адрес подписки выбранного агента
    #[arg(long, env = "IAPETUS_UPSTREAM")]
    upstream: Option<String>,

    /// Сокет, на который подменённая оболочка присылает команды агента
    #[arg(long, env = shell::SOCKET_ENV, default_value = shell::DEFAULT_SOCKET)]
    shell_socket: PathBuf,

    /// uid пользователя агента: ему отдаётся сокет оболочки
    #[arg(long, env = "IAPETUS_SHELL_UID", default_value_t = 1000)]
    shell_uid: u32,

    /// gid пользователя агента
    #[arg(long, env = "IAPETUS_SHELL_GID", default_value_t = 1000)]
    shell_gid: u32,

    /// Запустить агента выбранного `--agent` программой по умолчанию, когда команда после `--` не задана: так
    /// запускает session контейнер, созданный server, у которого команды нет
    #[arg(long, env = "IAPETUS_START_AGENT")]
    start_agent: bool,

    /// Команда агента; запускается от пользователя `--shell-uid` без `CAP_SYS_ADMIN`. Без неё сессия только
    /// монтирует ФС и обслуживает шлюз и оболочку
    #[arg(last = true)]
    agent_command: Vec<String>,
}

/// Сколько событий шлюза буферизует канал для медленного получателя.
const EVENT_BUFFER: usize = 256;

/// Сколько указаний server агенту ждёт того, кто держит его процесс.
const AGENT_COMMAND_BUFFER: usize = 4;

/// Сколько команд оболочки ждёт получателя, пока он занят.
const SHELL_TASK_BUFFER: usize = 16;

/// Кладёт файл правил Iapetus до запуска агента; без команды агента правил нет.
fn write_rules(args: &Args, setup: Setup) -> anyhow::Result<Option<rules::Rules>> {
    if args.agent_command.is_empty() {
        return Ok(None);
    }
    let rules = rules::Rules::new(args.agent, setup, Some(args.runner.clone()));
    rules.write().context("правила Iapetus не записаны")?;
    Ok(Some(rules))
}

/// Где и как агент запускается: настройки профиля и файловая система проекта.
struct Placement<'a> {
    setup: &'a Setup,
    workspace: &'a Workspace,
}

/// Запускает агента, когда подключился runner; без команды агента не запускает ничего и не возвращается.
async fn run_agent(
    args: &Args,
    placement: &Placement<'_>,
    link: &mut data::Link,
    terminal: &watch::Sender<Option<Terminal>>,
    running: &watch::Sender<bool>,
    events: &mpsc::UnboundedSender<AgentEvent>,
    mut restarts: launch::Restarts<'_>,
) -> anyhow::Result<Infallible> {
    if args.agent_command.is_empty() {
        // Агента нет: указания принимаются и отбрасываются, чтобы очередь не заполнилась, а ФС следует за проектом.
        let mut environment = restarts.environment.clone();
        loop {
            tokio::select! {
                command = restarts.commands.recv() => if command.is_none() { return std::future::pending().await },
                changed = environment.changed() => {
                    changed.context("назначения runner закрыты")?;
                    let current = environment.borrow_and_update().clone();
                    if let Some(current) = current {
                        placement.workspace.mount(&current)?;
                    }
                }
            }
        }
    }
    let environment = restarts.environment.clone();
    let start = |resumed: bool| {
        let environment = environment
            .borrow()
            .clone()
            .context("окружение runner не известно к запуску агента")?;
        placement.workspace.mount(&environment)?;
        agent::start(args.agent, placement.setup, &environment, &args.agent_command, resumed)
    };
    launch::run_after_runner(link, start, args.resume, terminal, running, events, restarts).await
}

/// Ожидающее обновление агента (или причина отказа) и тот, кто его выполнит. Обновляется агент, который запускает эта
/// session и у профиля которого есть обновление.
fn agent_updates(
    args: &Args,
    viewers: watch::Receiver<usize>,
    turn: watch::Receiver<bool>,
    commands: mpsc::Sender<launch::AgentCommand>,
) -> (Result<watch::Sender<Option<update::Offer>>, &'static str>, Option<update::Updater>) {
    let Some(profile) = args.agent.updates() else {
        return (Err(release::NOT_UPDATABLE), None);
    };
    let Some(program) = args.agent_command.first() else {
        return (Err(release::NO_AGENT_COMMAND), None);
    };
    let offers = watch::channel(None).0;
    let updater = update::Updater {
        agent: args.agent.0,
        offers: offers.clone(),
        viewers,
        turn,
        commands,
        profile,
        program: program.clone(),
        search_path: std::env::var_os("PATH"),
        owner: 0,
    };
    (Ok(offers), Some(updater))
}

/// Выполняет обновления агента; без `updater` ждёт вечно.
async fn run_updater(updater: Option<update::Updater>) {
    match updater {
        Some(updater) => updater.run().await,
        None => std::future::pending().await,
    }
}

/// Внутренний слушатель server: имя хоста из `orbit_url` сверяется с хост-сертификатом server.
fn orbit_server(args: &Args) -> anyhow::Result<Server> {
    let (host, _) = args
        .orbit_url
        .rsplit_once(':')
        .with_context(|| format!("адрес server {} без порта", args.orbit_url))?;
    let ca = PublicKey::read_openssh_file(&args.ca_public_key).with_context(|| format!("ключ CA {}", args.ca_public_key.display()))?;
    Ok(Server {
        address: args.orbit_url.clone(),
        host: host.to_owned(),
        ca,
    })
}

fn log_start(args: &Args, upstream: &str) {
    tracing::info!(
        agent = ?args.agent.0,
        orbit_url = %args.orbit_url,
        cache_dir = %args.cache_dir.display(),
        runner = %args.runner,
        data_listen = %args.data_listen,
        gateway_listen = %args.gateway_listen,
        %upstream,
        "orbit session запущен"
    );
}

/// Занимает сокет оболочки агента и запускает мост, который передаёт её команды runner.
fn start_shell(
    args: &Args,
    link: &data::Link,
    epoch: fs::Epoch,
) -> anyhow::Result<(tokio::net::UnixListener, mpsc::Sender<shell::ShellTask>)> {
    let listener = shell::bind(
        &args.shell_socket,
        Owner {
            uid: args.shell_uid,
            gid: args.shell_gid,
        },
    )
    .with_context(|| format!("не удалось занять сокет оболочки {}", args.shell_socket.display()))?;
    let (tasks, tasks_rx) = mpsc::channel(SHELL_TASK_BUFFER);
    tokio::spawn(exec::Bridge::new(link.clone(), epoch).serve(tasks_rx));
    Ok((listener, tasks))
}

pub(crate) async fn run(mut args: Args) -> anyhow::Result<()> {
    if args.start_agent && args.agent_command.is_empty() {
        args.agent_command = vec![args.agent.isolation().program.to_owned()];
    }
    let upstream = args.upstream.clone().unwrap_or_else(|| args.agent.isolation().upstream.to_owned());
    let gateway_api = args.gateway_api.unwrap_or_else(|| args.agent.gateway_api());
    log_start(&args, &upstream);

    // Порт проектного MCP нужен профилю агента до его запуска: сокет занимается заранее.
    let project_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("не удалось занять порт проектного MCP")?;
    let project_port = project_listener.local_addr().context("адрес проектного MCP не определён")?.port();
    let setup = agent_setup(&args, project_port);
    install_secrets(&args)?;
    let secret = std::fs::read_to_string(&args.session_secret_file)
        .with_context(|| format!("секрет сессии {} не прочитан", args.session_secret_file.display()))?;
    let orbit = orbit_server(&args)?;
    let DataServer {
        server: data_server,
        terminal,
        data,
        link,
        events: data_events,
    } = data_server(&args)?;
    let data_listener = TcpListener::bind(args.data_listen)
        .await
        .with_context(|| format!("не удалось занять адрес канала данных {}", args.data_listen))?;
    let config = fuse_config(&args, Root::Project)?;
    let home_config = fuse_config(&args, Root::Home)?;
    // `Epoch::advance` начинает новую эпоху «нового обращения»: после выполненной команды и в конце хода.
    let epoch = fs::Epoch::new();
    let vfs = Arc::new(fs::Vfs::new(link.clone(), config, epoch.clone()).context("кэш FUSE не создан")?);
    let home_vfs = Arc::new(fs::Vfs::new(link.clone(), home_config, epoch.clone()).context("кэш FUSE $HOME не создан")?);
    let vfses: Arc<[Arc<fs::Vfs>]> = Arc::new([Arc::clone(&vfs), Arc::clone(&home_vfs)]);

    let listener = TcpListener::bind(args.gateway_listen)
        .await
        .with_context(|| format!("не удалось занять адрес шлюза {}", args.gateway_listen))?;
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    tokio::spawn(log_events(events.subscribe()));
    tokio::spawn(advance_epoch_at_end_of_turn(events.subscribe(), epoch.clone()));
    tokio::spawn(preload_files(events.subscribe(), Arc::clone(&vfses)));
    let (turn, turn_rx) = watch::channel(false);
    tokio::spawn(state::track_turn(events.subscribe(), turn));
    let (agent_commands, agent_commands_rx) = mpsc::channel(AGENT_COMMAND_BUFFER);
    let (agent_update, updater) = agent_updates(&args, data_server.viewers(), turn_rx.clone(), agent_commands.clone());
    let (agent_events, agent_events_rx) = mpsc::unbounded_channel();
    let (verdict, verdict_rx) = watch::channel(Verdict::waiting());
    tokio::spawn(follow(events.subscribe(), link.clone(), agent_events_rx, verdict));

    let (shell_listener, shell_tasks) = start_shell(&args, &link, epoch)?;

    // ФС монтируется по назначению runner, по пути его проекта. Агент стартует после монтирования и подключения runner:
    // его рабочий каталог — файловая система сессии, уже отвечающая на запросы.
    let workspace = Arc::new(config::workspace(&args, &setup, Arc::clone(&vfs), Arc::clone(&home_vfs)));
    let (environment, environment_rx) = watch::channel(None);
    let (agent_running, agent_running_rx) = watch::channel(false);
    let rules = write_rules(&args, setup.clone())?;
    let project = mcp::Project::new(link.clone(), args.agent.mcp_name_limit());

    let gateway_config = Config::new(gateway_api, &upstream);
    let control = Link {
        server: orbit,
        session: args.session.clone(),
        secret: secret.trim().to_owned(),
        data,
        vfs: Arc::clone(&vfs),
        runner: watch::channel(Some(args.runner.clone())).0,
        activity: gateway_config.activity.subscribe(),
        agent: agent_running_rx,
        state: verdict_rx,
        rules,
        project: project.clone(),
        profile: args.agent,
        agent_update,
        commands: agent_commands,
        workspace: Arc::clone(&workspace),
        environment,
    };

    // Остановка и перезапуск по указанию server поднимают флаг перед остановкой агента: такой выход — не `failure`.
    let planned_exit = AtomicBool::new(false);
    let restarts = launch::Restarts {
        commands: agent_commands_rx,
        turn: turn_rx,
        viewers: data_server.viewers(),
        planned_exit: &planned_exit,
        project: project.clone(),
        environment: environment_rx.clone(),
    };
    let mut agent_link = link;
    let placement = Placement {
        setup: &setup,
        workspace: &workspace,
    };
    let result = tokio::select! {
        served = gateway::serve(listener, gateway_config, events) => served,
        served = shell::serve(shell_listener, shell_tasks) => served.map_err(Into::into),
        served = data_server.serve(data_listener) => served.context("сервер канала данных остановился"),
        () = fs::follow_watch(data_events, vfses.to_vec()) => anyhow::bail!("события канала данных закончились"),
        served = mcp::serve(project_listener, project) => served.context("проектный MCP остановился"),
        exited = run_agent(&args, &placement, &mut agent_link, &terminal, &agent_running, &agent_events, restarts) => exited.map(|never| match never {}),
        () = run_updater(updater) => anyhow::bail!("обновление Claude Code остановилось"),
        denied = control.run() => Err(denied.context("канал управления закрыт")),
        signal = shutdown_signal() => signal.map_err(Into::into),
    };
    tracing::info!("orbit session остановлен");
    result
}
