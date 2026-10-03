//! Терминал агента: pty, в котором работает родной TUI агента, и зрители — входящие worker.
//!
//! tmux нет: вывод агента получают все зрители, ввод принимается от любого, размер pty — наименьший из окон зрителей
//! (без зрителей остаётся прежним). Новому зрителю уходит копия экрана из эмулятора `vt100`. Зритель с окном выше
//! pty получает область прокрутки высотой pty, чтобы агент не рисовал ниже своих строк. Уход зрителя агента не
//! касается. Ответы терминалов зрителей на запросы агента (DA1, XTVERSION, kitty keyboard) в pty не попадают:
//! агент спросил один раз, а отвечают все зрители.
//!
//! Всё состояние держит одна задача ([`Terminal::start`]): зрители, эмулятор и ввод — её команды, замков нет.

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use pty_process::{Pty, Size as PtySize};
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

#[cfg(test)]
#[path = "../../tests/session/terminal.rs"]
mod tests;

/// Размер pty, пока нет ни одного зрителя.
const INITIAL_SIZE: Size = Size { rows: 24, cols: 80 };

/// Сколько кусков вывода ждёт зрителя; зритель, который отстал больше, отключается.
const VIEWER_BUFFER: usize = 1024;

/// Сколько введённых байт ждёт, пока агент их прочтёт; лишнее отбрасывается.
const MAX_PENDING_INPUT: usize = 1 << 20;

/// Размер окна в символах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    /// Окно без строк или столбцов бессмысленно: клиент прислал «неизвестно».
    pub(super) fn new(rows: u32, cols: u32) -> Self {
        let clamp = |value: u32| u16::try_from(value).unwrap_or(u16::MAX).max(1);
        Self {
            rows: clamp(rows),
            cols: clamp(cols),
        }
    }
}

#[derive(Debug)]
enum Command {
    Attach { id: u64, size: Size, output: mpsc::Sender<Bytes> },
    Input(Bytes),
    Resize { id: u64, size: Size },
    Detach(u64),
}

/// Управление терминалом; дешёвая копия.
#[derive(Debug, Clone)]
pub(super) struct Terminal {
    commands: mpsc::UnboundedSender<Command>,
}

impl Terminal {
    /// Запускает задачу терминала на pty с уже запущенным агентом.
    pub(super) fn start(pty: Pty) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run(pty, receiver));
        Self { commands }
    }

    /// Подключает зрителя `id` с окном `size`. В канале сначала копия экрана, затем вывод агента; канал закрывается,
    /// когда агент вышел или зритель отстал. `None` — терминала уже нет.
    pub(super) fn attach(&self, id: u64, size: Size) -> Option<mpsc::Receiver<Bytes>> {
        let (output, receiver) = mpsc::channel(VIEWER_BUFFER);
        self.commands.send(Command::Attach { id, size, output }).ok()?;
        Some(receiver)
    }

    pub(super) fn input(&self, data: Bytes) {
        self.commands.send(Command::Input(data)).unwrap_or_default();
    }

    pub(super) fn resize(&self, id: u64, size: Size) {
        self.commands.send(Command::Resize { id, size }).unwrap_or_default();
    }

    /// Отключает зрителя; на агента это не влияет.
    pub(super) fn detach(&self, id: u64) {
        self.commands.send(Command::Detach(id)).unwrap_or_default();
    }
}

/// То, что `vt100` не помнит и что нужно дописать к копии экрана: заголовок окна (`OSC 0`) и режимы `?1004`
/// (события фокуса) и `?2031` (уведомления о цветовой схеме).
#[derive(Debug, Default)]
struct Lost {
    title: Option<Vec<u8>>,
    focus_events: bool,
    color_scheme: bool,
}

impl vt100::Callbacks for Lost {
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(title.to_vec());
    }

    fn unhandled_csi(&mut self, _screen: &mut vt100::Screen, first: Option<u8>, _second: Option<u8>, params: &[&[u16]], action: char) {
        if first != Some(b'?') || !matches!(action, 'h' | 'l') {
            return;
        }
        let on = action == 'h';
        for param in params {
            match param {
                [1004] => self.focus_events = on,
                [2031] => self.color_scheme = on,
                _ => {}
            }
        }
    }
}

impl Lost {
    fn append_to(&self, snapshot: &mut Vec<u8>) {
        if let Some(title) = &self.title {
            snapshot.extend_from_slice(b"\x1b]0;");
            snapshot.extend_from_slice(title);
            snapshot.push(0x07);
        }
        if self.focus_events {
            snapshot.extend_from_slice(b"\x1b[?1004h");
        }
        if self.color_scheme {
            snapshot.extend_from_slice(b"\x1b[?2031h");
        }
    }
}

#[derive(Debug)]
struct Viewer {
    size: Size,
    output: mpsc::Sender<Bytes>,
}

/// Состояние задачи терминала.
struct State {
    parser: vt100::Parser<Lost>,
    viewers: BTreeMap<u64, Viewer>,
    size: Size,
    /// Введённое и ещё не записанное в pty.
    pending: VecDeque<u8>,
}

impl State {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(INITIAL_SIZE.rows, INITIAL_SIZE.cols, 0, Lost::default()),
            viewers: BTreeMap::new(),
            size: INITIAL_SIZE,
            pending: VecDeque::new(),
        }
    }

    /// Копия экрана для нового зрителя с окном `size`.
    fn snapshot(&self, size: Size) -> Vec<u8> {
        let mut snapshot = region(size, self.size);
        snapshot.extend(self.parser.screen().state_formatted());
        self.parser.callbacks().append_to(&mut snapshot);
        snapshot
    }

    /// Размер pty по окнам зрителей: наименьшие строки и столбцы. Без зрителей — прежний.
    fn wanted_size(&self) -> Size {
        let rows = self.viewers.values().map(|viewer| viewer.size.rows).min();
        let cols = self.viewers.values().map(|viewer| viewer.size.cols).min();
        match (rows, cols) {
            (Some(rows), Some(cols)) => Size { rows, cols },
            _ => self.size,
        }
    }

    /// Приводит pty и эмулятор к размеру по окнам зрителей; зрители, кроме `newcomer` (ему область прокрутки
    /// входит в копию экрана), получают новую область прокрутки. Возвращает `true`, если размер pty изменился
    /// и область обновлена всем зрителям.
    fn apply_size(&mut self, pty: &pty_process::WritePty<'_>, newcomer: Option<u64>) -> bool {
        let wanted = self.wanted_size();
        if wanted == self.size {
            return false;
        }
        if let Err(error) = pty.resize(PtySize::new(wanted.rows, wanted.cols)) {
            tracing::warn!(%error, "размер pty агента не изменён");
            return false;
        }
        self.size = wanted;
        self.parser.screen_mut().set_size(wanted.rows, wanted.cols);
        let mut evicted = Vec::new();
        for (id, viewer) in self.viewers.iter().filter(|(id, _)| Some(**id) != newcomer) {
            // Курсор сохраняется: установка области возвращает его в начало окна.
            let mut update = b"\x1b7".to_vec();
            update.extend(region(viewer.size, wanted));
            update.extend_from_slice(b"\x1b8");
            if viewer.output.try_send(Bytes::from(update)).is_err() {
                evicted.push(*id);
            }
        }
        for id in evicted {
            self.viewers.remove(&id);
        }
        true
    }

    /// Область прокрутки зрителю `id` по его текущему окну, когда размер pty не менялся.
    fn refresh_region(&mut self, id: u64) {
        let Some(viewer) = self.viewers.get(&id) else {
            return;
        };
        let mut update = b"\x1b7".to_vec();
        update.extend(region(viewer.size, self.size));
        update.extend_from_slice(b"\x1b8");
        if viewer.output.try_send(Bytes::from(update)).is_err() {
            self.viewers.remove(&id);
        }
    }

    fn broadcast(&mut self, chunk: &[u8]) {
        let chunk = Bytes::copy_from_slice(chunk);
        self.viewers.retain(|_, viewer| viewer.output.try_send(chunk.clone()).is_ok());
    }

    fn execute(&mut self, command: Command, pty: &pty_process::WritePty<'_>) {
        match command {
            Command::Attach { id, size, output } => {
                self.viewers.insert(id, Viewer { size, output });
                self.apply_size(pty, Some(id));
                let snapshot = self.snapshot(size);
                if let Some(viewer) = self.viewers.get(&id)
                    && viewer.output.try_send(Bytes::from(snapshot)).is_err()
                {
                    self.viewers.remove(&id);
                }
            }
            Command::Input(data) => {
                let data = strip_terminal_replies(&data);
                if self.pending.len() + data.len() <= MAX_PENDING_INPUT {
                    self.pending.extend(data);
                }
            }
            Command::Resize { id, size } => {
                if let Some(viewer) = self.viewers.get_mut(&id) {
                    viewer.size = size;
                    if !self.apply_size(pty, None) {
                        self.refresh_region(id);
                    }
                }
            }
            Command::Detach(id) => {
                self.viewers.remove(&id);
                self.apply_size(pty, None);
            }
        }
    }
}

/// Область прокрутки высотой pty для зрителя с окном выше; для остальных — сброс области.
fn region(viewer: Size, pty: Size) -> Vec<u8> {
    if viewer.rows > pty.rows {
        format!("\x1b[1;{}r", pty.rows).into_bytes()
    } else {
        b"\x1b[r".to_vec()
    }
}

/// Убирает из ввода ответы терминала зрителя: DA1 (`CSI ? … c`), kitty keyboard (`CSI ? … u`) и XTVERSION
/// (`DCS > | … ST`). Остальные байты, включая Esc и Ctrl-C, остаются; последовательность, разорванная границей
/// куска ввода, проходит как есть.
fn strip_terminal_replies(input: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(input.len());
    let mut at = 0;
    while at < input.len() {
        let rest = &input[at..];
        let length = reply_length(rest).unwrap_or(0);
        if length == 0 {
            kept.push(input[at]);
            at += 1;
        } else {
            at += length;
        }
    }
    kept
}

/// Длина ответа терминала в начале `bytes`.
fn reply_length(bytes: &[u8]) -> Option<usize> {
    if let Some(params) = bytes.strip_prefix(b"\x1b[?") {
        let digits = params.iter().take_while(|byte| byte.is_ascii_digit() || **byte == b';').count();
        return matches!(params.get(digits), Some(b'c' | b'u')).then_some(3 + digits + 1);
    }
    let body = bytes.strip_prefix(b"\x1bP>|")?;
    let end = body.windows(2).position(|pair| pair == b"\x1b\\")?;
    Some(4 + end + 2)
}

async fn run(mut pty: Pty, mut commands: mpsc::UnboundedReceiver<Command>) {
    let (mut reader, mut writer) = pty.split();
    let mut state = State::new();
    if let Err(error) = writer.resize(PtySize::new(state.size.rows, state.size.cols)) {
        tracing::warn!(%error, "начальный размер pty агента не задан");
    }
    let mut buffer = [0_u8; 8192];
    loop {
        // Ввод пишется в той же задаче, что и читается вывод: запись, которую агент не принимает, не должна
        // останавливать чтение — иначе агент, пишущий вывод, и мы, пишущие ввод, ждём друг друга.
        let front: Vec<u8> = state.pending.iter().take(buffer.len()).copied().collect();
        tokio::select! {
            read = reader.read(&mut buffer) => match read {
                // Ошибка чтения мастера — агент закрыл терминал (EIO после выхода последнего процесса).
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let chunk = &buffer[..count];
                    state.parser.process(chunk);
                    state.broadcast(chunk);
                }
            },
            written = writer.write(&front), if !front.is_empty() => match written {
                Ok(count) => {
                    state.pending.drain(..count);
                }
                Err(_) => break,
            },
            command = commands.recv() => match command {
                Some(command) => state.execute(command, &writer),
                None => break,
            },
        }
    }
}
