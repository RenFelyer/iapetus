//! Утверждения о кадрах: поток разбирается одинаково при любом разбиении на куски, мусор даёт ошибку без паники,
//! заявленные пределы проверяются по заголовку.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::mux::MAX_MESSAGE;
use crate::protocol::VERSION;

/// Сообщение глазами теста: направление и признак последнего задают флаги кадров.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Msg {
    id: u32,
    request: bool,
    last: bool,
    body: Vec<u8>,
}

impl Msg {
    fn flags(&self) -> u8 {
        (if self.request { REQUEST } else { 0 }) | (if self.last { LAST } else { 0 })
    }
}

fn hello(version: u16) -> BytesMut {
    let mut out = BytesMut::new();
    encode_frame(&mut out, 0, 0, &version.to_be_bytes());
    out
}

/// Байты потока, который пишет сторона: приветствие и кадры сообщений в порядке очереди отправки.
fn encode_stream(messages: &[Msg], round_limit: usize) -> Vec<u8> {
    let mut out = hello(VERSION);
    let mut scheduler = Scheduler::default();
    for message in messages {
        scheduler.push(Outgoing {
            id: message.id,
            flags: message.flags(),
            body: Bytes::from(message.body.clone()),
        });
    }
    while !scheduler.is_empty() {
        scheduler.round(&mut out, round_limit);
    }
    out.to_vec()
}

/// Разбирает кадры из `bytes`, подавая их кусками по `sizes` (по кругу); приветствие пропускается.
fn decode_in_chunks(bytes: &[u8], sizes: &[usize]) -> Result<Vec<Message>, FrameError> {
    let mut reader = FrameReader::default();
    let mut assembler = Assembler::new(MAX_MESSAGE);
    let mut messages = Vec::new();
    let mut greeted = false;
    let mut rest = bytes;
    let mut sizes = sizes.iter().cycle();
    while !rest.is_empty() {
        let take = (*sizes.next().unwrap_or(&1)).clamp(1, rest.len());
        reader.push(&rest[..take]);
        rest = &rest[take..];
        while let Some(frame) = reader.next()? {
            if !greeted {
                greeted = true;
                continue;
            }
            messages.extend(assembler.add(frame)?);
        }
    }
    Ok(messages)
}

/// Сообщения по ключу (id, направление): признак последнего и тело.
type ByKey = BTreeMap<(u32, bool), Vec<(bool, Vec<u8>)>>;

/// Сообщения по ключу (id, направление) в порядке прихода: порядок между разными ключами очередь не обещает.
fn by_key(messages: impl IntoIterator<Item = (u32, bool, bool, Vec<u8>)>) -> ByKey {
    let mut map: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (id, request, last, body) in messages {
        map.entry((id, request)).or_default().push((last, body));
    }
    map
}

fn body_strategy() -> impl Strategy<Value = Vec<u8>> {
    // Границы кадра отдельно: ошибки на стыках кадров живут именно там.
    prop_oneof![
        (0..64usize).prop_map(|n| vec![1; n]),
        Just(vec![2; FRAME_BODY - 1]),
        Just(vec![3; FRAME_BODY]),
        Just(vec![4; FRAME_BODY + 1]),
        Just(vec![5; 2 * FRAME_BODY]),
        prop::collection::vec(any::<u8>(), 0..3 * FRAME_BODY),
    ]
}

fn message_strategy() -> impl Strategy<Value = Msg> {
    // Малый диапазон id: сообщения с одним id сталкиваются в очереди.
    (1u32..4, any::<bool>(), any::<bool>(), body_strategy()).prop_map(|(id, request, last, body)| Msg { id, request, last, body })
}

proptest! {
    /// Любое разбиение потока на куски даёт те же сообщения; сообщения одного id идут по порядку и не смешиваются
    /// с соседними, даже когда в очереди несколько сообщений с одним id.
    #[test]
    fn any_split_of_the_stream_yields_the_same_messages(
        messages in prop::collection::vec(message_strategy(), 0..10),
        round_limit in 1usize..70_000,
        sizes in prop::collection::vec(1usize..5000, 1..20),
    ) {
        let bytes = encode_stream(&messages, round_limit);
        let decoded = decode_in_chunks(&bytes, &sizes).expect("корректный поток разобран с ошибкой");
        let expected = by_key(messages.iter().map(|m| (m.id, m.request, m.last, m.body.clone())));
        let actual = by_key(decoded.into_iter().map(|m| (m.id, m.request, m.last, m.body.to_vec())));
        prop_assert_eq!(actual, expected, "сообщения потеряны, искажены или переставлены внутри id");
    }

    /// Мусор на входе — ошибка или недочитанный кадр, но не паника и не бесконечная работа.
    #[test]
    fn garbage_never_panics_the_decoder(bytes in prop::collection::vec(any::<u8>(), 0..4000), size in 1usize..300) {
        let result = decode_in_chunks(&bytes, &[size]);
        if let Ok(messages) = result {
            for message in messages {
                prop_assert!(message.body.len() <= MAX_MESSAGE, "сообщение больше предела");
            }
        }
    }
}

#[test]
fn oversized_frame_is_rejected_from_the_header_alone() {
    // Тело ещё не пришло: ждать FRAME_BODY + 1 байт не нужно, заявленной длины достаточно.
    let mut reader = FrameReader::default();
    let mut header = BytesMut::new();
    encode_frame(&mut header, 0, 0, &[]);
    header[..4].copy_from_slice(&u32::try_from(FRAME_BODY + 1).unwrap().to_be_bytes());
    reader.push(&header);
    assert_eq!(reader.next(), Err(FrameError::TooLong(FRAME_BODY + 1)));
}

#[test]
fn unknown_flags_are_rejected() {
    let mut reader = FrameReader::default();
    let mut out = BytesMut::new();
    encode_frame(&mut out, 1, 0x80, &[]);
    reader.push(&out);
    assert_eq!(reader.next(), Err(FrameError::UnknownFlags(0x80)));
}

#[test]
fn frame_marked_as_continuing_and_last_is_rejected() {
    let mut reader = FrameReader::default();
    let mut out = BytesMut::new();
    encode_frame(&mut out, 1, MORE | LAST, &[]);
    reader.push(&out);
    assert_eq!(reader.next(), Err(FrameError::MoreAndLast));
}

#[test]
fn message_longer_than_the_limit_is_rejected_while_it_is_assembled() {
    let mut assembler = Assembler::new(10);
    let frame = |body: &[u8], flags| Frame {
        id: 1,
        flags,
        body: Bytes::copy_from_slice(body),
    };
    assert_eq!(assembler.add(frame(&[0; 6], MORE)), Ok(None));
    assert_eq!(assembler.add(frame(&[0; 6], 0)), Err(FrameError::MessageTooLarge));
}
