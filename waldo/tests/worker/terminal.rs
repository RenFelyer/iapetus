use super::*;

#[tokio::test]
async fn detaching_leaves_the_alternate_screen_of_the_agent() {
    let mut out = Vec::new();
    leave_agent_screen(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\x1b[?1049l"),
        "после выхода из агента терминал остался в его альтернативном экране: {text:?}"
    );
}
