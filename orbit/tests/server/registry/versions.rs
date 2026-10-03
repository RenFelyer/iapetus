use crate::server::registry::tests::Fixture;

#[test]
fn first_seen_time_of_a_version_is_kept_across_calls_and_reopening() {
    let mut f = Fixture::new();
    assert_eq!(f.reg.version_first_seen("claude", "2.1.286", 1000).unwrap(), (1000, true));
    assert_eq!(
        f.reg.version_first_seen("claude", "2.1.286", 5000).unwrap(),
        (1000, false),
        "повторное чтение канала сдвинуло время первого появления"
    );

    f.reopen();

    assert_eq!(
        f.reg.version_first_seen("claude", "2.1.286", 9000).unwrap(),
        (1000, false),
        "время первого появления не пережило перезапуск"
    );
    assert_eq!(
        f.reg.version_first_seen("claude", "2.1.287", 9000).unwrap(),
        (9000, true),
        "у новой версии нет своего отсчёта"
    );
}
