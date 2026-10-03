//! Остановка назначения канала данных: ответ после файловых операций и забытые итоги команд.

use std::collections::BTreeMap;
use std::fs;

use nerve::protocol::ExecRequest;

use super::*;

const FIRST: Assignment = Assignment(1);
const SECOND: Assignment = Assignment(2);

/// Чуть меньше срока, после которого потерянная сессия лишается процессов команд.
const BEFORE_LIMIT: Duration = Duration::from_secs(299);

#[tokio::test(start_paused = true)]
async fn stop_waits_for_running_file_operations() {
    let data = Arc::new(Data::new(None));
    data.start(FIRST);
    // Выполняющийся файловый запрос держит разрешение, пока не ответит.
    let operation = Arc::clone(&data.fs_permits).acquire_owned().await.unwrap();

    let stopping = Arc::clone(&data);
    let mut stopped = tokio::spawn(async move { stopping.stop().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!stopped.is_finished(), "остановка не дождалась файловой операции");
    assert_eq!(data.assignment(), None, "назначение не снято, пока операция выполняется");

    drop(operation);
    timeout(Duration::from_secs(5), &mut stopped)
        .await
        .expect("остановка не закончилась после файловой операции")
        .unwrap();
}

#[tokio::test]
async fn results_of_commands_do_not_survive_the_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = Arc::new(Workdir::open(&fs::canonicalize(dir.path()).unwrap()).unwrap());
    let executor = Arc::new(Executor::new(workdir, dir.path().join("no-such.env"), Vec::new(), None));
    let data = Data::new(None);
    executor.start(FIRST, BTreeMap::new());
    data.start(FIRST);
    let request = ExecRequest {
        id: 7,
        command: "echo a".to_owned(),
        cwd: fs::canonicalize(dir.path()).unwrap().display().to_string(),
        timeout_ms: None,
    };
    data.jobs().start(request, &executor);
    assert!(data.jobs().get(7).is_some(), "команда не записана в итоги назначения");

    data.stop().await;
    executor.stop().await;
    data.start(SECOND);
    assert!(data.jobs().get(7).is_none(), "итог команды прежнего назначения достался новому");
}

fn pid_alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next()) != Some("Z"))
}

/// Назначение с командой, оставившей фоновый процесс в своей группе; возвращает pid этого процесса.
async fn session_with_background_process() -> (tempfile::TempDir, Arc<Data>, Arc<Executor>, u32) {
    let dir = tempfile::tempdir().unwrap();
    let work = fs::canonicalize(dir.path()).unwrap();
    let executor = Arc::new(Executor::new(
        Arc::new(Workdir::open(&work).unwrap()),
        dir.path().join("no-such.env"),
        Vec::new(),
        None,
    ));
    let data = Arc::new(Data::new(None));
    executor.start(FIRST, BTreeMap::new());
    data.start(FIRST);
    let request = ExecRequest {
        id: 1,
        command: "sleep 330 >/dev/null 2>&1 & echo $! > background.pid".to_owned(),
        cwd: work.display().to_string(),
        timeout_ms: None,
    };
    let (events, mut received) = tokio::sync::mpsc::channel(16);
    executor.run(FIRST, request, events).await;
    while received.recv().await.is_some() {}
    let text = fs::read_to_string(work.join("background.pid")).unwrap();
    let pid = text.trim().parse().unwrap();
    (dir, data, executor, pid)
}

fn connected() -> Mux {
    let (stream, _peer) = tokio::io::duplex(1024);
    Mux::new(stream).0
}

fn spawn_guard(data: &Arc<Data>, executor: &Arc<Executor>) -> tokio::task::JoinHandle<Infallible> {
    let (data, executor) = (Arc::clone(data), Arc::clone(executor));
    tokio::spawn(async move { data.abandon_when_absent(&executor).await })
}

fn kill(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap_or_default();
}

#[tokio::test]
async fn session_absent_for_five_minutes_loses_background_processes() {
    let (_dir, data, executor, pid) = session_with_background_process().await;
    // Команда уже выполнена настоящим временем; дальше время тестовое.
    tokio::time::pause();
    let guard = spawn_guard(&data, &executor);
    tokio::time::sleep(BEFORE_LIMIT).await;
    assert!(pid_alive(pid), "фоновый процесс убит раньше срока");

    tokio::time::sleep(Duration::from_secs(2)).await;
    for _ in 0..500 {
        if !pid_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let alive = pid_alive(pid);
    kill(pid);
    guard.abort();
    assert!(!alive, "фоновый процесс пережил потерю сессии");
}

#[tokio::test]
async fn session_returning_in_time_keeps_background_processes() {
    let (_dir, data, executor, pid) = session_with_background_process().await;
    // Команда уже выполнена настоящим временем; дальше время тестовое.
    tokio::time::pause();
    let guard = spawn_guard(&data, &executor);
    tokio::time::sleep(BEFORE_LIMIT).await;
    data.link.send_replace(Some(connected()));
    tokio::time::sleep(ABSENT_LIMIT).await;
    data.link.send_replace(None);
    // Отсчёт после нового обрыва начался заново.
    tokio::time::sleep(BEFORE_LIMIT).await;
    let alive = pid_alive(pid);
    kill(pid);
    guard.abort();
    assert!(alive, "возвращение сессии не сбросило отсчёт: процесс убит");
}
