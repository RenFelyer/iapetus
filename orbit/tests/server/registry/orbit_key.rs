//! Сценарии `orbit_key`.

use super::*;
use crate::server::registry::tests::Fixture;

#[test]
fn reissued_orbit_key_replaces_the_previous_one_and_survives_a_restart() {
    let mut f = Fixture::new();
    assert!(
        matches!(f.reg.check_orbit_key(&OrbitKey::new("")), Err(Error::BadCredentials)),
        "без выданного ключа принят пустой"
    );
    let old = f.reg.issue_orbit_key(10).unwrap();
    let new = f.reg.issue_orbit_key(20).unwrap();

    assert!(
        matches!(f.reg.check_orbit_key(&old), Err(Error::BadCredentials)),
        "прежний orbit_key действует после выдачи нового"
    );
    f.reg.check_orbit_key(&new).expect("новый orbit_key не действует");
    f.reopen();
    f.reg.check_orbit_key(&new).expect("orbit_key не пережил перезапуск");
    assert_eq!(*f.reg.orbit_key_issued().borrow(), Some(20), "время выдачи не пережило перезапуск");
}

#[test]
fn new_orbit_key_comes_at_least_two_seconds_after_the_previous_one() {
    // Входы по ключу помечаются секундой после его выдачи; отзыв по новому ключу должен накрыть и их.
    let mut f = Fixture::new();
    f.reg.issue_orbit_key(100).unwrap();
    f.reg.issue_orbit_key(100).unwrap();
    assert_eq!(*f.reg.orbit_key_issued().borrow(), Some(102));
    f.reg.issue_orbit_key(500).unwrap();
    assert_eq!(*f.reg.orbit_key_issued().borrow(), Some(500), "поздний ключ сдвинут без нужды");
}

#[test]
fn first_orbit_key_is_issued_once_and_only_into_an_empty_registry() {
    let mut f = Fixture::new();
    let first = f.reg.issue_first_orbit_key(1).unwrap().expect("пустой реестр без первого ключа");
    assert!(f.reg.issue_first_orbit_key(2).unwrap().is_none(), "первый ключ выдан дважды");
    f.reg.check_orbit_key(&first).expect("повторная попытка заменила первый ключ");

    let mut g = Fixture::new();
    g.node("дом");
    assert!(
        g.reg.issue_first_orbit_key(1).unwrap().is_none(),
        "реестр с runner считается пустым"
    );
}
