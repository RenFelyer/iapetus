use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::session::gateway::tests::{slow_tests_enabled, status_of};

fn watch_events(chunks: &[&[u8]]) -> Vec<GatewayEvent> {
    let mut watch = StreamWatch::default();
    let mut events = Vec::new();
    for chunk in chunks {
        watch.feed(chunk, |event| events.push(event));
    }
    events
}

/// Поле JSON с нужным или испорченным значением: чужой поток не обязан
/// держать форму.
fn field(values: Vec<Value>) -> impl Strategy<Value = Value> {
    prop::sample::select(values)
}

fn arb_data() -> impl Strategy<Value = String> {
    let name = field(vec![
        json!("view_file"),
        json!("read_file"),
        json!("run_command"),
        json!(4),
        Value::Null,
    ]);
    let args = field(vec![
        json!({"AbsolutePath": "/a"}),
        json!({"file_path": "/b"}),
        json!({"TargetFile": 3}),
        json!("текст"),
        Value::Null,
    ]);
    let parts = field(vec![json!(1), Value::Null, json!("x")]);
    let reason = field(vec![json!("STOP"), json!("MAX_TOKENS"), json!(1), Value::Null]);
    let candidates = field(vec![json!([]), json!({}), json!(5), Value::Null]);
    (name, args, parts, reason, candidates, any::<bool>(), any::<bool>()).prop_map(
        |(name, args, parts, reason, candidates, wrapped, broken)| {
            if broken {
                return "{не json".to_owned();
            }
            let part = json!({"functionCall": {"name": name, "args": args}});
            let intact = json!({"candidates": [{"content": {"parts": [part, {"text": "t"}, parts]}, "finishReason": reason}]});
            let response = if wrapped { json!({"response": intact}) } else { intact };
            // Null — форма цела, остальное подменяет `candidates` целиком.
            if candidates.is_null() {
                response.to_string()
            } else {
                json!({"candidates": candidates}).to_string()
            }
        },
    )
}

fn arb_stream() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(arb_data(), 0..24).prop_map(|events| {
        events
            .into_iter()
            .map(|data| format!("data: {data}\n\n"))
            .collect::<Vec<_>>()
            .concat()
            .into_bytes()
    })
}

#[test]
fn malformed_gemini_events_do_not_panic_and_do_not_depend_on_chunking() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(stream in arb_stream(), cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8))| {
        let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(stream.len() + 1)).collect();
        points.sort_unstable();
        let mut chunks = Vec::new();
        let mut start = 0;
        for point in points {
            chunks.push(&stream[start..point]);
            start = point;
        }
        chunks.push(&stream[start..]);
        prop_assert_eq!(watch_events(&chunks), watch_events(&[&stream]));
    });
}

/// Тело ошибки: испорченный JSON с полями, которые смотрит разбор, или любые байты.
fn arb_error_body() -> impl Strategy<Value = Vec<u8>> {
    let kind = field(vec![
        json!("type.googleapis.com/google.rpc.RetryInfo"),
        json!("type.googleapis.com/google.rpc.QuotaFailure"),
        json!("type.googleapis.com/google.rpc.ErrorInfo"),
        json!(1),
        Value::Null,
    ]);
    let delay = field(vec![
        json!("40s"),
        json!("2.5s"),
        json!("-1s"),
        json!("1e300s"),
        json!("s"),
        json!(40),
        Value::Null,
    ]);
    let reason = field(vec![
        json!("API_KEY_INVALID"),
        json!("QUOTA_EXHAUSTED"),
        json!("с пробелом"),
        Value::Null,
    ]);
    let message = field(vec![
        json!("API key not valid"),
        json!("exceeds the maximum number of tokens"),
        json!(3),
        Value::Null,
    ]);
    let structured = (kind, delay, reason, message).prop_map(|(kind, delay, reason, message)| {
        json!({"error": {"status": "X", "message": message,
                         "details": [{"@type": kind, "retryDelay": delay, "reason": reason}]}})
        .to_string()
        .into_bytes()
    });
    prop_oneof![structured, prop::collection::vec(any::<u8>(), 0..64)]
}

/// Какое событие допустимо у статуса: вид события решает статус, тело только уточняет его.
fn fits_status(status: u16, event: Option<&GatewayEvent>) -> bool {
    use GatewayEvent::{ContextOverflow, RequestRateLimited, ServerError, SubscriptionLimitReached, Unauthorized, UnretriedError};
    let kind_fits = match status {
        401 | 403 => matches!(event, Some(Unauthorized { .. })),
        400 => matches!(event, Some(Unauthorized { .. } | ContextOverflow | UnretriedError { .. })),
        429 => matches!(event, Some(SubscriptionLimitReached { .. } | RequestRateLimited)),
        500 => matches!(event, Some(UnretriedError { .. })),
        501..=599 => matches!(event, Some(ServerError { .. })),
        _ => event.is_none(),
    };
    kind_fits && status_of(event).is_none_or(|got| got == status)
}

#[test]
fn malformed_error_bodies_give_the_event_of_their_status_without_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(body in arb_error_body(), status in 100u16..600)| {
        let event = error_body_event(StatusCode::from_u16(status).unwrap(), &body);
        prop_assert!(fits_status(status, event.as_ref()), "у статуса {status} событие {event:?}");
    });
}

/// Сводка квоты: окна с испорченными полями или любые байты.
fn arb_summary() -> impl Strategy<Value = Vec<u8>> {
    let fraction = field(vec![json!(0), json!(0.5), json!(-1), json!("0"), Value::Null]);
    let reset = field(vec![
        json!("2026-10-01T13:00:00Z"),
        json!("2026-10-01T13:00:00.123+05:30"),
        json!("1969-12-31T23:59:59Z"),
        json!("2026-13-01T00:00:00Z"),
        json!(1_790_900_000),
        Value::Null,
    ]);
    let disabled = field(vec![json!(true), json!(false), json!("true"), Value::Null]);
    let bucket = (fraction, reset, disabled)
        .prop_map(|(fraction, reset, disabled)| json!({"remainingFraction": fraction, "resetTime": reset, "disabled": disabled}));
    let structured = (prop::collection::vec(bucket, 0..4), any::<bool>()).prop_map(|(buckets, wrapped)| {
        let summary = json!({ "buckets": buckets });
        let body = if wrapped { json!({ "response": summary }) } else { summary };
        body.to_string().into_bytes()
    });
    prop_oneof![structured, prop::collection::vec(any::<u8>(), 0..64)]
}

#[test]
fn malformed_quota_summaries_give_at_most_a_limit_without_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(body in arb_summary())| {
        let event = quota_summary_event(&body);
        prop_assert!(
            matches!(event, None | Some(GatewayEvent::SubscriptionLimitReached { reset: None | Some(LimitReset::At(_)) })),
            "сводка квоты дала {event:?}"
        );
    });
}
