//! Терминал на настоящем pty: агент — маленький `sh`-сценарий, зрители — каналы вывода без SSH.

use std::time::Duration;

use super::*;

/// Сколько ждать того, что должно случиться сразу: дольше — зависание.
const HANG: Duration = Duration::from_secs(20);

/// Запускает `script` в pty и возвращает терминал с процессом (он живёт, пока жив `Child`).
fn start(script: &str) -> (Terminal, tokio::process::Child) {
    let (pty, pts) = pty_process::open().unwrap();
    let child = pty_process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .kill_on_drop(true)
        .spawn(pts)
        .unwrap();
    (Terminal::start(pty), child)
}

/// Читает вывод, пока в нём не появится `needle`; возвращает всё прочитанное.
async fn read_until(output: &mut mpsc::Receiver<Bytes>, needle: &str) -> Vec<u8> {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(HANG, async {
        while !String::from_utf8_lossy(&seen).contains(needle) {
            seen.extend(output.recv().await.expect("вывод закрылся раньше ожидаемого"));
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "зависание: в выводе нет {needle:?}, пришло {:?}",
        String::from_utf8_lossy(&seen)
    );
    seen
}

fn screen_of(rows: u16, cols: u16, stream: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(stream);
    parser
}

const DRAW: &str = r"printf '\033]0;agent\007\033[?1004h\033[?2031h\033[1;31mhello\033[0m\r\nworld'; sleep 60";

#[tokio::test]
async fn new_viewer_gets_a_copy_of_the_screen_with_the_modes_vt100_drops() {
    let (terminal, _agent) = start(DRAW);
    let size = Size { rows: 24, cols: 80 };
    let mut first = terminal.attach(1, size).unwrap();
    let live = read_until(&mut first, "world").await;

    let mut second = terminal.attach(2, size).unwrap();
    let snapshot = second.recv().await.unwrap();

    let expected = screen_of(24, 80, &live);
    let copy = screen_of(24, 80, &snapshot);
    assert_eq!(
        copy.screen().contents_formatted(),
        expected.screen().contents_formatted(),
        "копия экрана новому зрителю не совпала с экраном, который видит первый"
    );
    assert_eq!(
        copy.screen().cursor_position(),
        expected.screen().cursor_position(),
        "курсор в копии не там"
    );
    let text = String::from_utf8_lossy(&snapshot);
    assert!(text.contains("\x1b]0;agent\x07"), "в копии нет заголовка окна (OSC 0)");
    assert!(text.contains("\x1b[?1004h"), "в копии нет событий фокуса (?1004h)");
    assert!(text.contains("\x1b[?2031h"), "в копии нет уведомлений о цветовой схеме (?2031h)");
}

#[tokio::test]
async fn pty_takes_the_smallest_window_and_taller_viewers_get_a_scroll_region() {
    // Сценарий сам опрашивает размер: обработчик SIGWINCH терял сигнал, пришедший, пока он ещё печатал прошлый.
    let (terminal, _agent) = start(r"echo ready; while :; do stty size; sleep 0.1; done");
    let mut tall = terminal.attach(1, Size { rows: 30, cols: 100 }).unwrap();
    let first = tall.recv().await.unwrap();
    read_until(&mut tall, "ready").await;
    assert!(
        first.starts_with(b"\x1b[r"),
        "у зрителя с окном, равным pty, область прокрутки не сброшена"
    );

    let _small = terminal.attach(2, Size { rows: 20, cols: 90 }).unwrap();
    let update = read_until(&mut tall, "\x1b[1;20r").await;
    assert!(!update.is_empty(), "высокий зритель не получил область прокрутки высотой pty");
    read_until(&mut tall, "20 90").await;

    let mut taller = terminal.attach(3, Size { rows: 40, cols: 90 }).unwrap();
    let snapshot = taller.recv().await.unwrap();
    assert!(
        snapshot.starts_with(b"\x1b[1;20r"),
        "копия экрана зрителю выше pty не начинается с области прокрутки"
    );

    // Малое окно ушло: pty расширяется до наименьшего из оставшихся.
    terminal.detach(2);
    read_until(&mut tall, "30 90").await;
}

#[tokio::test]
async fn input_from_any_viewer_reaches_the_agent_and_output_reaches_all() {
    let (terminal, _agent) = start("cat");
    let size = Size { rows: 24, cols: 80 };
    let mut first = terminal.attach(1, size).unwrap();
    let mut second = terminal.attach(2, size).unwrap();
    terminal.input(Bytes::from_static(b"ping\n"));
    read_until(&mut first, "ping").await;
    read_until(&mut second, "ping").await;
}

#[tokio::test]
async fn viewer_leaving_does_not_stop_the_agent() {
    let (terminal, _agent) = start("cat");
    let size = Size { rows: 24, cols: 80 };
    drop(terminal.attach(1, size).unwrap());
    terminal.detach(1);
    let mut again = terminal.attach(2, size).unwrap();
    terminal.input(Bytes::from_static(b"alive\n"));
    read_until(&mut again, "alive").await;
}

#[tokio::test]
async fn viewer_growing_taller_than_the_pty_gets_a_scroll_region_without_a_pty_resize() {
    let (terminal, _agent) = start("cat");
    let mut tall = terminal.attach(1, Size { rows: 20, cols: 80 }).unwrap();
    let _other = terminal.attach(2, Size { rows: 20, cols: 80 }).unwrap();
    assert!(
        tall.recv().await.unwrap().starts_with(b"\x1b[r"),
        "у зрителя с окном, равным pty, область прокрутки не сброшена"
    );

    terminal.resize(1, Size { rows: 30, cols: 80 });
    read_until(&mut tall, "\x1b[1;20r").await;
}

#[tokio::test]
async fn terminal_replies_do_not_reach_the_agent_and_other_input_does() {
    let (terminal, _agent) = start("stty raw -echo; echo ready; cat");
    let mut viewer = terminal.attach(1, Size { rows: 24, cols: 80 }).unwrap();
    read_until(&mut viewer, "ready").await;

    terminal.input(Bytes::from_static(
        b"a\x1b[?62;4;22cb\x1bP>|xterm(388)\x1b\\c\x1b[?0ud\x03\x1b[Ae\n",
    ));
    let seen = read_until(&mut viewer, "e\n").await;
    let expected = b"abcd\x03\x1b[Ae\n";
    assert_eq!(
        String::from_utf8_lossy(&seen),
        String::from_utf8_lossy(expected),
        "агент получил ответы терминала или потерял остальной ввод"
    );
}
