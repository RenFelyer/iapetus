use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Включает журнал в stderr; уровень берётся из `RUST_LOG`, по умолчанию `info`.
pub fn init_logging() {
    tracing_subscriber::fmt().with_env_filter(filter()).init();
}

/// Включает журнал в файл `path` (создаётся с правами 0600, дописывается): для программ, у которых stderr занят
/// экраном. Уровень — как у [`init_logging`].
pub fn init_logging_to_file(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}

/// Многопоточный runtime tokio. Программы собирают его сами, а не через `#[tokio::main]`: до его создания процесс
/// остаётся однопоточным.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()
}

/// Ждёт SIGINT или SIGTERM.
///
/// SIGTERM нужен, потому что им `docker stop` останавливает процесс, а
/// orbit session — PID 1 в контейнере.
#[cfg(unix)]
pub async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

/// Ждёт Ctrl+C.
#[cfg(not(unix))]
pub async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
