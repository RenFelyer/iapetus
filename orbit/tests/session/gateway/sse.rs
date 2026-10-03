use insta::assert_snapshot;
use proptest::prelude::*;

use super::*;
use crate::session::gateway::tests::slow_tests_enabled;

/// События, которые разборщик выдал на куски, по строке на событие.
fn events_of(chunks: &[&[u8]]) -> Vec<String> {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        parser.feed(chunk, |event, data| {
            events.push(format!("{event:?} {data:?}"));
        });
    }
    events
}

#[test]
fn events_follow_the_sse_format() {
    // Комментарий пропускается, CRLF считается концом строки, строки `data`
    // склеиваются через `\n`, пробел после двоеточия не входит в значение.
    let events = events_of(&[
        b": ping\r\nevent: a\r\ndata: x\r\ndata:y\r\n\r\n",
        b"event: only-name\n\n",
        b"data: only-data\n\n",
        b"\n\n\n",
        b"data: {\"\xd1\x84\":1}\n\n",
    ]);
    assert_snapshot!(events.join("\n"), @r#"
    "a" "x\ny"
    "only-name" ""
    "" "only-data"
    "" "{\"ф\":1}"
    "#);
}

/// Байты, из которых собирается похожий на SSE мусор, и любые другие.
fn sse_like_bytes() -> impl Strategy<Value = Vec<u8>> {
    let structural = prop::sample::select(b"event:data \r\n{}\"x1\xd1".to_vec());
    prop::collection::vec(prop_oneof![3 => structural, 1 => any::<u8>()], 0..300)
}

#[test]
fn any_chunking_of_any_input_gives_the_same_events_without_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(input in sse_like_bytes(), cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8))| {
        let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(input.len() + 1)).collect();
        points.sort_unstable();
        let mut chunks = Vec::new();
        let mut start = 0;
        for point in points {
            chunks.push(&input[start..point]);
            start = point;
        }
        chunks.push(&input[start..]);
        prop_assert_eq!(events_of(&chunks), events_of(&[&input]));
    });
}
