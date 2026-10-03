//! Разбор потока событий: любое разбиение входа даёт те же события, мусор не паникует.

use proptest::prelude::*;

use super::*;

const STREAM: &str =
    ": comment\nevent: endpoint\ndata: /messages?s=1\n\ndata: {\"a\":1}\ndata: second\r\n\r\nevent: ping\n\nid: 7\ndata: last\r\r";

fn parse(chunks: &[&[u8]]) -> Vec<Event> {
    let mut events = Events::default();
    chunks.iter().flat_map(|chunk| events.push(chunk).unwrap()).collect()
}

fn event(name: &str, data: &str) -> Event {
    Event {
        name: name.to_owned(),
        data: data.to_owned(),
    }
}

#[test]
fn events_take_name_and_joined_data_and_skip_comments_and_events_without_data() {
    assert_eq!(
        parse(&[STREAM.as_bytes()]),
        [
            event("endpoint", "/messages?s=1"),
            event("message", "{\"a\":1}\nsecond"),
            event("message", "last"),
        ],
        "события разобраны неверно"
    );
}

#[test]
fn event_without_the_closing_blank_line_is_not_delivered() {
    assert!(parse(&[b"data: x\n"]).is_empty(), "неполное событие доставлено");
}

#[test]
fn event_over_the_limit_is_an_error() {
    let mut events = Events::default();
    let big = vec![b'x'; MAX_MESSAGE + 1];
    assert!(events.push(&big).is_err(), "строка больше предела принята");
}

proptest! {
    #[test]
    fn any_split_of_the_stream_gives_the_same_events(cuts in proptest::collection::vec(0..STREAM.len(), 0..6)) {
        let bytes = STREAM.as_bytes();
        let mut cuts = cuts;
        cuts.sort_unstable();
        let mut chunks = Vec::new();
        let mut from = 0;
        for cut in cuts {
            chunks.push(&bytes[from..cut]);
            from = cut;
        }
        chunks.push(&bytes[from..]);
        prop_assert_eq!(parse(&chunks), parse(&[bytes]), "разбиение {:?} изменило события", chunks);
    }

    #[test]
    fn garbage_is_never_a_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut events = Events::default();
        events.push(&bytes).unwrap();
    }
}
