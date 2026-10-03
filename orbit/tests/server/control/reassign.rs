//! Смена пары session и runner: прежний runner занят, пока не подтвердит `End` своей пары.

use std::time::SystemTime;

use nerve::protocol::{Agent, AgentRun, Control as ToSession, McpCatalog, RunnerRequest, SessionMessage, SessionResponse, ToRunner};
use tokio::sync::mpsc;

use super::runner::catalog;
use super::*;
use crate::server::control::reassign::Pair;
use crate::server::control::runner as runner_control;
use crate::server::testing::unix;

/// Отвечает на `Start` или `End`, которые server шлёт runner при подключении, и ждёт, пока runner освободится.
async fn settle(stand: &Stand, channel: &mut Channel, runner: &Fingerprint) -> ToRunner {
    let (message, incoming): (ToRunner, _) = next_request(channel).await;
    respond(&incoming, &SessionResponse::Ok(())).await;
    let free = async {
        while stand.control.ending().contains(runner) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(WAIT, free)
        .await
        .expect("runner не освободился после ответа на End");
    message
}

#[tokio::test]
async fn runner_of_the_replaced_pair_is_busy_until_it_answers_end_of_that_pair() {
    let stand = Stand::start().await;
    let (old, new) = (stand.env.enroll("old"), stand.env.enroll("new"));
    let first = stand.env.reg().create_session(Agent::ClaudeCode).unwrap();
    let second = stand.env.reg().create_session(Agent::ClaudeCode).unwrap();
    let started = stand.env.reg().assign_runner(&first, &old.fingerprint).unwrap().assignment;
    let mut old_channel = runner_channel(&stand, &old).await;
    let mut new_channel = runner_channel(&stand, &new).await;
    settle(&stand, &mut old_channel, &old.fingerprint).await;
    settle(&stand, &mut new_channel, &new.fingerprint).await;

    let pair = stand.control.assign_pair(&first, &new.fingerprint).unwrap();
    assert!(
        matches!(&pair, Pair::New { old: Some(runner) } if *runner == old.fingerprint),
        "смена не назвала прежний runner"
    );
    assert!(
        matches!(stand.control.assign_pair(&second, &old.fingerprint), Err(Error::Conflict)),
        "прежний runner выдан другой сессии до ответа на End"
    );

    let announced = tokio::spawn({
        let control = Arc::clone(&stand.control);
        let (mux, ()) = stand.control.runners.get(&old.fingerprint).unwrap();
        runner_control::announce_session(control, old.fingerprint.clone(), mux)
    });
    let (end, incoming): (ToRunner, _) = next_request(&mut old_channel).await;
    assert_eq!(
        end,
        ToRunner::Session(SessionMessage::End { assignment: started }),
        "прежний runner получил End не своей пары"
    );
    assert!(
        matches!(stand.control.assign_pair(&second, &old.fingerprint), Err(Error::Conflict)),
        "прежний runner выдан другой сессии, пока не ответил на End"
    );

    respond(&incoming, &SessionResponse::Ok(())).await;
    announced.await.unwrap();
    assert!(
        matches!(stand.control.assign_pair(&second, &old.fingerprint), Ok(Pair::New { old: None })),
        "прежний runner не освободился после ответа на End"
    );
}
// Смена runner без пересоздания контейнера и остановка агента без runner. Server на `duplex` с остановленным временем.

/// Узел отвечает `reply` на каждый запрос server; запросы складываются в очередь.
fn answering<T: DeserializeOwned + Send + 'static>(mut requests: Requests, reply: Result<(), Error>) -> mpsc::UnboundedReceiver<T> {
    let (sent, received) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = requests.recv().await {
            sent.send(protocol::decode_body(incoming.body()).unwrap()).unwrap();
            respond(&incoming, &reply).await;
        }
    });
    received
}

pub(super) async fn runner_online(direct: &Direct, node: &TestNode) -> (Mux, mpsc::UnboundedReceiver<ToRunner>) {
    let now = unix(SystemTime::now());
    let (mux, requests) = direct.connect(Peer::Runner {
        fingerprint: node.fingerprint.clone(),
        issued: now,
        valid_before: now + 86_400,
    });
    // Ответы на состояние сессии идут с начала, чтобы `Host` не ждал их: ответа на него ждёт сам тест.
    let received = answering(requests, Ok(()));
    report_host(&mux).await;
    (mux, received)
}

pub(super) fn session_online(
    direct: &Direct,
    session: &Fingerprint,
    reply: Result<(), Error>,
) -> (Mux, mpsc::UnboundedReceiver<ToSession>) {
    let (mux, requests) = direct.connect(Peer::Session {
        fingerprint: session.clone(),
        ip: [127, 0, 0, 1].into(),
    });
    (mux, answering(requests, reply))
}

pub(super) async fn next<T>(received: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), received.recv())
        .await
        .expect("server ничего не прислал")
        .expect("канал узла закрыт")
}

/// Ничего нового не пришло: все задачи сначала доработали.
pub(super) async fn assert_silent<T: std::fmt::Debug>(received: &mut mpsc::UnboundedReceiver<T>, why: &str) {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(received.try_recv().is_err(), "{why}");
}

/// Сессия с парой на runner `home`; session и runner подключены, первые сообщения сняты.
pub(super) struct Connected {
    pub(super) direct: Direct,
    pub(super) session: Fingerprint,
    pub(super) home: TestNode,
    pub(super) session_mux: Mux,
    pub(super) control: mpsc::UnboundedReceiver<ToSession>,
    pub(super) home_mux: Mux,
    pub(super) home_seen: mpsc::UnboundedReceiver<ToRunner>,
}

pub(super) async fn connected_pair(session_reply: Result<(), Error>) -> Connected {
    let direct = Direct::new();
    let home = direct.env.enroll("home");
    let session = direct.env.reg().create_session(Agent::ClaudeCode).unwrap();
    direct.env.reg().assign_runner(&session, &home.fingerprint).unwrap();
    let (home_mux, mut home_seen) = runner_online(&direct, &home).await;
    next(&mut home_seen).await;
    let (session_mux, mut control) = session_online(&direct, &session, session_reply);
    next(&mut control).await;
    Connected {
        direct,
        session,
        home,
        session_mux,
        control,
        home_mux,
        home_seen,
    }
}

fn pair_runner(direct: &Direct, session: &Fingerprint) -> Option<Fingerprint> {
    direct.env.reg().session(session).unwrap().runner
}

#[tokio::test(start_paused = true)]
async fn reassigning_writes_the_pair_and_the_session_gets_the_restart_with_the_chosen_resume() {
    for resume in [true, false] {
        let mut p = connected_pair(Ok(())).await;
        let new = p.direct.env.enroll("new");
        let (_new_mux, mut new_seen) = runner_online(&p.direct, &new).await;
        next(&mut new_seen).await;

        p.direct.control.reassign(&p.session, &new.fingerprint, resume).await.unwrap();

        let ToSession::AssignRunner {
            runner: Some(assigned),
            agent,
        } = next(&mut p.control).await
        else {
            panic!("session получила не назначение");
        };
        assert_eq!(assigned.runner, new.fingerprint, "session получила не нового runner");
        assert_eq!(agent, AgentRun::Restart { resume }, "флаг resume не дошёл до session");
        assert_eq!(
            pair_runner(&p.direct, &p.session),
            Some(new.fingerprint.clone()),
            "пара не записана"
        );
        assert!(
            matches!(next(&mut new_seen).await, ToRunner::Session(SessionMessage::Start { .. })),
            "новый runner не получил Start"
        );
        assert!(
            matches!(next(&mut p.home_seen).await, ToRunner::Session(SessionMessage::End { .. })),
            "прежний runner не получил End"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn refused_check_keeps_the_pair_and_tells_the_session_nothing() {
    let mut p = connected_pair(Ok(())).await;
    let offline = p.direct.env.enroll("offline");

    let refused = p.direct.control.reassign(&p.session, &offline.fingerprint, true).await;

    assert_eq!(refused, Err(Error::Io("runner не в сети".to_owned())));
    assert_eq!(
        pair_runner(&p.direct, &p.session),
        Some(p.home.fingerprint.clone()),
        "пара изменилась"
    );
    assert_silent(&mut p.control, "session получила указание, хотя проверка отказала").await;
}

#[tokio::test(start_paused = true)]
async fn session_that_does_not_take_the_assignment_leaves_the_previous_pair_and_both_runners_untouched() {
    let mut p = connected_pair(Err(Error::Io("не принято".to_owned()))).await;
    p.direct.env.reg().set_agent_stopped(&p.session, true).unwrap();
    let new = p.direct.env.enroll("new");
    let (_new_mux, mut new_seen) = runner_online(&p.direct, &new).await;
    next(&mut new_seen).await;

    let refused = p.direct.control.reassign(&p.session, &new.fingerprint, true).await;

    assert_eq!(
        refused,
        Err(Error::Io("не принято".to_owned())),
        "отказ session не дошёл до вызывающего"
    );
    let info = p.direct.env.reg().session(&p.session).unwrap();
    assert_eq!(info.runner, Some(p.home.fingerprint.clone()), "прежняя пара не возвращена");
    assert!(info.agent_stopped, "признак остановки потерян при откате");
    assert!(
        p.direct.control.check_assignable(&new.fingerprint).is_ok(),
        "новый runner остался занятым"
    );
    assert!(
        !p.direct.control.ending().contains(&p.home.fingerprint),
        "прежний runner остался в списке завершаемых"
    );
    assert_silent(&mut new_seen, "новый runner узнал о паре, которой нет").await;
    assert_silent(&mut p.home_seen, "прежний runner получил End, хотя пара не менялась").await;
    drop((p.session_mux, p.home_mux));
}

#[tokio::test(start_paused = true)]
async fn session_outside_the_network_cannot_be_reassigned_and_nothing_changes() {
    let direct = Direct::new();
    let (home, new) = (direct.env.enroll("home"), direct.env.enroll("new"));
    let session = direct.env.reg().create_session(Agent::ClaudeCode).unwrap();
    direct.env.reg().assign_runner(&session, &home.fingerprint).unwrap();
    let (_new_mux, mut new_seen) = runner_online(&direct, &new).await;
    next(&mut new_seen).await;

    let refused = direct.control.reassign(&session, &new.fingerprint, true).await;

    assert_eq!(refused, Err(Error::Io("session не в сети".to_owned())));
    assert_eq!(
        pair_runner(&direct, &session),
        Some(home.fingerprint),
        "пара изменилась без session"
    );
}

#[tokio::test(start_paused = true)]
async fn same_runner_changes_nothing_while_the_agent_runs_and_restarts_a_stopped_agent() {
    let mut p = connected_pair(Ok(())).await;

    p.direct.control.reassign(&p.session, &p.home.fingerprint, true).await.unwrap();
    assert_silent(&mut p.control, "то же назначение перезапустило работающего агента").await;

    p.direct.env.reg().set_agent_stopped(&p.session, true).unwrap();
    p.direct.control.reassign(&p.session, &p.home.fingerprint, false).await.unwrap();

    let ToSession::AssignRunner {
        runner: Some(assigned),
        agent,
    } = next(&mut p.control).await
    else {
        panic!("session получила не назначение");
    };
    assert_eq!(assigned.runner, p.home.fingerprint);
    assert_eq!(
        agent,
        AgentRun::Restart { resume: false },
        "остановленный агент не запущен по явному назначению"
    );
    assert!(
        !p.direct.env.reg().session(&p.session).unwrap().agent_stopped,
        "признак остановки не снят"
    );
    assert_eq!(pair_runner(&p.direct, &p.session), Some(p.home.fingerprint.clone()));
}

#[tokio::test(start_paused = true)]
async fn stopped_agent_is_not_restarted_when_the_runner_is_offline() {
    let mut p = connected_pair(Ok(())).await;
    p.direct.env.reg().set_agent_stopped(&p.session, true).unwrap();
    p.home_mux.close();
    tokio::task::yield_now().await;

    let refused = p.direct.control.reassign(&p.session, &p.home.fingerprint, true).await;

    assert_eq!(refused, Err(Error::Io("runner не в сети".to_owned())));
    assert!(
        p.direct.env.reg().session(&p.session).unwrap().agent_stopped,
        "признак снят, хотя агент не запущен"
    );
    assert_silent(&mut p.control, "session получила запуск без runner").await;
}

#[tokio::test(start_paused = true)]
async fn stopped_agent_is_announced_as_stopped_on_every_connection() {
    let p = connected_pair(Ok(())).await;
    p.direct.env.reg().set_agent_stopped(&p.session, true).unwrap();
    p.session_mux.close();
    drop(p.control);

    let (_mux, mut control) = session_online(&p.direct, &p.session, Ok(()));

    let ToSession::AssignRunner { runner, agent } = next(&mut control).await else {
        panic!("session получила не назначение");
    };
    assert_eq!(runner.map(|assigned| assigned.runner), Some(p.home.fingerprint.clone()));
    assert_eq!(
        agent,
        AgentRun::Stop,
        "после переподключения session остановленный агент не остался остановленным"
    );
}

/// Сколько runner может отсутствовать без последствий.
const FIVE_MINUTES: Duration = Duration::from_mins(5);

#[tokio::test(start_paused = true)]
async fn runner_absent_for_five_minutes_stops_the_agent_and_keeps_the_pair() {
    let mut p = connected_pair(Ok(())).await;
    p.home_mux.close();

    tokio::time::sleep(Duration::from_secs(270)).await;
    assert_silent(&mut p.control, "агент остановлен раньше пяти минут").await;
    assert!(!p.direct.env.reg().session(&p.session).unwrap().agent_stopped);

    let ToSession::AssignRunner { runner, agent } = next(&mut p.control).await else {
        panic!("session получила не назначение");
    };
    assert_eq!(agent, AgentRun::Stop, "через пять минут без runner агент не остановлен");
    assert_eq!(
        runner.map(|assigned| assigned.runner),
        Some(p.home.fingerprint.clone()),
        "пара снята"
    );
    let info = p.direct.env.reg().session(&p.session).unwrap();
    assert!(info.agent_stopped, "остановка не записана в реестр");
    assert_eq!(info.runner, Some(p.home.fingerprint.clone()), "пара снята остановкой");
    assert_eq!(
        p.direct.env.reg().runner(&p.home.fingerprint).unwrap().session,
        Some(p.session.clone()),
        "runner освободился: его могут взять в другую сессию"
    );
}

#[tokio::test(start_paused = true)]
async fn outages_shorter_than_five_minutes_never_stop_the_agent_however_often_they_repeat() {
    let mut p = connected_pair(Ok(())).await;

    for _ in 0..4 {
        p.home_mux.close();
        tokio::time::sleep(Duration::from_secs(150)).await;
        let (mux, seen) = runner_online(&p.direct, &p.home).await;
        (p.home_mux, p.home_seen) = (mux, seen);
        tokio::time::sleep(Duration::from_secs(60)).await;
    }

    assert_silent(&mut p.control, "агент остановлен из-за коротких обрывов").await;
    assert!(!p.direct.env.reg().session(&p.session).unwrap().agent_stopped);
}

#[tokio::test(start_paused = true)]
async fn runner_returning_after_the_stop_does_not_restart_the_agent() {
    let mut p = connected_pair(Ok(())).await;
    p.home_mux.close();
    tokio::time::sleep(FIVE_MINUTES + Duration::from_secs(30)).await;
    let ToSession::AssignRunner { agent, .. } = next(&mut p.control).await else {
        panic!("session получила не назначение");
    };
    assert_eq!(agent, AgentRun::Stop, "агент не остановлен");

    let (_home_mux, _home_seen) = runner_online(&p.direct, &p.home).await;
    tokio::time::sleep(Duration::from_mins(20)).await;

    assert_silent(&mut p.control, "вернувшийся runner поднял агента без явного назначения").await;
    assert!(
        p.direct.env.reg().session(&p.session).unwrap().agent_stopped,
        "признак остановки снят возвращением runner"
    );
}

/// Сводка от runner дошла до session и сохранена; session ответила на неё.
async fn send_catalog(p: &mut Connected, catalog: &McpCatalog) {
    let stored: Result<(), Error> = ask(&p.home_mux, &RunnerRequest::Mcp(catalog.clone())).await.unwrap();
    assert_eq!(stored, Ok(()), "сводка не принята");
    assert_eq!(
        next(&mut p.control).await,
        ToSession::McpCatalog(catalog.clone()),
        "session получила не ту сводку"
    );
}

#[tokio::test(start_paused = true)]
async fn catalog_of_the_replaced_runner_is_dropped_and_the_session_gets_an_empty_one() {
    let mut p = connected_pair(Ok(())).await;
    send_catalog(&mut p, &catalog("old")).await;
    let new = p.direct.env.enroll("new");
    let (_new_mux, mut new_seen) = runner_online(&p.direct, &new).await;
    next(&mut new_seen).await;

    p.direct.control.reassign(&p.session, &new.fingerprint, true).await.unwrap();

    assert!(
        matches!(next(&mut p.control).await, ToSession::AssignRunner { .. }),
        "session получила не назначение"
    );
    assert_eq!(
        next(&mut p.control).await,
        ToSession::McpCatalog(McpCatalog::default()),
        "после смены пары session не получила пустую сводку"
    );
    assert!(
        p.direct.control.catalogs().is_empty(),
        "сводка прежнего runner осталась после смены пары"
    );
}

#[tokio::test(start_paused = true)]
async fn catalog_is_dropped_when_the_runner_learns_that_its_pair_ended() {
    let mut p = connected_pair(Ok(())).await;
    send_catalog(&mut p, &catalog("home")).await;
    p.direct.env.reg().release_runner(&p.session).unwrap();

    let (_again, mut seen) = runner_online(&p.direct, &p.home).await;
    assert!(
        matches!(next(&mut seen).await, ToRunner::Session(SessionMessage::End { .. })),
        "runner не получил End"
    );

    assert!(p.direct.control.catalogs().is_empty(), "сводка осталась после End");
    drop((p.session_mux, p.home_mux));
}

#[tokio::test(start_paused = true)]
async fn catalog_of_a_revoked_runner_is_dropped_and_the_session_gets_an_empty_one() {
    let mut p = connected_pair(Ok(())).await;
    send_catalog(&mut p, &catalog("home")).await;

    p.direct.control.revoke_runner(&p.home.fingerprint).unwrap();

    assert!(
        matches!(next(&mut p.control).await, ToSession::AssignRunner { runner: None, .. }),
        "session не потеряла runner"
    );
    assert_eq!(
        next(&mut p.control).await,
        ToSession::McpCatalog(McpCatalog::default()),
        "после отзыва session не получила пустую сводку"
    );
    assert!(p.direct.control.catalogs().is_empty(), "сводка отозванного runner осталась");
}

#[tokio::test(start_paused = true)]
async fn catalog_survives_the_runner_channel_loss_while_the_pair_lives() {
    let mut p = connected_pair(Ok(())).await;
    send_catalog(&mut p, &catalog("home")).await;

    p.home_mux.close();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    assert!(
        p.direct.control.catalogs().contains_key(&p.home.fingerprint),
        "обрыв канала runner снял сводку, хотя пара жива"
    );
}

#[tokio::test(start_paused = true)]
async fn catalog_of_an_offline_replaced_runner_is_dropped_without_its_end() {
    let mut p = connected_pair(Ok(())).await;
    send_catalog(&mut p, &catalog("old")).await;
    p.home_mux.close();
    while p.direct.control.runners.is_online(&p.home.fingerprint) {
        tokio::task::yield_now().await;
    }
    let new = p.direct.env.enroll("new");
    let (_new_mux, mut new_seen) = runner_online(&p.direct, &new).await;
    next(&mut new_seen).await;

    p.direct.control.reassign(&p.session, &new.fingerprint, true).await.unwrap();

    assert!(
        p.direct.control.catalogs().is_empty(),
        "сводка отключённого прежнего runner осталась после смены пары"
    );
}
