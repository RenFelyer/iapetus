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
    let kind = field(vec![
        json!("response.created"),
        json!("response.output_item.done"),
        json!("response.completed"),
        json!("response.incomplete"),
        json!("response.failed"),
        json!("response.output_text.delta"),
        json!(5),
        Value::Null,
    ]);
    let item_type = field(vec![
        json!("function_call"),
        json!("custom_tool_call"),
        json!("message"),
        json!(1),
        Value::Null,
    ]);
    let name = field(vec![json!("exec_command"), json!(7), Value::Null]);
    let arguments = field(vec![json!("{\"cmd\":\"ls\"}"), json!("{не json"), json!(3), Value::Null]);
    let end_turn = field(vec![json!(false), json!(true), json!("false"), Value::Null]);
    let reason = field(vec![json!("interrupted"), json!("max_output_tokens"), json!(2), Value::Null]);
    let code = field(vec![
        json!("context_length_exceeded"),
        json!("rate_limit_exceeded"),
        json!("x"),
        json!(1),
        Value::Null,
    ]);
    (kind, item_type, name, arguments, end_turn, reason, code, any::<bool>()).prop_map(
        |(kind, item_type, name, arguments, end_turn, reason, code, broken)| {
            if broken {
                return "{не json".to_owned();
            }
            json!({
                "type": kind,
                "item": {"type": item_type, "name": name, "arguments": arguments, "input": "текст"},
                "response": {"end_turn": end_turn, "incomplete_details": {"reason": reason}, "error": {"code": code}},
            })
            .to_string()
        },
    )
}

fn arb_stream() -> impl Strategy<Value = Vec<u8>> {
    let name = prop::sample::select(vec!["response.created", "response.completed", "ping", "неизвестное"]);
    prop::collection::vec((name, arb_data()), 0..24).prop_map(|events| {
        events
            .into_iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect::<Vec<_>>()
            .concat()
            .into_bytes()
    })
}

#[test]
fn malformed_responses_events_do_not_panic_and_do_not_depend_on_chunking() {
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
        json!("usage_limit_reached"),
        json!("rate_limit_exceeded"),
        json!(3),
        Value::Null,
    ]);
    let code = field(vec![
        json!("invalid_api_key"),
        json!("usage_limit_reached"),
        json!("пробел и текст"),
        Value::Null,
    ]);
    let resets_at = field(vec![json!(1_790_900_000), json!(-1), json!("1790900000"), json!(1.5), Value::Null]);
    let resets_in = field(vec![json!(3600), json!(-5), json!("3600"), Value::Null]);
    let structured = (kind, code, resets_at, resets_in).prop_map(|(kind, code, resets_at, resets_in)| {
        json!({"error": {"type": kind, "code": code, "resets_at": resets_at, "resets_in_seconds": resets_in}})
            .to_string()
            .into_bytes()
    });
    prop_oneof![structured, prop::collection::vec(any::<u8>(), 0..64)]
}

/// Какое событие допустимо у статуса: вид события решает статус, тело только уточняет его.
fn fits_status(status: u16, event: Option<&GatewayEvent>) -> bool {
    use GatewayEvent::{ServerError, SubscriptionLimitReached, Unauthorized, UnretriedError};
    let kind_fits = match status {
        401 | 403 => matches!(event, Some(Unauthorized { .. })),
        429 => matches!(event, Some(SubscriptionLimitReached { .. } | UnretriedError { .. })),
        500..=599 => matches!(event, Some(ServerError { .. })),
        _ => event.is_none(),
    };
    kind_fits && status_of(event).is_none_or(|got| got == status)
}

#[test]
fn malformed_error_bodies_give_the_event_of_their_status_without_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(body in arb_error_body(), status in 100u16..600, header in any::<bool>())| {
        let reset = header.then_some(LimitReset::At(1));
        let event = error_body_event(StatusCode::from_u16(status).unwrap(), &body, reset);
        prop_assert!(fits_status(status, event.as_ref()), "у статуса {status} событие {event:?}");
    });
}
fn codex_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn usage_windows_come_from_codex_percent_minutes_and_reset_headers() {
    let headers = codex_headers(&[
        ("x-codex-primary-used-percent", "42.5"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", "1790800000"),
        ("x-codex-secondary-used-percent", "250"),
    ]);

    assert_eq!(
        usage_windows(&headers),
        [
            UsageWindow {
                duration: Some(18_000),
                scope: None,
                used: 0.425,
                resets_at: Some(1_790_800_000),
            },
            UsageWindow {
                duration: None,
                scope: None,
                used: 1.0,
                resets_at: None,
            },
        ]
    );
}

#[test]
fn codex_windows_without_a_parsable_percent_are_skipped() {
    let headers = codex_headers(&[
        ("x-codex-primary-used-percent", "много"),
        ("x-codex-primary-reset-at", "1790800000"),
        ("x-codex-secondary-window-minutes", "10080"),
    ]);

    assert_eq!(usage_windows(&headers), [], "окно без процента не должно появляться");
}

#[test]
fn codex_usage_windows_do_not_panic_and_stay_within_bounds() {
    if !slow_tests_enabled() {
        return;
    }
    let value = prop::sample::select(vec!["0", "55.5", "-1", "1e40", "inf", "NaN", "", "18446744073709551615", "завтра"]);
    let field = prop::sample::select(vec!["used-percent", "window-minutes", "reset-at"]);
    let window = prop::sample::select(vec!["primary", "secondary"]);
    proptest!(|(entries in prop::collection::vec((window, field, value), 0..8))| {
        let mut headers = HeaderMap::new();
        for (window, field, value) in entries {
            if let Ok(value) = axum::http::HeaderValue::from_str(value) {
                headers.insert(
                    axum::http::HeaderName::from_bytes(format!("x-codex-{window}-{field}").as_bytes()).unwrap(),
                    value,
                );
            }
        }
        for window in usage_windows(&headers) {
            prop_assert!((0.0..=1.0).contains(&window.used), "доля {} вне 0..1", window.used);
        }
    });
}
