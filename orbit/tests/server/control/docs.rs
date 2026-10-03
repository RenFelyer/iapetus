use tempfile::TempDir;

use super::*;

fn doc(name: &str, content: &str) -> Document {
    Document {
        name: name.to_owned(),
        content: content.to_owned(),
    }
}

#[tokio::test(start_paused = true)]
async fn subscription_sends_snapshot_then_updates_and_removals() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.md"), "один").unwrap();
    fs::write(dir.path().join("note.txt"), "не документ").unwrap();
    let mut subscription = Docs::new(dir.path().to_owned()).subscribe();

    assert_eq!(
        subscription.next().await,
        DocsEvent::Snapshot(vec![doc("a.md", "один")]),
        "снимок не первый или в него попал не документ"
    );

    fs::write(dir.path().join("a.md"), "два").unwrap();
    assert_eq!(
        subscription.next().await,
        DocsEvent::Updated(doc("a.md", "два")),
        "изменение документа не доставлено"
    );
    fs::write(dir.path().join("b.md"), "новый").unwrap();
    assert_eq!(
        subscription.next().await,
        DocsEvent::Updated(doc("b.md", "новый")),
        "новый документ не доставлен"
    );
    fs::remove_file(dir.path().join("a.md")).unwrap();
    assert_eq!(
        subscription.next().await,
        DocsEvent::Removed { name: "a.md".to_owned() },
        "удаление документа не доставлено"
    );
}

#[tokio::test(start_paused = true)]
async fn oversized_document_is_not_sent() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("big.md"), vec![b'x'; usize::try_from(MAX_DOCUMENT).unwrap() + 1]).unwrap();
    fs::write(dir.path().join("ok.md"), "ок").unwrap();
    let mut subscription = Docs::new(dir.path().to_owned()).subscribe();

    assert_eq!(
        subscription.next().await,
        DocsEvent::Snapshot(vec![doc("ok.md", "ок")]),
        "документ больше 1 МиБ попал в снимок"
    );
}
