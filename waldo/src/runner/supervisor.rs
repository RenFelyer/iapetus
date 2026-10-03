//! Супервизор runner: внешний `waldo runner` держит настоящий runner дочерним процессом.
//!
//! Супервизор — subreaper: если runner убит (SIGKILL), его потомки — команды агента — переходят к нему, и он их
//! завершает, кроме клонов. Если убит супервизор, настоящий runner получает SIGTERM (`PR_SET_PDEATHSIG`) и
//! останавливается штатно. Одновременная гибель обоих потомков не спасает.

use std::process::Stdio;

use anyhow::{Context, bail};
use nerve::process::shutdown_signal;
use rustix::process::{Pid, Signal, getppid, kill_process, set_parent_process_death_signal};
use tokio::process::Command;

use super::reaper::{Reaper, mark_orphan_clones, own_child};

/// Запускает настоящий runner с теми же аргументами и признаком `--supervised <pid супервизора>`, ждёт его конца и
/// завершает оставшихся потомков.
pub(super) async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_default();
    // Признак — сразу после подкоманды: после `--` он стал бы позиционным аргументом.
    let subcommand = args.next().unwrap_or_default();
    let mut runner = Command::new("/proc/self/exe");
    runner
        .arg0(program)
        .arg(subcommand)
        .arg("--supervised")
        .arg(std::process::id().to_string())
        .args(args);
    supervise(runner).await
}

/// Запускает `runner` дочерним процессом и держит его, пока он не закончится; потом завершает оставшихся потомков.
async fn supervise(mut runner: Command) -> anyhow::Result<()> {
    let reaper = Reaper::start()?;
    let mut child = runner.stdin(Stdio::inherit()).spawn().context("настоящий runner не запущен")?;
    let own = child.id().map(own_child);
    let status = tokio::select! {
        status = child.wait() => status?,
        signal = shutdown_signal() => {
            signal?;
            if let Some(pid) = child.id().and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?)) {
                kill_process(pid, Signal::TERM).unwrap_or_default();
            }
            child.wait().await?
        }
    };
    drop(own);
    let _clones = mark_orphan_clones();
    reaper.stop().await;
    if !status.success() {
        bail!("настоящий runner завершился: {status}");
    }
    Ok(())
}

/// Настоящий runner просит ядро прислать ему SIGTERM, когда умрёт супервизор `supervisor`. Если супервизор успел
/// умереть раньше, сигнал не придёт, и runner не запускается.
pub(super) fn guard(supervisor: i32) -> anyhow::Result<()> {
    set_parent_process_death_signal(Some(Signal::TERM)).context("PR_SET_PDEATHSIG не выставлен")?;
    if getppid().map(|pid| pid.as_raw_nonzero().get()) != Some(supervisor) {
        bail!("супервизор {supervisor} завершился до запуска runner");
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/runner/supervisor.rs"]
mod tests;
