//! Выбор runner в TUI: что выбирается, что нет, и какие запросы уходят server.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nerve::mux::Mux;
use nerve::protocol::{self, Error, RunnersOp, SessionsOp, WorkerRequest};
use russh::keys::PrivateKey;
use russh::keys::ssh_key::Algorithm;

use super::*;

fn fingerprint(name: &str) -> Fingerprint {
    name.parse().unwrap()
}

fn runner(name: &str, presence: RunnerPresence) -> RunnerInfo {
    RunnerInfo {
        fingerprint: fingerprint(name),
        name: name.to_owned(),
        presence,
        temporary: false,
        parent: None,
    }
}

fn session(name: &str) -> SessionInfo {
    SessionInfo {
        fingerprint: fingerprint(name),
        agent: Agent::ClaudeCode,
        state: SessionState::Success,
        reason: None,
        cause: None,
        limit_reset: None,
        runner: None,
        agent_running: true,
        limits: Vec::new(),
        restart_pending: false,
    }
}

fn app() -> App {
    App {
        sessions: vec![session("session-1")],
        runners: vec![
            runner("runner-free", RunnerPresence::Free),
            runner("runner-busy", RunnerPresence::Busy),
            runner("runner-off", RunnerPresence::Offline),
        ],
        nodes: Vec::new(),
        env: Vec::new(),
        section: None,
        manage: ListState::default(),
        table: TableState::default().with_selected(Some(0)),
        screen: Screen::List,
        status: String::new(),
    }
}

/// Control с настоящим каналом `Mux` до поддельного server, который запоминает запросы; список сессий у него пуст.
fn control() -> (Control, Arc<Mutex<Vec<WorkerRequest>>>) {
    control_listing(Vec::new())
}

/// То же, но server отвечает на список сессий `listed`.
fn control_listing(listed: Vec<SessionInfo>) -> (Control, Arc<Mutex<Vec<WorkerRequest>>>) {
    let ca = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .to_openssh()
        .unwrap();
    let control = Control::new("orbit.example:2222", &ca, "secret", "alice", PathBuf::from("/nonexistent/.env")).unwrap();
    let (near, far) = tokio::io::duplex(1 << 16);
    let (mux, client_requests) = Mux::new(near);
    let (server, mut requests) = Mux::new(far);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let _keep = (client_requests, server);
        while let Some(incoming) = requests.recv().await {
            let request: WorkerRequest = protocol::decode_body(incoming.body()).unwrap();
            let reply = match &request {
                WorkerRequest::Sessions(SessionsOp::List) => protocol::encode(&Ok::<Vec<SessionInfo>, Error>(listed.clone())),
                WorkerRequest::Runners(_) => protocol::encode(&Ok::<Vec<RunnerInfo>, Error>(Vec::new())),
                _ => protocol::encode(&Ok::<(), Error>(())),
            };
            log.lock().unwrap().push(request);
            incoming.respond(reply.unwrap().into(), true).await.unwrap();
        }
    });
    control.channel.send_replace(Some(mux));
    (control, seen)
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Нажимает `Down` до пункта `label` и Enter.
async fn choose(app: &mut App, control: &Control, label: &str) {
    let Screen::Pick(pick) = &app.screen else {
        panic!("открыт не выбор: {:?}", app.screen);
    };
    let at = pick
        .labels
        .iter()
        .position(|shown| shown.contains(label))
        .unwrap_or_else(|| panic!("пункта {label:?} нет в {:?}", pick.labels));
    for _ in 0..at {
        app.handle_key(control, press(KeyCode::Down)).await;
    }
    app.handle_key(control, press(KeyCode::Enter)).await;
}

fn labels(app: &App) -> &[String] {
    let Screen::Pick(pick) = &app.screen else {
        panic!("открыт не выбор: {:?}", app.screen);
    };
    &pick.labels
}

#[tokio::test]
async fn runner_picker_shows_icon_name_and_fingerprint() {
    let (control, _) = control();
    let mut app = app();
    app.handle_key(&control, press(KeyCode::Char('R'))).await;
    let shown = labels(&app);
    assert_eq!(shown[0], "🔓 runner-free runner-free", "свободный: значок, имя, fingerprint");
    assert!(
        shown[1].starts_with("🔒 runner-busy"),
        "занятый: значок 🔒, получено {:?}",
        shown[1]
    );
    assert!(
        shown[2].starts_with("📴 runner-off"),
        "не в сети: значок 📴, получено {:?}",
        shown[2]
    );
}

/// Запросы `Assign`, которые ушли server.
fn assigns(seen: &Mutex<Vec<WorkerRequest>>) -> Vec<WorkerRequest> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|request| matches!(request, WorkerRequest::Runners(RunnersOp::Assign { .. })))
        .cloned()
        .collect()
}

#[tokio::test]
async fn changing_the_runner_asks_whether_to_continue_the_conversation_and_sends_the_answer() {
    for (answer, resume) in [("Да", true), ("Нет", false)] {
        let (control, seen) = control();
        let mut app = app();
        app.handle_key(&control, press(KeyCode::Char('R'))).await;
        choose(&mut app, &control, "runner-free").await;

        assert!(seen.lock().unwrap().is_empty(), "Assign ушёл до ответа на вопрос о разговоре");
        assert_eq!(labels(&app).len(), 2, "после выбора runner нет вопроса «продолжить разговор?»");
        choose(&mut app, &control, answer).await;

        assert_eq!(
            assigns(&seen),
            [WorkerRequest::Runners(RunnersOp::Assign {
                session: fingerprint("session-1"),
                runner: fingerprint("runner-free"),
                resume,
            })],
            "Assign ушёл не один раз с сессией, выбранным runner и ответом {answer:?}"
        );
        assert!(matches!(app.screen, Screen::List), "после смены экран — список");
    }
}

#[tokio::test]
async fn pressing_a_asks_the_server_to_restart_the_selected_agent_and_says_what_happens() {
    let mut waiting = session("session-1");
    waiting.restart_pending = true;
    for (listed, said) in [(session("session-1"), "в конце хода"), (waiting, "сразу")] {
        let (control, seen) = control_listing(vec![listed.clone()]);
        let mut app = app();
        app.sessions = vec![listed];

        app.handle_key(&control, press(KeyCode::Char('a'))).await;

        let restarts: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|request| matches!(request, WorkerRequest::Sessions(SessionsOp::RestartAgent(_))))
            .cloned()
            .collect();
        assert_eq!(
            restarts,
            [WorkerRequest::Sessions(SessionsOp::RestartAgent(fingerprint("session-1")))],
            "перезапуск агента ушёл не один раз и не для выбранной сессии"
        );
        assert!(app.status.contains(said), "строка состояния не говорит {said:?}: {:?}", app.status);
    }
}

#[test]
fn pending_restart_is_visible_in_the_reason_column() {
    let mut waiting = session("session-1");
    waiting.reason = Some("агент работает".to_owned());
    assert_eq!(reason_text(&waiting), "агент работает");
    waiting.restart_pending = true;
    assert!(
        reason_text(&waiting).contains("перезапуск ждёт конца хода"),
        "ожидающий перезапуск не виден в списке: {:?}",
        reason_text(&waiting)
    );
}

#[tokio::test]
async fn escaping_the_resume_question_assigns_nothing() {
    let (control, seen) = control();
    let mut app = app();
    app.handle_key(&control, press(KeyCode::Char('R'))).await;
    choose(&mut app, &control, "runner-free").await;

    app.handle_key(&control, press(KeyCode::Esc)).await;

    assert!(assigns(&seen).is_empty(), "отмена вопроса назначила runner");
    assert!(matches!(app.screen, Screen::List));
}

#[tokio::test]
async fn own_runner_of_a_stopped_agent_is_selectable_though_busy() {
    let (control, seen) = control();
    let mut app = app();
    app.sessions[0].runner = Some(fingerprint("runner-busy"));
    app.sessions[0].cause = Some(Cause::RunnerRequired);
    app.handle_key(&control, press(KeyCode::Char('R'))).await;

    choose(&mut app, &control, "runner-busy").await;
    choose(&mut app, &control, "Да").await;

    assert_eq!(
        assigns(&seen),
        [WorkerRequest::Runners(RunnersOp::Assign {
            session: fingerprint("session-1"),
            runner: fingerprint("runner-busy"),
            resume: true,
        })],
        "остановленному агенту нельзя назначить прежний runner"
    );
}

#[tokio::test]
async fn runner_of_a_working_agent_is_not_selectable() {
    let (control, seen) = control();
    let mut app = app();
    app.sessions[0].runner = Some(fingerprint("runner-busy"));
    app.handle_key(&control, press(KeyCode::Char('R'))).await;

    choose(&mut app, &control, "runner-busy").await;

    assert!(matches!(app.screen, Screen::Pick(_)), "выбор закрыт");
    assert!(seen.lock().unwrap().is_empty(), "работающей сессии назначен её же занятый runner");
}

#[tokio::test]
async fn busy_and_offline_runners_are_not_assignable() {
    let (control, seen) = control();
    for name in ["runner-busy", "runner-off"] {
        let mut app = app();
        app.handle_key(&control, press(KeyCode::Char('R'))).await;
        choose(&mut app, &control, name).await;
        assert!(matches!(app.screen, Screen::Pick(_)), "{name}: выбор остаётся открытым");
        assert!(!app.status.is_empty(), "{name}: причина отказа в строке состояния");
    }
    assert!(seen.lock().unwrap().is_empty(), "на занятый и недоступный runner запрос не уходит");
}

#[tokio::test]
async fn new_connection_accepts_only_free_runner() {
    let (control, _) = control();
    for name in ["runner-busy", "runner-off"] {
        let mut app = app();
        app.handle_key(&control, press(KeyCode::Char('n'))).await;
        choose(&mut app, &control, "Claude Code").await;
        choose(&mut app, &control, name).await;
        assert!(matches!(app.screen, Screen::Pick(_)), "{name}: мастер остаётся на выборе runner");
    }
    let mut app = app();
    app.handle_key(&control, press(KeyCode::Char('n'))).await;
    choose(&mut app, &control, "Claude Code").await;
    let outcome = {
        let Screen::Pick(_) = &app.screen else {
            panic!("нет выбора runner")
        };
        app.handle_key(&control, press(KeyCode::Enter)).await
    };
    assert!(
        matches!(outcome, Outcome::Start { runner, .. } if runner == fingerprint("runner-free")),
        "свободный runner запускает сессию"
    );
}

#[tokio::test]
async fn clone_is_refused_on_offline_runner_only() {
    let (control, _) = control();
    let mut app = app();
    app.handle_key(&control, press(KeyCode::Char('c'))).await;
    choose(&mut app, &control, "runner-off").await;
    assert!(matches!(app.screen, Screen::Pick(_)), "клон на 📴 отклонён");

    for name in ["runner-free", "runner-busy"] {
        let mut app = app_with_clone_picker(&control).await;
        choose(&mut app, &control, name).await;
        let Screen::Input(input) = &app.screen else {
            panic!("{name}: нет строки ввода каталога клона");
        };
        assert!(matches!(&input.typing, Typing::CloneDir { runner } if *runner == fingerprint(name)));
    }
}

async fn app_with_clone_picker(control: &Control) -> App {
    let mut app = app();
    app.handle_key(control, press(KeyCode::Char('c'))).await;
    app
}

/// Новое подключение — два шага, агент и runner: ключа агента нет, пользователь входит в самом агенте.
#[tokio::test]
async fn new_connection_goes_from_the_agent_straight_to_the_runner_and_starts_without_a_key() {
    let (control, seen) = control();
    let mut app = app();
    app.handle_key(&control, press(KeyCode::Char('n'))).await;
    assert_eq!(labels(&app), ["Claude Code", "Codex", "Antigravity"], "первый шаг мастера — агент");

    choose(&mut app, &control, "Codex").await;
    let Screen::Pick(pick) = &app.screen else {
        panic!("после агента нет выбора runner");
    };
    assert!(
        matches!(pick.purpose, Purpose::NewRunner { agent: Agent::Codex }),
        "второй шаг — runner"
    );

    let outcome = app.handle_key(&control, press(KeyCode::Enter)).await;
    assert!(
        matches!(&outcome, Outcome::Start { agent: Agent::Codex, runner } if *runner == fingerprint("runner-free")),
        "мастер не дал запуск с агентом и runner: {outcome:?}"
    );
    assert!(seen.lock().unwrap().is_empty(), "до запуска на server ушли лишние запросы");
}

/// Разделов ключей агентов нет: клавиши `K` и `s` ничего не открывают.
#[tokio::test]
async fn agent_key_screens_are_gone() {
    let (control, seen) = control();
    for code in ['K', 's'] {
        let mut app = app();
        app.handle_key(&control, press(KeyCode::Char(code))).await;
        assert!(matches!(app.screen, Screen::List), "клавиша {code} открыла экран: {:?}", app.screen);
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn start_refusal_reason_is_shown_in_the_status_line() {
    let mut app = app();
    let started = app.report_start(Ok(Some(SessionEvent::Refused("runner занят".to_owned()))));
    assert_eq!(started, None, "отказанная сессия считается запущенной");
    assert!(
        app.status.contains("runner занят"),
        "причины нет в строке состояния: {:?}",
        app.status
    );
}

fn window(duration: Option<u64>, scope: Option<&str>, used: f32) -> UsageWindow {
    UsageWindow {
        duration,
        scope: scope.map(str::to_owned),
        used,
        resets_at: None,
    }
}

#[test]
fn limits_show_two_fullest_windows_with_duration_labels() {
    let windows = [
        window(Some(5 * 3_600), None, 0.2),
        window(Some(7 * 86_400), None, 0.9),
        window(None, Some("opus"), 0.5),
    ];
    assert_eq!(limits_text(&windows), "7д 90% · opus 50%", "показаны не два самых заполненных окна");
    assert_eq!(limits_text(&[]), "—", "пустой список лимитов не показан прочерком");
}

fn stopped(state: SessionState, cause: Cause, reason: &str) -> SessionInfo {
    SessionInfo {
        state,
        cause: Some(cause),
        reason: Some(reason.to_owned()),
        agent_running: false,
        ..session("session-1")
    }
}

#[tokio::test]
async fn entering_a_session_without_an_agent_says_the_state_and_what_to_do() {
    let cases = [
        (
            stopped(SessionState::Failure, Cause::AgentExited, "агент завершается сразу после запуска"),
            [
                "сбой",
                "агент завершается сразу после запуска",
                "f — перезапустить",
                "e — завершить",
            ],
        ),
        (
            stopped(SessionState::Waiting, Cause::RunnerRequired, "нужно назначить runner"),
            ["ждёт входа", "нужно назначить runner", "R — назначить runner", "e — завершить"],
        ),
    ];
    for (info, expected) in cases {
        let (control, _) = control_listing(vec![info.clone()]);
        let mut app = app();
        app.sessions = vec![info];

        let outcome = app.handle_key(&control, press(KeyCode::Enter)).await;

        assert!(matches!(outcome, Outcome::Stay), "вход открылся в сессию без агента");
        for part in expected {
            assert!(app.status.contains(part), "в сообщении {:?} нет {part:?}", app.status);
        }
        assert!(!app.status.contains("вход не удался"), "вместо понятного сообщения ошибка входа");
    }
}

#[tokio::test]
async fn entering_re_reads_the_list_so_a_restarted_agent_is_let_in() {
    let (control, _) = control_listing(vec![session("session-1")]);
    let mut app = app();
    app.sessions = vec![stopped(SessionState::Waiting, Cause::AgentRestarted, "агент перезапущен")];

    let outcome = app.handle_key(&control, press(KeyCode::Enter)).await;

    assert!(
        matches!(&outcome, Outcome::Enter(session) if *session == fingerprint("session-1")),
        "устаревший список не дал войти в уже запущенного агента: {outcome:?}"
    );
}

#[tokio::test]
async fn entering_a_session_with_an_agent_opens_it_without_a_message() {
    let (control, _) = control();
    let mut app = app();

    let outcome = app.handle_key(&control, press(KeyCode::Enter)).await;

    assert!(matches!(outcome, Outcome::Enter(_)), "вход в работающего агента не открылся");
    assert!(app.status.is_empty(), "лишнее сообщение при входе: {:?}", app.status);
}

#[test]
fn closed_agent_channel_is_reported_as_restart_stop_or_end_of_session() {
    let mut app = app();
    let target = fingerprint("session-1");
    assert_eq!(app.agent_closed_message(&target), "агент перезапущен: Enter — войти снова");
    app.sessions = vec![stopped(SessionState::Failure, Cause::AgentExited, "цикл")];
    assert!(app.agent_closed_message(&target).starts_with("агент не запущен: f — перезапустить"));
    app.sessions.clear();
    assert_eq!(app.agent_closed_message(&target), "сессия завершена");
}
