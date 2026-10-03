//! Сценарии секретов сессий.

use super::*;
use crate::server::registry::tests::{Fixture, ghost};

#[test]
fn session_secret_matches_only_the_one_issued_last() {
    let mut f = Fixture::new();
    let session = f.session();
    let other = f.session();
    assert!(
        !f.reg.session_secret_matches(&session, &SessionSecret::new("")).unwrap(),
        "сессия без секрета приняла пустой"
    );

    let first = f.reg.issue_session_secret(&session).unwrap();
    assert!(f.reg.session_secret_matches(&session, &first).unwrap(), "выданный секрет не принят");
    assert!(
        !f.reg.session_secret_matches(&other, &first).unwrap(),
        "секрет принят для другой сессии"
    );
    assert!(
        !f.reg.session_secret_matches(&session, &SessionSecret::new("guess")).unwrap(),
        "чужой секрет принят"
    );

    let second = f.reg.issue_session_secret(&session).unwrap();
    assert!(f.reg.session_secret_matches(&session, &second).unwrap());
    assert!(
        !f.reg.session_secret_matches(&session, &first).unwrap(),
        "прежний секрет остался действующим"
    );
    assert!(
        !f.reg.session_secret_matches(&ghost(), &second).unwrap(),
        "несуществующая сессия приняла секрет"
    );
    assert!(matches!(
        f.reg.issue_session_secret(&ghost()),
        Err(Error::NotFound(Entity::Session))
    ));
}

#[test]
fn removed_session_releases_its_runner() {
    let mut f = Fixture::new();
    let session = f.session();
    let node = f.node("дом");
    f.reg.assign_runner(&session, &node.fingerprint).unwrap();

    f.reg.remove_session(&session).unwrap();

    assert!(
        f.reg.runner(&node.fingerprint).unwrap().session.is_none(),
        "runner остался назначен удалённой сессии"
    );
    assert!(matches!(f.reg.remove_session(&session), Err(Error::NotFound(Entity::Session))));
}

#[test]
fn every_new_pair_gets_a_larger_assignment_number_and_the_same_runner_keeps_it() {
    let mut f = Fixture::new();
    let (first, second) = (f.session(), f.session());
    let (home, office) = (f.node("дом"), f.node("офис"));

    let started = f.reg.assign_runner(&first, &home.fingerprint).unwrap();
    let repeated = f.reg.assign_runner(&first, &home.fingerprint).unwrap();
    let moved = f.reg.assign_runner(&first, &office.fingerprint).unwrap();
    f.reg.remove_session(&first).unwrap();
    let reused = f.reg.assign_runner(&second, &home.fingerprint).unwrap();

    assert!(
        !repeated.changed && repeated.assignment == started.assignment,
        "то же назначение сменило номер"
    );
    assert_eq!(
        moved.previous,
        Some((home.fingerprint.clone(), started.assignment)),
        "смена не назвала прежнюю пару"
    );
    assert!(moved.assignment > started.assignment, "новая пара не получила больший номер");
    assert!(
        reused.assignment > moved.assignment,
        "номер повторился после удаления сессии: End старой пары коснулся бы новой"
    );
    assert_eq!(
        f.reg.runner(&office.fingerprint).unwrap().last_assignment,
        moved.assignment,
        "у освобождённого runner нет номера последней пары, End нечем подписать"
    );
}

#[test]
fn recreated_container_starts_from_waiting_whatever_the_old_state() {
    let mut f = Fixture::new();
    let session = f.session();
    f.reg.set_session_state(&session, SessionState::Success).unwrap();

    f.reg.reset_session_state(&session).unwrap();

    assert_eq!(f.reg.session(&session).unwrap().state, SessionState::Waiting);
    assert!(matches!(f.reg.reset_session_state(&ghost()), Err(Error::NotFound(Entity::Session))));
}
#[test]
fn stopped_agent_mark_survives_a_restart_and_goes_with_the_pair_not_with_the_runner() {
    let mut f = Fixture::new();
    let session = f.session();
    let (home, office) = (f.node("дом"), f.node("офис"));
    f.reg.assign_runner(&session, &home.fingerprint).unwrap();
    assert!(
        !f.reg.session(&session).unwrap().agent_stopped,
        "новая пара началась с остановленным агентом"
    );

    f.reg.set_agent_stopped(&session, true).unwrap();
    f.reopen();
    assert!(
        f.reg.session(&session).unwrap().agent_stopped,
        "признак остановки не пережил перезапуск server"
    );

    f.reg.assign_runner(&session, &home.fingerprint).unwrap();
    assert!(
        f.reg.session(&session).unwrap().agent_stopped,
        "повторное назначение того же runner сняло признак: его снимает только явный запуск"
    );

    f.reg.assign_runner(&session, &office.fingerprint).unwrap();
    assert!(
        !f.reg.session(&session).unwrap().agent_stopped,
        "новая пара унаследовала остановку прежней"
    );
    assert!(matches!(
        f.reg.set_agent_stopped(&ghost(), true),
        Err(Error::NotFound(Entity::Session))
    ));
}

#[test]
fn working_session_may_wait_again_after_a_planned_agent_stop() {
    let mut f = Fixture::new();
    let session = f.session();
    f.reg.set_session_state(&session, SessionState::Success).unwrap();

    f.reg.set_session_state(&session, SessionState::Waiting).unwrap();

    assert_eq!(f.reg.session(&session).unwrap().state, SessionState::Waiting);
}
