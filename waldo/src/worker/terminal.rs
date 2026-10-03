//! Вход в родной TUI агента: SSH через jump server к orbit session, канал pty.
//!
//! Локальный терминал уходит в raw-режим, и каждый байт клавиатуры идёт агенту как есть; размер окна передаётся при
//! входе и при каждом изменении. Выход — закрытие канала: агент продолжает работать, а другие worker видят тот же
//! экран. Закрыть канал можно клавишей [`DETACH`] или завершением агента.
//!
//! Вход после нового подключения идёт только по `Ready` от server: агент к этому моменту проснулся.
//!
//! Ожидается от server и session: jump (`iapetus-jump`) по сертификату worker проводит `direct-tcpip` на хост с
//! fingerprint сессии, session принимает сертификат с принципалом `name` и `permit-pty` и отдаёт pty агента
//! в ответ на `shell`; хост-сертификат session несёт принципал `DATA_HOST`.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;

use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nerve::connect::server::Server;
use nerve::connect::session::{Link, Target, link};
use nerve::fingerprint::Fingerprint;
use russh::ChannelMsg;
use russh::client;
use russh::keys::{Certificate, PrivateKey};
use rustix::event::{PollFd, PollFlags, Timespec};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncWriteExt, Interest};
use tokio::signal::unix::{SignalKind, signal};

/// Клавиша выхода из агента (Ctrl-]): остальные байты, включая Ctrl-C, идут агенту.
pub(super) const DETACH: u8 = 0x1d;

/// Чем кончился вход в агента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leave {
    /// Пользователь вышел сам: [`DETACH`] или конец ввода.
    Detached,
    /// Канал закрыла session: агент вышел или перезапускается, сессия кончилась.
    AgentClosed,
}

/// Соединение с session: пока значение живёт, живы оба SSH-соединения.
struct Connection {
    channel: russh::Channel<client::Msg>,
    _link: Link,
}

/// Входит в session и открывает канал pty размером `size` (столбцы, строки).
async fn connect(
    server: &Server,
    key: &PrivateKey,
    certificate: &Certificate,
    name: &str,
    session: &Fingerprint,
    size: (u16, u16),
) -> Result<Connection> {
    let target = Target {
        jump: &server.address,
        jump_host: &server.host,
        ca: &server.ca,
    };
    let link = link(&target, key, certificate, session.as_str(), name).await?;
    let channel = link.session.channel_open_session().await.context("сессия не открыла канал")?;
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_owned());
    channel
        .request_pty(true, &term, u32::from(size.0), u32::from(size.1), 0, 0, &[])
        .await
        .context("запрос pty не отправлен")?;
    channel.request_shell(true).await.context("запрос оболочки не отправлен")?;
    Ok(Connection { channel, _link: link })
}

/// Возвращает терминал в обычный режим, что бы ни случилось.
struct RawMode;

impl RawMode {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("терминал не перешёл в raw-режим")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Err(error) = disable_raw_mode() {
            tracing::warn!(%error, "raw-режим терминала не снят");
        }
    }
}

/// Копия stdin, готовая к ожиданию без потока: брошенное ожидание не оставляет чтение, которое съело бы клавишу
/// после выхода. Режим блокировки общий с настоящим stdin и не меняется, поэтому читаем только при данных: `read`
/// блокирующего файла не возвращает `WouldBlock`, и без проверки готовность `AsyncFd` осталась бы взведённой, а
/// следующее чтение остановило бы поток, а с ним и вывод агента, до следующей клавиши.
struct Stdin(AsyncFd<File>);

impl Stdin {
    fn new() -> Result<Self> {
        let fd = std::io::stdin().as_fd().try_clone_to_owned()?;
        let fd = AsyncFd::with_interest(File::from(fd), Interest::READABLE).context("stdin не терминал")?;
        Ok(Self(fd))
    }

    async fn read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            let mut file = self.0.get_ref();
            let mut polled = [PollFd::new(file, PollFlags::IN)];
            rustix::event::poll(&mut polled, Some(&Timespec::default()))?;
            if polled[0].revents().is_empty() {
                ready.clear_ready();
                continue;
            }
            return file.read(buffer);
        }
    }
}

/// Работает с агентом сессии `session`, пока не закроется канал или не нажата [`DETACH`].
pub(super) async fn attach(
    server: &Server,
    key: &PrivateKey,
    certificate: &Certificate,
    name: &str,
    session: &Fingerprint,
) -> Result<Leave> {
    let size = crossterm::terminal::size().context("размер терминала не определён")?;
    let connection = connect(server, key, certificate, name, session, size).await?;
    let mut channel = connection.channel;
    let mut winch = signal(SignalKind::window_change())?;
    let stdin = Stdin::new()?;
    let mut stdout = tokio::io::stdout();
    let _raw = RawMode::enter()?;

    let mut buffer = [0_u8; 4096];
    let left = loop {
        tokio::select! {
            read = stdin.read(&mut buffer) => {
                let read = read?;
                if read == 0 {
                    break Leave::Detached;
                }
                let input = &buffer[..read];
                if let Some(at) = input.iter().position(|byte| *byte == DETACH) {
                    channel.data(&input[..at]).await?;
                    break Leave::Detached;
                }
                channel.data(input).await?;
            }
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                Some(ChannelMsg::Failure | ChannelMsg::Eof | ChannelMsg::Close | ChannelMsg::ExitStatus { .. }) | None => break Leave::AgentClosed,
                Some(_) => {}
            },
            _ = winch.recv() => {
                let (cols, rows) = crossterm::terminal::size()?;
                channel.window_change(u32::from(cols), u32::from(rows), 0, 0).await?;
            }
        }
    };
    // Закрытие канала — выход из агента без его остановки; ошибка закрытия значит, что канал уже закрыт.
    if let Err(error) = channel.close().await {
        tracing::debug!(%error, "канал pty уже закрыт");
    }
    leave_agent_screen(&mut stdout).await?;
    std::io::stdout().flush()?;
    Ok(left)
}

/// Выводит терминал из альтернативного экрана агента. Агент включает его в терминале пользователя и после
/// закрытия канала не выключает: без этого TUI worker рисовал бы поверх остатков экрана агента.
async fn leave_agent_screen(out: &mut (impl AsyncWriteExt + Unpin)) -> std::io::Result<()> {
    out.write_all(b"\x1b[?1049l\r\n").await?;
    out.flush().await
}

#[cfg(test)]
#[path = "../../tests/worker/terminal.rs"]
mod tests;
