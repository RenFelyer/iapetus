mod server;
mod session;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// orbit — server ждёт внешних подключений, session ждёт, пока её разбудит waldo runner
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Запускается в Docker и ждёт внешних подключений
    Server(Box<server::Args>),
    /// Ждёт, пока её разбудит waldo runner: его назначает server по запросу waldo worker
    Session(Box<session::Args>),
    /// Понижает права и запускает команду агента; сессия вызывает её сама
    #[command(hide = true)]
    DropExec(session::DropExecArgs),
}

fn main() -> anyhow::Result<ExitCode> {
    let mut args = std::env::args_os();
    // Под именем `bash` программа — оболочка Claude Code: без журнала в stdout и без clap.
    if let Some(argv0) = args.next()
        && session::invoked_as_shell(&argv0)
    {
        return nerve::process::runtime()?.block_on(async { Ok(session::run_shell(args.collect()).await) });
    }
    nerve::process::init_logging();
    match Cli::parse().mode {
        // Без runtime tokio: процесс должен остаться однопоточным до `exec`, иначе смена uid задела бы один поток из многих.
        Mode::DropExec(args) => match session::drop_and_exec(&args)? {},
        Mode::Server(args) => nerve::process::runtime()?.block_on(server::run(*args))?,
        Mode::Session(args) => nerve::process::runtime()?.block_on(session::run(*args))?,
    }
    Ok(ExitCode::SUCCESS)
}
