//! Сценарии регистрации узлов.

use russh::keys::PrivateKey;

use super::*;
use crate::server::registry::sessions::new_fingerprint;
use crate::server::registry::tests::{Fixture, random_key};

fn new_node(fingerprint: &Fingerprint, key: &PrivateKey, name: &str) -> NewNode {
    NewNode {
        fingerprint: fingerprint.clone(),
        public_key: key.public_key().clone(),
        name: name.to_owned(),
        os: "linux".to_owned(),
        parent: None,
    }
}

#[test]
fn fingerprint_cannot_be_taken_over_with_another_key() {
    let mut f = Fixture::new();
    let node = f.node("дом");

    let result = f.reg.register_node(&new_node(&node.fingerprint, &random_key(), "дом"));

    assert!(
        matches!(result, Err(Error::FingerprintTaken)),
        "fingerprint присвоен с другим ключом"
    );
    assert_eq!(
        f.reg.node_public_key(&node.fingerprint).unwrap(),
        *node.key.public_key(),
        "ключ узла заменён"
    );
}

#[test]
fn public_key_cannot_serve_two_fingerprints() {
    let mut f = Fixture::new();
    let node = f.node("дом");
    let other = new_fingerprint().unwrap();

    let result = f.reg.register_node(&new_node(&other, &node.key, "офис"));

    assert!(matches!(result, Err(Error::KeyTaken)), "один ключ служит двум узлам");
}

#[test]
fn key_comment_does_not_make_a_different_key() {
    let mut f = Fixture::new();
    let node = f.node("дом");
    let mut commented = new_node(&node.fingerprint, &node.key, "дом");
    commented.public_key = PublicKey::from_openssh(&format!("{} ноутбук", node.key.public_key().to_openssh().unwrap())).unwrap();

    f.reg
        .register_node(&commented)
        .expect("тот же ключ с комментарием отвергнут как чужой");
}

#[test]
fn node_cannot_take_the_fingerprint_of_a_session() {
    let mut f = Fixture::new();
    let session = f.session();
    let node = NewNode {
        fingerprint: session,
        public_key: random_key().public_key().clone(),
        name: "r".to_owned(),
        os: "linux".to_owned(),
        parent: None,
    };
    assert!(
        matches!(f.reg.register_node(&node), Err(Error::FingerprintTaken)),
        "узел занял fingerprint сессии"
    );
    assert!(f.reg.runners().unwrap().is_empty());
}

#[test]
fn revoked_node_stays_in_the_registry_but_cannot_register_or_renew_and_releases_its_session() {
    let mut f = Fixture::new();
    let node = f.node("дом");
    let session = f.session();
    f.reg.assign_runner(&session, &node.fingerprint).unwrap();

    let revocation = f.reg.revoke_node(&node.fingerprint).unwrap();

    assert_eq!(
        revocation.sessions,
        std::slice::from_ref(&session),
        "отзыв не назвал сессию, потерявшую runner"
    );
    assert!(
        f.reg.node(&node.fingerprint).unwrap().revoked,
        "запись отозванного узла не помечена"
    );
    assert!(
        f.reg.session(&session).unwrap().runner.is_none(),
        "назначение отозванного runner осталось"
    );
    assert!(
        matches!(f.reg.node_public_key(&node.fingerprint), Err(Error::Revoked)),
        "ключ отозванного узла выдан для проверки продления"
    );
    assert!(
        matches!(
            f.reg.register_node(&new_node(&node.fingerprint, &node.key, "дом")),
            Err(Error::Revoked)
        ),
        "отозванный узел зарегистрировался заново с тем же ключом"
    );
    assert!(
        matches!(f.reg.runner_destination(&node.fingerprint), Err(Error::Revoked)),
        "отозванному runner назван маршрут"
    );
    let other = f.session();
    assert!(
        matches!(f.reg.assign_runner(&other, &node.fingerprint), Err(Error::Revoked)),
        "отозванный runner назначен сессии"
    );
}

#[test]
fn revoked_key_cannot_be_registered_under_a_new_fingerprint() {
    let mut f = Fixture::new();
    let node = f.node("дом");
    f.reg.revoke_node(&node.fingerprint).unwrap();

    let result = f.reg.register_node(&new_node(&new_fingerprint().unwrap(), &node.key, "дом"));

    assert!(
        matches!(result, Err(Error::KeyTaken)),
        "ключ отозванного узла принят под новым fingerprint"
    );
}

fn new_clone(parent: &Fingerprint, name: &str) -> NewNode {
    NewNode {
        parent: Some(parent.clone()),
        ..new_node(&new_fingerprint().unwrap(), &random_key(), name)
    }
}

#[test]
fn clone_is_registered_as_temporary_with_its_parent_and_may_repeat_a_name() {
    let mut f = Fixture::new();
    let parent = f.node("дом");

    let first = f.reg.register_node(&new_clone(&parent.fingerprint, "proj-clone-1")).unwrap();
    let second = f.reg.register_node(&new_clone(&parent.fingerprint, "proj-clone-1")).unwrap();

    assert!(first.temporary && second.temporary, "клон не помечен временным");
    assert_eq!(first.parent.as_ref(), Some(&parent.fingerprint), "у клона нет родителя");
    assert_ne!(first.fingerprint, second.fingerprint);
    assert_eq!(first.name, second.name, "повтор имени клона изменил имя");
    assert!(!f.reg.node(&parent.fingerprint).unwrap().temporary, "родитель помечен временным");
}

#[test]
fn clone_of_an_unknown_parent_is_refused() {
    let mut f = Fixture::new();

    let result = f.reg.register_node(&new_clone(&new_fingerprint().unwrap(), "c"));

    assert!(
        matches!(result, Err(Error::InvalidParent)),
        "клон незарегистрированного родителя принят"
    );
    assert!(f.reg.runners().unwrap().is_empty(), "отказанный клон попал в реестр");
}

#[test]
fn clone_of_a_revoked_parent_is_refused() {
    let mut f = Fixture::new();
    let parent = f.node("дом");
    f.reg.revoke_node(&parent.fingerprint).unwrap();

    let result = f.reg.register_node(&new_clone(&parent.fingerprint, "c"));

    assert!(matches!(result, Err(Error::InvalidParent)), "клон отозванного родителя принят");
}

#[test]
fn clone_of_a_clone_is_refused() {
    let mut f = Fixture::new();
    let parent = f.node("дом");
    let clone = f.reg.register_node(&new_clone(&parent.fingerprint, "c")).unwrap();

    let result = f.reg.register_node(&new_clone(&clone.fingerprint, "cc"));

    assert!(matches!(result, Err(Error::InvalidParent)), "клон клона принят");
}

#[test]
fn revoking_a_parent_revokes_its_clones_and_releases_their_sessions() {
    let mut f = Fixture::new();
    let parent = f.node("дом");
    let other = f.node("чужой");
    let clone = f.reg.register_node(&new_clone(&parent.fingerprint, "c")).unwrap();
    let session = f.session();
    f.reg.assign_runner(&session, &clone.fingerprint).unwrap();

    let revocation = f.reg.revoke_node(&parent.fingerprint).unwrap();

    assert_eq!(revocation.nodes, [parent.fingerprint.clone(), clone.fingerprint.clone()]);
    assert_eq!(revocation.sessions, [session], "сессия клона не освобождена");
    assert!(
        f.reg.node(&clone.fingerprint).unwrap().revoked,
        "клон отозванного родителя не отозван"
    );
    assert!(!f.reg.node(&other.fingerprint).unwrap().revoked, "отозван посторонний узел");
}
