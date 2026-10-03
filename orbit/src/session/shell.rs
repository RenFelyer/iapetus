//! Подменённая оболочка orbit session.
//!
//! Агент находит оболочку по переменной окружения, по оболочке пользователя или как `bash` из `PATH` — как задаёт
//! его профиль. Если программа вызвана под именем `bash`, она работает оболочкой: служебные вызовы отдаёт настоящему
//! bash, а команду модели передаёт агенту по unix-сокету.
//!
//! Сторона сокета в `orbit session` (модуль `agent`) отдаёт команды получателю канала (мосту к runner) и
//! пересылает оболочке вывод и результат.

mod agent;
mod call;
mod client;
mod protocol;

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub(super) use agent::{Owner, ShellTask, bind, serve};
pub(super) use call::{CALLS_ENV, Calls, Invocation, classify};
pub(super) use protocol::{ExitStatus, Reply};

/// Сокет агента, если путь не задан переменной `IAPETUS_SHELL_SOCKET`.
pub(super) const DEFAULT_SOCKET: &str = "/run/iapetus/shell.sock";

/// Переменная с путём сокета агента.
pub(super) const SOCKET_ENV: &str = "IAPETUS_SHELL_SOCKET";

/// Настоящий bash для служебных вызовов.
const LOCAL_BASH: &str = "/bin/bash";

/// Программа вызвана как оболочка: имя файла в `argv[0]` оканчивается на `bash`.
pub(crate) fn invoked_as_shell(argv0: &OsString) -> bool {
    Path::new(argv0)
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().ends_with(b"bash"))
}

/// Работает оболочкой. `args` — аргументы без имени программы.
///
/// Ошибки идут в stderr, откуда их заберёт агент вместе с выводом команды.
pub(crate) async fn run(args: Vec<OsString>) -> ExitCode {
    match run_inner(&args).await {
        Ok(code) => code,
        Err(error) => {
            writeln!(std::io::stderr(), "orbit shell: {error:#}").unwrap_or_default();
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(args: &[OsString]) -> anyhow::Result<ExitCode> {
    let text_args: Option<Vec<String>> = args.iter().map(|arg| arg.to_str().map(str::to_owned)).collect();
    let text_args = text_args.ok_or_else(|| anyhow::anyhow!("аргумент оболочки не UTF-8"))?;
    let calls = std::env::var_os(CALLS_ENV);
    let calls = Calls::from_env(calls.as_ref().map(|value| value.to_str().unwrap_or("не UTF-8")))?;
    match classify(calls, &text_args)? {
        Invocation::Local => {
            // `exec` возвращается только с ошибкой.
            let error = Command::new(LOCAL_BASH).args(args).exec();
            Err(anyhow::Error::new(error).context(format!("не удалось запустить {LOCAL_BASH}")))
        }
        Invocation::Command { text, cwd_file } => {
            let socket = std::env::var_os(SOCKET_ENV).map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from);
            client::run(&socket, text, cwd_file.as_deref().map(Path::new)).await
        }
    }
}
