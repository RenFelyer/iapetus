use proptest::prelude::*;

use super::*;

fn arb_status() -> impl Strategy<Value = ExitStatus> {
    prop_oneof![any::<i32>().prop_map(ExitStatus::Code), any::<i32>().prop_map(ExitStatus::Signal)]
}

fn arb_reply() -> impl Strategy<Value = Reply> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..64).prop_map(Reply::Output),
        (arb_status(), any::<String>()).prop_map(|(status, cwd)| Reply::Exit { status, cwd }),
        any::<String>().prop_map(Reply::Failed),
    ]
}

fn arb_client_message() -> impl Strategy<Value = ClientMessage> {
    prop_oneof![
        (any::<String>(), any::<String>()).prop_map(|(command, cwd)| ClientMessage::Run { command, cwd }),
        Just(ClientMessage::Cancel),
    ]
}

#[test]
fn frames_survive_transport_in_one_byte_pieces() {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    proptest!(|(message in arb_client_message(), reply in arb_reply())| {
        runtime.block_on(async {
            // Канал на один байт: сообщения приходят читателю кусками, как по сокету.
            let (mut near, mut far) = tokio::io::duplex(1);
            let (sent_message, sent_reply) = (message.clone(), reply.clone());
            let writer = tokio::spawn(async move {
                write_frame(&mut near, &sent_message).await.unwrap();
                write_frame(&mut near, &sent_reply).await.unwrap();
            });
            assert_eq!(read_frame::<_, ClientMessage>(&mut far).await.unwrap(), message, "ClientMessage потерян");
            assert_eq!(read_frame::<_, Reply>(&mut far).await.unwrap(), reply, "Reply потерян");
            writer.await.unwrap();
        });
    });
}

#[tokio::test]
async fn oversized_length_is_rejected_before_allocation() {
    let mut bytes: &[u8] = &(MAX_FRAME + 1).to_be_bytes();
    let error = read_frame::<_, ClientMessage>(&mut bytes).await.unwrap_err();
    assert_eq!(
        error.kind(),
        io::ErrorKind::InvalidData,
        "длина выше предела должна быть InvalidData"
    );
}

#[test]
fn garbage_is_an_error_not_a_panic() {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    proptest!(|(garbage in prop::collection::vec(any::<u8>(), 0..64), body in prop::collection::vec(any::<u8>(), 0..64))| {
        // Мусор целиком и мусор под верным префиксом длины: во втором случае
        // разбирается именно тело. Результат любой, главное — без паники и
        // без зависания на конечном входе.
        let mut framed = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        for input in [&garbage, &framed] {
            let mut bytes: &[u8] = input;
            drop(runtime.block_on(read_frame::<_, ClientMessage>(&mut bytes)));
            let mut bytes: &[u8] = input;
            drop(runtime.block_on(read_frame::<_, Reply>(&mut bytes)));
        }
    });
}
