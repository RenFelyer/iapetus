//! Таблица переходов автомата session: каждая строка — отдельная проверка.

use super::*;

const NOW: u64 = 1_000;

fn gateway(request: RequestKind, event: GatewayEvent) -> Input {
    Input::Gateway(Observation { request, event })
}

fn main_event(event: GatewayEvent) -> Input {
    gateway(RequestKind::Main, event)
}

/// Агент запущен, runner подключён, сервис ответил 2xx.
fn working() -> Machine {
    let mut machine = Machine::default();
    for input in [
        Input::Runner { connected: true },
        Input::Agent(AgentEvent::Started),
        main_event(GatewayEvent::Accepted),
    ] {
        machine.apply(input, NOW);
    }
    assert_eq!(
        machine.verdict().state,
        SessionState::Success,
        "рабочая сессия должна быть в success"
    );
    machine
}

#[track_caller]
fn assert_state(machine: &Machine, state: SessionState) {
    assert_eq!(machine.verdict().state, state, "причина: {:?}", machine.verdict().reason);
}

#[test]
fn session_waits_for_runner_until_it_connects() {
    let mut machine = Machine::default();
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    let verdict = machine.verdict();
    assert_eq!(verdict.state, SessionState::Waiting);
    assert_eq!(verdict.reason.as_deref(), Some("runner не подключён"));
    machine.apply(Input::Runner { connected: true }, NOW);
    assert_eq!(machine.verdict().reason.as_deref(), Some("ждёт первого ответа сервиса"));
}

#[test]
fn first_accepted_response_moves_waiting_to_success() {
    let mut machine = Machine::default();
    machine.apply(Input::Runner { connected: true }, NOW);
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_state(&machine, SessionState::Waiting);
    // Проба квоты Claude Code и Antigravity — не основной ход.
    machine.apply(gateway(RequestKind::Other, GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn unauthorized_response_waits_for_a_login_and_the_next_success_clears_it() {
    let mut machine = working();
    machine.apply(main_event(GatewayEvent::Unauthorized { status: 401, code: None }), NOW);
    let verdict = machine.verdict();
    assert_eq!(
        (verdict.state, verdict.cause),
        (SessionState::Waiting, Some(Cause::LoginRequired)),
        "401 не дал ожидание входа"
    );
    assert_eq!(
        verdict.reason.as_deref(),
        Some("нужен вход: подключитесь к агенту и войдите заново (401)")
    );
    // Агент не перезапускается: пользователь вошёл в нём, и первый успешный ответ возвращает `success`.
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    let verdict = machine.verdict();
    assert_eq!(
        (verdict.state, verdict.cause),
        (SessionState::Success, None),
        "после входа сессия не ожила"
    );
}

#[test]
fn unauthorized_response_of_a_service_request_also_counts() {
    let mut machine = working();
    machine.apply(
        gateway(RequestKind::Title, GatewayEvent::Unauthorized { status: 403, code: None }),
        NOW,
    );
    assert_state(&machine, SessionState::Waiting);
}

#[test]
fn subscription_limit_warns_with_reset_until_main_turn_succeeds() {
    let mut machine = working();
    machine.apply(
        main_event(GatewayEvent::SubscriptionLimitReached {
            reset: Some(LimitReset::At(5_000)),
        }),
        NOW,
    );
    let verdict = machine.verdict();
    assert_eq!(verdict.state, SessionState::Warning);
    assert_eq!(verdict.limit_reset, Some(5_000));
    // Служебный 2xx лимит не снимает.
    machine.apply(gateway(RequestKind::Title, GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Warning);
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn relative_limit_reset_becomes_an_absolute_moment() {
    let mut machine = working();
    machine.apply(
        main_event(GatewayEvent::SubscriptionLimitReached {
            reset: Some(LimitReset::After(Duration::from_secs(60))),
        }),
        NOW,
    );
    assert_eq!(machine.verdict().limit_reset, Some(NOW + 60));
}

#[test]
fn limit_of_a_service_request_is_visible() {
    let mut machine = working();
    machine.apply(
        gateway(RequestKind::Other, GatewayEvent::SubscriptionLimitReached { reset: None }),
        NOW,
    );
    assert_state(&machine, SessionState::Warning);
}

#[test]
fn context_overflow_of_the_main_turn_warns_until_it_succeeds() {
    let mut machine = working();
    machine.apply(main_event(GatewayEvent::ContextOverflow), NOW);
    assert_state(&machine, SessionState::Warning);
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn context_overflow_of_a_service_request_is_ignored() {
    let mut machine = working();
    machine.apply(gateway(RequestKind::Title, GatewayEvent::ContextOverflow), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn connection_error_warns_from_the_first_failure_and_exhaustion_is_not_failure() {
    let mut machine = working();
    machine.apply(main_event(GatewayEvent::ConnectionError { exhausted: false }), NOW);
    assert_eq!(machine.verdict().reason.as_deref(), Some("нет соединения"));
    assert_state(&machine, SessionState::Warning);
    machine.apply(main_event(GatewayEvent::ConnectionError { exhausted: true }), NOW);
    assert_state(&machine, SessionState::Warning);
    // Соединение вернулось: любой 2xx.
    machine.apply(gateway(RequestKind::Other, GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn lost_runner_warns_until_it_is_back() {
    let mut machine = working();
    machine.apply(Input::Runner { connected: false }, NOW);
    assert_state(&machine, SessionState::Warning);
    machine.apply(Input::Runner { connected: true }, NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn agent_that_exited_by_itself_is_relaunched_without_failure_until_the_service_answers() {
    let mut machine = working();
    machine.apply(Input::Agent(AgentEvent::Relaunched), NOW);
    let verdict = machine.verdict();
    assert_eq!(
        verdict.state,
        SessionState::Waiting,
        "выход агента без указания server дал не waiting"
    );
    assert_eq!(verdict.cause, Some(Cause::AgentRestarted));
    assert_eq!(verdict.reason.as_deref(), Some("агент перезапущен"));
    // Запуск не прячет причину: до первого ответа сервиса человек видит, что агент перезапущен.
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_eq!(
        machine.verdict().cause,
        Some(Cause::AgentRestarted),
        "запуск стёр причину перезапуска"
    );
    machine.apply(gateway(RequestKind::Other, GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
    assert_eq!(machine.verdict().cause, None, "ответ сервиса не снял причину перезапуска");
}

#[test]
fn pending_restart_is_reported_until_the_agent_stops_or_exits() {
    for end in [AgentEvent::Stopped, AgentEvent::Relaunched, AgentEvent::Exited, AgentEvent::Halted] {
        let mut machine = working();
        assert!(!machine.verdict().restart_pending, "перезапуск ожидает без требования");
        machine.apply(Input::Agent(AgentEvent::RestartPending), NOW);
        assert!(machine.verdict().restart_pending, "ожидающий перезапуск не виден");
        assert_state(&machine, SessionState::Success);
        machine.apply(Input::Agent(end), NOW);
        assert!(!machine.verdict().restart_pending, "после {end:?} перезапуск всё ещё ожидает");
    }
}

#[test]
fn planned_stop_after_a_relaunch_is_not_reported_as_one() {
    let mut machine = working();
    machine.apply(Input::Agent(AgentEvent::Relaunched), NOW);
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    machine.apply(Input::Agent(AgentEvent::Stopped), NOW);
    assert_eq!(machine.verdict().cause, Some(Cause::Restarting));
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_eq!(machine.verdict().cause, Some(Cause::AwaitingFirstResponse));
}

#[test]
fn agent_exit_is_failure_and_restart_goes_to_waiting() {
    let mut machine = working();
    machine.apply(Input::Agent(AgentEvent::Exited), NOW);
    assert_state(&machine, SessionState::Failure);
    // В failure события шлюза и runner его не меняют.
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    machine.apply(Input::Runner { connected: true }, NOW);
    assert_state(&machine, SessionState::Failure);
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_state(&machine, SessionState::Waiting);
}

#[test]
fn unretried_error_of_the_main_turn_warns_until_2xx() {
    let mut machine = working();
    machine.apply(main_event(GatewayEvent::UnretriedError { status: 429, code: None }), NOW);
    assert_state(&machine, SessionState::Warning);
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Success);
}

#[test]
fn unretried_error_of_a_service_request_is_ignored() {
    let mut machine = working();
    machine.apply(
        gateway(RequestKind::Title, GatewayEvent::UnretriedError { status: 500, code: None }),
        NOW,
    );
    assert_state(&machine, SessionState::Success);
}

#[test]
fn retried_errors_and_stream_events_do_not_change_the_state() {
    let mut machine = working();
    for event in [
        GatewayEvent::ServerError { status: 503, code: None },
        GatewayEvent::RequestRateLimited,
        GatewayEvent::UpstreamBroken,
        GatewayEvent::Interrupted,
        GatewayEvent::EndTurn,
    ] {
        machine.apply(main_event(event), NOW);
        assert_state(&machine, SessionState::Success);
    }
}

#[test]
fn errors_in_waiting_stay_waiting() {
    let mut machine = Machine::default();
    machine.apply(Input::Runner { connected: true }, NOW);
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    for event in [
        GatewayEvent::SubscriptionLimitReached { reset: None },
        GatewayEvent::UnretriedError { status: 400, code: None },
        GatewayEvent::ConnectionError { exhausted: true },
        GatewayEvent::ContextOverflow,
    ] {
        machine.apply(main_event(event), NOW);
        assert_state(&machine, SessionState::Waiting);
    }
    assert_eq!(machine.verdict().reason.as_deref(), Some("нет соединения"));
}

#[test]
fn planned_agent_stop_is_waiting_for_restart_not_failure() {
    let mut machine = working();
    machine.apply(Input::Agent(AgentEvent::Stopped), NOW);
    let verdict = machine.verdict();
    assert_eq!(verdict.state, SessionState::Waiting, "плановый выход агента дал не waiting");
    assert_eq!(verdict.cause, Some(Cause::Restarting));
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_eq!(machine.verdict().cause, Some(Cause::AwaitingFirstResponse));
}

#[test]
fn agent_halted_for_lack_of_a_runner_waits_for_an_assignment_whatever_the_gateway_says() {
    let mut machine = working();
    machine.apply(Input::Agent(AgentEvent::Stopped), NOW);
    machine.apply(Input::Agent(AgentEvent::Halted), NOW);
    // Шлюз и runner живы, но агента нет: ответы шлюза и возвращение runner его не оживляют.
    machine.apply(Input::Runner { connected: false }, NOW);
    machine.apply(Input::Runner { connected: true }, NOW);
    machine.apply(main_event(GatewayEvent::Accepted), NOW);
    let verdict = machine.verdict();
    assert_eq!(verdict.state, SessionState::Waiting, "остановленный агент не в waiting");
    assert_eq!(verdict.cause, Some(Cause::RunnerRequired));
    assert_eq!(verdict.reason.as_deref(), Some("нужно назначить runner"));
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_eq!(
        machine.verdict().cause,
        Some(Cause::AwaitingFirstResponse),
        "запуск не снял ожидание назначения"
    );
}

#[tokio::test]
async fn agent_exit_is_reported_as_planned_only_when_it_was_planned() {
    let (events, mut received) = mpsc::unbounded_channel();
    let (running, _) = watch::channel(true);
    for (planned, expected) in [(true, AgentEvent::Stopped), (false, AgentEvent::Exited)] {
        let flag = AtomicBool::new(planned);
        let report = report_agent_exit(async { Ok(()) }, &events, &running, &flag);
        tokio::select! {
            never = report => match never {},
            event = received.recv() => assert_eq!(event, Some(expected)),
        }
    }
}

#[test]
fn rejected_credentials_and_first_wait_have_distinct_causes() {
    let mut machine = Machine::default();
    machine.apply(Input::Runner { connected: true }, NOW);
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    assert_eq!(machine.verdict().cause, Some(Cause::AwaitingFirstResponse));
    machine.apply(main_event(GatewayEvent::Unauthorized { status: 401, code: None }), NOW);
    assert_eq!(machine.verdict().cause, Some(Cause::LoginRequired));
}

#[test]
fn accepted_response_before_the_first_runner_keeps_waiting() {
    let mut machine = Machine::default();
    machine.apply(Input::Agent(AgentEvent::Started), NOW);
    machine.apply(gateway(RequestKind::Other, GatewayEvent::Accepted), NOW);
    assert_state(&machine, SessionState::Waiting);
    machine.apply(Input::Runner { connected: true }, NOW);
    assert_state(&machine, SessionState::Success);
}

#[tokio::test]
async fn agent_exit_lowers_the_running_flag_and_ends_in_failure() {
    let (events, mut received) = mpsc::unbounded_channel();
    let (running, running_rx) = watch::channel(true);
    let planned = AtomicBool::new(false);
    let mut machine = working();
    let report = report_agent_exit(async { Ok(()) }, &events, &running, &planned);
    tokio::select! {
        never = report => match never {},
        event = received.recv() => machine.apply(Input::Agent(event.unwrap()), NOW),
    }
    assert!(!*running_rx.borrow(), "флаг агента остался поднятым после его выхода");
    assert_state(&machine, SessionState::Failure);
}

#[tokio::test]
async fn main_turn_is_open_from_the_first_accepted_request_until_it_ends() {
    let (events, _) = broadcast::channel(16);
    let (open, mut turn) = watch::channel(false);
    tokio::spawn(track_turn(events.subscribe(), open));
    let send = |request, event| {
        events.send(Observation { request, event }).unwrap();
    };

    send(RequestKind::Main, GatewayEvent::Accepted);
    turn.wait_for(|open| *open).await.unwrap();
    send(RequestKind::Compaction, GatewayEvent::EndTurn);
    send(
        RequestKind::Main,
        GatewayEvent::ToolUse {
            name: "Read".to_owned(),
            input: serde_json::Value::Null,
            path: None,
        },
    );
    send(RequestKind::Subagent { id: None, kind: None }, GatewayEvent::EndTurn);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(*turn.borrow(), "ход закрыт чужим EndTurn или вызовом инструмента");

    send(RequestKind::Main, GatewayEvent::EndTurn);
    turn.wait_for(|open| !*open).await.unwrap();
}
fn window(used: f32) -> UsageWindow {
    UsageWindow {
        duration: Some(18_000),
        scope: None,
        used,
        resets_at: None,
    }
}

#[test]
fn usage_windows_are_replaced_by_each_report_and_survive_an_agent_restart() {
    let mut machine = working();
    machine.apply(main_event(GatewayEvent::Limits(vec![window(0.2)])), 0);
    assert_eq!(machine.verdict().limits, [window(0.2)], "окна не дошли до вердикта");

    machine.apply(main_event(GatewayEvent::Limits(vec![window(0.6)])), 0);
    assert_eq!(machine.verdict().limits, [window(0.6)], "новый отчёт не заменил прежний");

    machine.apply(Input::Agent(AgentEvent::Started), 0);
    assert_eq!(machine.verdict().limits, [window(0.6)], "перезапуск агента стёр окна");
}
