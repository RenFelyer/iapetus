mod runner;
mod worker;

#[cfg(test)]
#[path = "../tests/testing.rs"]
mod testing;

use clap::{Parser, Subcommand};

/// waldo — worker получает CLI-доступ к агенту на orbit session, runner ждёт от неё работу
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Получает CLI-доступ к harness агента на orbit session
    Worker(worker::Args),
    /// Ждёт, когда orbit session пришлёт ему работу
    Runner(runner::Args),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().mode {
        Mode::Worker(args) => nerve::process::runtime()?.block_on(worker::run(args))?,
        Mode::Runner(args) => {
            nerve::process::init_logging();
            nerve::process::runtime()?.block_on(Box::pin(runner::run(args)))?;
        }
    }
    Ok(())
}
