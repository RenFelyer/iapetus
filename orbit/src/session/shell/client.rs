//! Сторона оболочки: тонкий клиент, который передаёт команду агенту и
//! повторяет у себя её вывод, код выхода и cwd.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};

use super::protocol::{ClientMessage, ExitStatus, Reply, read_frame, write_frame};

/// Выполняет команду через агента и возвращает код выхода оболочки.
///
/// Вывод идёт в stdout. Cwd записывается в `cwd_file` (он есть только у агентов
/// с шаблонами вызовов) после успешной команды: вызов агента пишет его через `&&`, и после
/// неудачной команды прежний cwd остаётся.
pub(super) async fn run(socket: &Path, command: String, cwd_file: Option<&Path>) -> anyhow::Result<ExitCode> {
    let current_dir = std::env::current_dir().context("не удалось узнать текущий каталог")?;
    let cwd = current_dir
        .to_str()
        .with_context(|| format!("текущий каталог не UTF-8: {}", current_dir.display()))?
        .to_owned();

    // Обработчики ставятся до отправки команды: сигнал, пришедший раньше, не потеряется.
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;

    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("нет соединения с агентом {}", socket.display()))?;
    let (mut reader, mut writer) = stream.into_split();
    write_frame(&mut writer, &ClientMessage::Run { command, cwd })
        .await
        .context("не удалось отправить команду агенту")?;

    // Писатель остаётся жить до конца: его закрытие агент принял бы за пропажу оболочки.
    let canceller = tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        write_frame(&mut writer, &ClientMessage::Cancel).await.unwrap_or_default();
        std::future::pending::<()>().await;
    });

    let result = relay_reply(&mut reader, cwd_file).await;
    canceller.abort();
    result
}

async fn relay_reply(reader: &mut tokio::net::unix::OwnedReadHalf, cwd_file: Option<&Path>) -> anyhow::Result<ExitCode> {
    let mut stdout = tokio::io::stdout();
    loop {
        let reply = read_frame(reader).await.context("агент оборвал соединение до завершения команды")?;
        match reply {
            Reply::Output(bytes) => {
                stdout.write_all(&bytes).await.context("не удалось записать вывод")?;
                stdout.flush().await.context("не удалось записать вывод")?;
            }
            Reply::Exit { status, cwd } => {
                if let Some(cwd_file) = cwd_file
                    && status == ExitStatus::Code(0)
                {
                    tokio::fs::write(cwd_file, format!("{cwd}\n"))
                        .await
                        .with_context(|| format!("не удалось записать cwd в {}", cwd_file.display()))?;
                }
                return Ok(exit_code(status));
            }
            Reply::Failed(reason) => bail!("команда не выполнена: {reason}"),
        }
    }
}

/// Код выхода оболочки, как у bash: код команды или 128 плюс номер сигнала.
fn exit_code(status: ExitStatus) -> ExitCode {
    let code = match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(signal) => 128_i32.saturating_add(signal),
    };
    ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1))
}
