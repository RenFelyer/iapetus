//! waldo runner — ждёт, когда orbit session пришлёт ему работу.
//!
//! Работает в рабочем каталоге машины: подключается к назначенной сессии по каналу данных и выполняет её файловые
//! операции и команды.

mod clone;
mod control;
mod data;
mod enroll;
mod exec;
mod forbidden;
mod fs;
mod identity;
mod mcp;
mod reaper;
mod supervisor;

#[cfg(test)]
#[path = "../tests/runner.rs"]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use nerve::connect::OrbitKey;
use nerve::fingerprint::Fingerprint;
use nerve::process::shutdown_signal;
use russh::keys::PublicKey;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Рабочий каталог; без аргумента — каталог запуска. Не пересекается с путями контейнера session (`/usr`, `/tmp`,
    /// каталог состояния агента и другие): FUSE монтирует его туда по тому же пути
    #[arg(default_value = ".")]
    workdir: PathBuf,

    /// Файл `.env` с переменными окружения команд; относительный путь — от каталога запуска
    #[arg(long, env = "IAPETUS_ENV_FILE", default_value = ".env")]
    env_file: PathBuf,

    /// Отдавать `$HOME` runner сессии (чтение и запись, кроме `~/.claude`): агент видит его файлы как на runner.
    /// `off` — `$HOME` агента остаётся в контейнере session
    #[arg(long, env = "IAPETUS_HOME", action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new(), default_value = "on", value_name = "on|off")]
    home: bool,

    /// Адрес SSH orbit server (`хост:порт`): по нему runner входит на канал управления, регистрируется и подключается
    /// к сессии через jump
    #[arg(long, env = "IAPETUS_ORBIT_URL")]
    orbit_url: String,

    /// Ключ доступа к orbit server; runner сохраняет его в каталоге личности и дальше берёт оттуда, новое значение
    /// заменяет сохранённый
    #[arg(long, env = "IAPETUS_ORBIT_KEY", hide_env_values = true)]
    orbit_key: Option<OrbitKey>,

    /// Имя узла; без него — имя локального пользователя
    #[arg(long, env = "IAPETUS_NAME")]
    name: Option<String>,

    /// Каталог личности узла (fingerprint, ключ, сертификат, `orbit_key`); без него — `iapetus/runner/<каталог>-<хеш>`
    /// по рабочему каталогу в каталоге состояния пользователя (`$XDG_STATE_HOME`, `~/.local/state`)
    #[arg(long, env = "IAPETUS_IDENTITY_DIR")]
    identity_dir: Option<PathBuf>,

    /// Открытый ключ центра сертификации orbit server: по нему проверяются хост-сертификаты server, jump и сессии
    #[arg(long, env = "IAPETUS_ORBIT_CA")]
    orbit_ca: PathBuf,

    /// Временный runner (клон): останавливается через 5 минут без сессии и после её конца; клон в `git worktree`
    /// коммитит изменения в свою ветку и удаляется, клон без git и клон без единой сессии каталог не трогают. Клонов
    /// не создаёт. Каталог личности задаёт родитель (`--identity-dir`), клон удаляет его при остановке
    #[arg(long)]
    temporary: bool,

    /// Fingerprint runner, от которого создан клон
    #[arg(long, requires = "temporary")]
    parent: Option<Fingerprint>,

    /// Служебный признак: настоящий runner, запущенный супервизором с этим pid; без него `waldo runner` — супервизор
    #[arg(long, hide = true)]
    supervised: Option<i32>,
}

pub(crate) async fn run(args: Args) -> anyhow::Result<()> {
    if let Some(supervisor) = args.supervised {
        supervisor::guard(supervisor)?;
    }
    let workdir = checked_workdir(&args.workdir)?;
    if args.temporary && args.identity_dir.is_none() {
        bail!("клону нужен каталог личности от родителя: --identity-dir");
    }
    let enrolled = enroll::enroll(&args, &workdir)?;
    if identity::orbit_key_in_process() {
        // Команды runner видят окружение и аргументы его процесса через /proc; orbit_key уже в каталоге личности.
        drop(enrolled);
        return Err(identity::exec_without_orbit_key());
    }
    if args.supervised.is_none() {
        // Каталог личности занимает настоящий runner; ключ уже в нём, а процесс без ключа в аргументах и окружении.
        drop(enrolled);
        return supervisor::run().await;
    }
    let reaper = reaper::Reaper::start()?;
    let served = Box::pin(serve(args, workdir, enrolled)).await;
    // Потомки команд завершаются при любом выходе runner: по сигналу, по концу клона и по ошибке.
    reaper.stop().await;
    served
}

/// Канонический рабочий каталог, если он не пересекается с путями контейнера session.
fn checked_workdir(dir: &Path) -> anyhow::Result<PathBuf> {
    let workdir = std::fs::canonicalize(dir).with_context(|| format!("рабочий каталог {} недоступен", dir.display()))?;
    forbidden::check(&workdir).map_err(|e| anyhow::anyhow!("рабочий каталог runner не подходит: {e}"))?;
    Ok(workdir)
}

/// Канонический `$HOME`, который runner отдаёт сессии; `None` — отдавать нечего: флаг `--home off`, `$HOME` не задан,
/// не существует или пересекается с путями контейнера session.
fn home_dir(args: &Args, workdir: &Path) -> Option<PathBuf> {
    if !args.home {
        return None;
    }
    let home = std::env::var_os("HOME").and_then(|home| std::fs::canonicalize(home).ok())?;
    if let Err(reason) = forbidden::check(&home) {
        tracing::warn!(home = %home.display(), reason, "$HOME не отдаётся сессии");
        return None;
    }
    tracing::info!(home = %home.display(), workdir = %workdir.display(), "$HOME отдаётся сессии");
    Some(home)
}

/// Отправляет изменения `$HOME`; без `$HOME` ждёт вечно.
async fn watch_home(data: &data::Data, home: Option<&Arc<fs::Workdir>>) -> anyhow::Result<()> {
    match home {
        Some(home) => fs::serve_watch(data.link(), Arc::clone(home)).await,
        None => std::future::pending().await,
    }
}

async fn serve(args: Args, workdir: PathBuf, enrolled: enroll::Enrolled) -> anyhow::Result<()> {
    let root = fs::Workdir::open(&workdir).with_context(|| format!("рабочий каталог {} не открыт", workdir.display()))?;
    let home = home_dir(&args, &workdir)
        .map(|dir| fs::Workdir::open_home(&dir, &workdir).with_context(|| format!("$HOME {} не открыт", dir.display())))
        .transpose()?
        .map(Arc::new);
    let node = enrolled.node(&args, args.parent.clone()).await?;
    let fingerprint = enrolled.identity.fingerprint().clone();
    tracing::info!(
        workdir = %workdir.display(),
        env_file = %args.env_file.display(),
        orbit_url = %args.orbit_url,
        %fingerprint,
        name = %enrolled.name,
        "waldo runner запущен"
    );
    let ca = PublicKey::read_openssh_file(&args.orbit_ca).with_context(|| format!("ключ CA {} не прочитан", args.orbit_ca.display()))?;
    let settings = data::Settings {
        jump: args.orbit_url.clone(),
        ca,
        identity: Arc::clone(&enrolled.identity),
    };
    let root = Arc::new(root);
    let env = Arc::new(control::env::ServerEnv::new(fingerprint.clone(), args.parent.clone()));
    let env_file = std::path::absolute(&args.env_file).with_context(|| format!("путь {} не определён", args.env_file.display()))?;
    let executor = Arc::new(exec::Executor::new(
        Arc::clone(&root),
        env_file,
        std::env::vars_os().collect(),
        Some(Arc::clone(&env)),
    ));
    let data = Arc::new(data::Data::new(home.clone()));
    let host = control::host::collect(&workdir, home.is_some());
    let lifetime = args
        .temporary
        .then(|| Arc::new(clone::Lifetime::restored(Arc::clone(&enrolled.identity))));
    let parent = if args.temporary {
        None
    } else {
        Some(clone::Parent {
            workdir: workdir.clone(),
            identity_dir: enrolled.dir().to_owned(),
            exe: std::env::current_exe().context("исполняемый файл waldo не определён")?,
            fingerprint: fingerprint.clone(),
            orbit_url: args.orbit_url.clone(),
            orbit_ca: std::path::absolute(&args.orbit_ca).context("путь ключа CA не определён")?,
            orbit_key: enrolled.orbit_key().to_owned(),
            home: args.home,
        })
    };
    let stop = tokio::select! {
        served = data.run(&settings, Arc::clone(&root), Arc::clone(&executor)) => { served?; None }
        watched = fs::serve_watch(data.link(), root) => { watched?; None }
        watched = watch_home(&data, home.as_ref()) => { watched?; None }
        denied = node.run(async |channel| {
            control::serve(channel, &env, &executor, &data, lifetime.as_deref(), parent.as_ref(), &host).await;
        }) => return Err(denied.into()),
        stored = enrolled.store_certificates(node.certificates()) => { stored?; None }
        stop = clone::stopped(lifetime.as_deref()) => Some(stop),
        signal = shutdown_signal() => { signal?; None }
    };
    let finished = if let (Some(stop), Some(lifetime)) = (stop, &lifetime) {
        tracing::info!(?stop, "временный runner останавливается");
        clone::finish(&workdir, lifetime).await;
        true
    } else {
        false
    };
    if finished && let Err(e) = enrolled.discard_identity() {
        tracing::warn!(error = format!("{e:#}"), "личность клона не удалена");
    }
    tracing::info!("waldo runner остановлен");
    Ok(())
}
