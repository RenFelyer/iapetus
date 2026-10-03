use std::time::Duration;

use super::*;

fn main_request(event: GatewayEvent) -> Observation {
    Observation {
        request: RequestKind::Main,
        event,
    }
}

/// Ждёт, пока эпоха достигнет `wanted`: обработчик событий работает в своей задаче.
async fn wait_for_epoch(epoch: &fs::Epoch, wanted: u64) {
    for _ in 0..200 {
        if epoch.current() >= wanted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(epoch.current(), wanted, "эпоха FUSE не достигла ожидаемого номера");
}

#[tokio::test]
async fn only_end_of_turn_advances_the_epoch() {
    let epoch = fs::Epoch::new();
    let start = epoch.current();
    let (events, received) = broadcast::channel(16);
    tokio::spawn(advance_epoch_at_end_of_turn(received, epoch.clone()));
    for event in [
        GatewayEvent::ToolUse {
            name: "Bash".to_owned(),
            input: serde_json::Value::Null,
            path: None,
        },
        GatewayEvent::Interrupted,
        GatewayEvent::ContextOverflow,
        GatewayEvent::RequestRateLimited,
    ] {
        events.send(main_request(event)).unwrap();
    }
    events.send(main_request(GatewayEvent::EndTurn)).unwrap();
    // События приходят по порядку: когда конец хода обработан, остальные уже обработаны, и ни одно не сменило эпоху.
    wait_for_epoch(&epoch, start + 1).await;
    events.send(main_request(GatewayEvent::EndTurn)).unwrap();
    wait_for_epoch(&epoch, start + 2).await;
}

#[tokio::test]
async fn lost_events_advance_the_epoch_because_one_of_them_may_be_the_end_of_turn() {
    let epoch = fs::Epoch::new();
    let start = epoch.current();
    let (events, received) = broadcast::channel(2);
    for _ in 0..5 {
        events.send(main_request(GatewayEvent::Interrupted)).unwrap();
    }
    tokio::spawn(advance_epoch_at_end_of_turn(received, epoch.clone()));
    wait_for_epoch(&epoch, start + 1).await;
}

#[tokio::test]
async fn end_of_turn_of_a_service_request_does_not_advance_the_epoch() {
    let epoch = fs::Epoch::new();
    let start = epoch.current();
    let (events, received) = broadcast::channel(16);
    tokio::spawn(advance_epoch_at_end_of_turn(received, epoch.clone()));
    for request in [
        RequestKind::Subagent { id: None, kind: None },
        RequestKind::Title,
        RequestKind::Compaction,
        RequestKind::Other,
    ] {
        events
            .send(Observation {
                request,
                event: GatewayEvent::EndTurn,
            })
            .unwrap();
    }
    events.send(main_request(GatewayEvent::EndTurn)).unwrap();
    // Обработчик без await между приёмом события и сменой эпохи: когда очередь пуста, все события учтены.
    while !events.is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(epoch.current(), start + 1, "эпоха должна смениться один раз, за основной ход");
}

fn tool_use(name: &str, path: Option<&str>) -> GatewayEvent {
    GatewayEvent::ToolUse {
        name: name.to_owned(),
        input: serde_json::Value::Null,
        path: path.map(str::to_owned),
    }
}

#[test]
fn only_reading_tools_with_a_path_ask_for_preload() {
    let wanted = |event: GatewayEvent| file_to_preload(&event).map(|path| path.to_str().unwrap().to_owned());
    assert_eq!(wanted(tool_use("Read", Some("/w/a"))).as_deref(), Some("/w/a"));
    assert_eq!(wanted(tool_use("view_file", Some("/w/a"))).as_deref(), Some("/w/a"));
    assert_eq!(wanted(tool_use("Read", None)), None, "путь не распознан");
    assert_eq!(wanted(tool_use("Write", Some("/w/a"))), None, "новый файл скачивать нечего");
    assert_eq!(wanted(tool_use("Bash", Some("/w/a"))), None);
    assert_eq!(wanted(GatewayEvent::EndTurn), None);
}
