//! Слой окружения server: снимок и изменения по подписке `Env`, ожидание свежего снимка.

use std::sync::Arc;

use nerve::mux::{Incoming, Requests};
use nerve::protocol::{self, EnvEntry};
use tokio::io::duplex;

use super::*;

fn fingerprint(text: &str) -> Fingerprint {
    text.parse().unwrap()
}

fn entry(key: &str, value: &str) -> EnvEntry {
    EnvEntry {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

async fn send(subscription: &Incoming, event: &EnvEvent, last: bool) {
    subscription.respond(protocol::encode(event).unwrap().into(), last).await.unwrap();
}

/// Runner `runner-a` (клон `parent`) подписался на `Env`; возвращает env, подписку со стороны server и канал,
/// который держит соединение открытым.
async fn subscribed() -> (Arc<ServerEnv>, Incoming, (Mux, Mux, Requests)) {
    let (a, b) = duplex(1 << 16);
    let (server, mut server_requests) = Mux::new(a);
    let (runner, runner_requests) = Mux::new(b);
    let env = Arc::new(ServerEnv::new(fingerprint("runner-a"), Some(fingerprint("parent"))));
    let followed = Arc::clone(&env);
    let mux = runner.clone();
    tokio::spawn(async move { followed.follow(&mux).await });
    let subscription = server_requests.recv().await.unwrap();
    let request: RunnerRequest = protocol::decode_body(subscription.body()).unwrap();
    assert_eq!(request, RunnerRequest::Env);
    // Приёмник запросов runner должен жить: иначе поток закроется.
    (env, subscription, (server, runner, runner_requests))
}

#[tokio::test]
async fn layers_are_all_then_parent_then_own_and_foreign_keys_are_ignored() {
    let (env, subscription, _channel) = subscribed().await;
    let snapshot = vec![
        entry("all.A", "shared"),
        entry("all.B", "shared"),
        entry("parent.B", "parent"),
        entry("parent.C", "parent"),
        entry("runner-a.C", "own"),
        entry("other.D", "foreign"),
    ];
    send(&subscription, &EnvEvent::Snapshot(snapshot), false).await;

    let got = env.vars(Duration::from_secs(10)).await.unwrap();

    assert_eq!(
        got,
        vars(&[("A", "shared"), ("B", "parent"), ("C", "own")]),
        "слои окружения слиты не в порядке all, родитель, свой, или попал чужой слой"
    );
}

#[tokio::test]
async fn changes_after_the_snapshot_are_applied() {
    let (env, subscription, _channel) = subscribed().await;
    send(
        &subscription,
        &EnvEvent::Snapshot(vec![entry("all.A", "1"), entry("all.B", "1")]),
        false,
    )
    .await;
    send(
        &subscription,
        &EnvEvent::Change(EnvChange::Set {
            key: "all.A".to_owned(),
            value: "2".to_owned(),
        }),
        false,
    )
    .await;
    send(
        &subscription,
        &EnvEvent::Change(EnvChange::Delete { key: "all.B".to_owned() }),
        false,
    )
    .await;

    // Изменения идут тем же потоком после снимка: ждём, пока дойдёт последнее.
    for _ in 0..100 {
        if env.vars(Duration::from_secs(10)).await.unwrap() == vars(&[("A", "2")]) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "изменения Set и Delete не применены к снимку: {:?}",
        env.vars(Duration::from_secs(10)).await
    );
}

#[tokio::test(start_paused = true)]
async fn without_a_snapshot_vars_are_unavailable_after_the_wait() {
    let env = ServerEnv::new(fingerprint("runner-a"), None);

    let error = env.vars(Duration::from_secs(10)).await.unwrap_err();

    assert_eq!(error, "переменные server недоступны");
}

#[tokio::test]
async fn snapshot_is_dropped_when_the_subscription_ends() {
    let (env, subscription, _channel) = subscribed().await;
    send(&subscription, &EnvEvent::Snapshot(vec![entry("all.A", "old")]), false).await;
    env.vars(Duration::from_secs(10)).await.unwrap();

    // Server закрыл подписку: значения могли измениться, старые отдавать нельзя.
    send(&subscription, &EnvEvent::Snapshot(Vec::new()), true).await;

    for _ in 0..100 {
        if env.vars(Duration::from_millis(1)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("после закрытия подписки команда получает старый снимок");
}
