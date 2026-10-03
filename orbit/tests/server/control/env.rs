use std::str::FromStr;
use std::time::Duration;

use russh::keys::ssh_key::{Algorithm, PrivateKey};
use tempfile::TempDir;

use super::*;
use crate::server::registry::{NewNode, TokenCipher};

struct Fixture {
    _dir: TempDir,
    registry: Mutex<Registry>,
    env: Env,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open(&dir.path().join("registry.db"), TokenCipher::from_key([3; 32])).unwrap();
        Self {
            _dir: dir,
            registry: Mutex::new(registry),
            env: Env::new(),
        }
    }

    fn runner(&self, name: &str, parent: Option<&Fingerprint>) -> Fingerprint {
        let fingerprint = Fingerprint::from_str(name).unwrap();
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let node = NewNode {
            fingerprint: fingerprint.clone(),
            public_key: key.public_key().clone(),
            name: name.to_owned(),
            os: "linux".to_owned(),
            parent: parent.cloned(),
        };
        self.registry.lock().unwrap().register_node(&node).unwrap();
        fingerprint
    }

    fn set(&self, key: &str, value: &str) {
        let change = EnvChange::Set {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        self.env.change(&self.registry, &change).unwrap();
    }
}

fn set(key: &str, value: &str) -> EnvChange {
    EnvChange::Set {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

/// Ждёт изменение не дольше секунды: подписчик, не получивший своё, не должен зависать тест.
async fn next(subscription: &mut EnvSubscription) -> Option<EnvChange> {
    tokio::time::timeout(Duration::from_secs(1), subscription.next())
        .await
        .ok()
        .flatten()
}

fn snapshot_keys(subscription: &EnvSubscription) -> Vec<&str> {
    subscription.snapshot.iter().map(|entry| entry.key.as_str()).collect()
}

#[test]
fn env_key_is_a_shared_or_per_runner_variable_and_nothing_else() {
    for key in ["all.PATH", "all._X1", "runner-1.TOKEN", "abc_DEF.Y"] {
        assert!(key_is_valid(key), "{key:?} отвергнут");
    }
    for key in [
        "all", "all.", ".X", "all.1X", "all.A-B", "a.b.C", "all.A B", "r*.X", "all.>", "all.*",
    ] {
        assert!(!key_is_valid(key), "{key:?} принят за ключ окружения");
    }
}

#[test]
fn snapshot_has_only_own_parent_and_shared_layers() {
    let f = Fixture::new();
    let parent = f.runner("parent", None);
    let clone = f.runner("clone", Some(&parent));
    let stranger = f.runner("stranger", None);
    f.set("all.A", "1");
    f.set("parent.B", "2");
    f.set("clone.C", "3");
    f.set("stranger.D", "4");

    let own = f.env.subscribe(&f.registry, &clone).unwrap();
    let plain = f.env.subscribe(&f.registry, &parent).unwrap();

    assert_eq!(
        snapshot_keys(&own),
        ["all.A", "clone.C", "parent.B"],
        "клон получил чужой слой или не получил свой"
    );
    assert_eq!(
        snapshot_keys(&plain),
        ["all.A", "parent.B"],
        "runner получил слой клона или чужой слой"
    );
    assert!(
        !snapshot_keys(&f.env.subscribe(&f.registry, &stranger).unwrap()).contains(&"parent.B"),
        "runner получил слой чужого родителя"
    );
}

#[tokio::test(start_paused = true)]
async fn runner_receives_only_changes_of_its_layers() {
    let f = Fixture::new();
    let parent = f.runner("parent", None);
    let clone = f.runner("clone", Some(&parent));
    f.runner("stranger", None);
    let mut subscription = f.env.subscribe(&f.registry, &clone).unwrap();

    f.set("stranger.X", "чужое");
    f.set("all.A", "1");
    f.set("stranger.Y", "чужое");
    f.env
        .change(
            &f.registry,
            &EnvChange::Delete {
                key: "parent.B".to_owned(),
            },
        )
        .unwrap();
    f.set("clone.C", "3");

    let received = [
        next(&mut subscription).await,
        next(&mut subscription).await,
        next(&mut subscription).await,
    ];
    assert_eq!(
        received,
        [
            Some(set("all.A", "1")),
            Some(EnvChange::Delete {
                key: "parent.B".to_owned()
            }),
            Some(set("clone.C", "3")),
        ],
        "подписчик получил чужое изменение или потерял своё"
    );
    assert_eq!(next(&mut subscription).await, None, "после своих изменений пришло ещё одно");
}

#[test]
fn foreign_key_shape_is_rejected_and_not_stored() {
    let f = Fixture::new();

    let result = f.env.change(&f.registry, &set("all.A-B", "x"));

    assert!(result.is_err(), "ключ неверного вида принят");
    assert!(Env::list(&f.registry).unwrap().is_empty(), "ключ неверного вида записан");
}

#[test]
fn deleted_key_leaves_the_list_and_the_next_snapshot() {
    let f = Fixture::new();
    let runner = f.runner("r1", None);
    f.set("all.A", "1");
    f.env.change(&f.registry, &EnvChange::Delete { key: "all.A".to_owned() }).unwrap();

    assert!(Env::list(&f.registry).unwrap().is_empty(), "удалённый ключ остался в списке");
    assert!(
        f.env.subscribe(&f.registry, &runner).unwrap().snapshot.is_empty(),
        "удалённый ключ попал в снимок"
    );
}
