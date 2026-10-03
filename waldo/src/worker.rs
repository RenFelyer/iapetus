//! waldo worker — получает CLI-доступ к harness агента на orbit session.
//!
//! Личности у worker нет: входит на server по `orbit_key` и получает короткий сертификат SSH на ключ этого запуска;
//! дальше держит канал управления. Новый `orbit_key` на server закрывает его соединение. Журнал пишется в файл рядом с настройками.

mod control;
mod settings;
mod terminal;
mod tui;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use nerve::connect;
use nerve::process::shutdown_signal;

use control::Control;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Файл настроек (`orbit_url`, `orbit_ca`, `orbit_key`, `name`) с правами 0600; по умолчанию
    /// `$XDG_CONFIG_HOME/iapetus/worker.conf`
    #[arg(long, env = "IAPETUS_WORKER_SETTINGS")]
    settings: Option<PathBuf>,

    /// Файл `.env` с `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL` и `GITHUB_TOKEN` для новых сессий; относительный путь —
    /// от каталога запуска
    #[arg(long, env = "IAPETUS_ENV_FILE", default_value = ".env")]
    env_file: PathBuf,
}

pub(crate) async fn run(args: Args) -> Result<()> {
    let path = match &args.settings {
        Some(path) => path.clone(),
        None => settings::default_path()?,
    };
    // Экран занят TUI, поэтому журнал — в файл рядом с настройками: `worker.conf` → `worker.log`.
    let log = path.with_extension("log");
    nerve::process::init_logging_to_file(&log).with_context(|| format!("журнал {} не открыт", log.display()))?;
    let settings = settings::load(&path)?;
    let name = match settings.name {
        Some(name) => name,
        None => connect::default_name()?,
    };
    let control = Control::new(
        &settings.orbit_url,
        &settings.orbit_ca,
        settings.orbit_key.expose(),
        &name,
        args.env_file,
    )?;
    tracing::info!(%name, "waldo worker запущен");
    tokio::select! {
        denied = control.run() => bail!("{denied}"),
        result = tui::run(&control, &name) => result?,
        signal = shutdown_signal() => signal?,
    }
    tracing::info!("waldo worker остановлен");
    Ok(())
}
