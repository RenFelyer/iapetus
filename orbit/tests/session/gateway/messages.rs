use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::session::gateway::tests::{slow_tests_enabled, status_of};

/// Поле JSON с нужным или испорченным значением: чужой поток не обязан
/// держать форму.
fn field(values: Vec<Value>) -> impl Strategy<Value = Value> {
    prop::sample::select(values)
}

fn arb_data() -> impl Strategy<Value = String> {
    let index = field(vec![json!(0), json!(1), json!(2), json!(-1), json!("0"), Value::Null]);
    let name = field(vec![json!("Read"), json!("Write"), json!("Bash"), json!(7), Value::Null]);
    let kind = field(vec![json!("tool_use"), json!("text"), json!("input_json_delta"), json!(3)]);
    let input = field(vec![json!({}), json!({"file_path": "/a"}), json!("текст"), Value::Null]);
    let partial = field(vec![json!("{\"file_"), json!("path\":\"/b\"}"), json!(""), json!(5), Value::Null]);
    let stop = field(vec![json!("end_turn"), json!("tool_use"), json!(1), Value::Null]);
    let message = field(vec![json!("prompt is too long"), json!("x"), json!(2), Value::Null]);
    let error_type = field(vec![json!("rate_limit_error"), json!("overloaded_error"), Value::Null]);
    (index, name, kind, input, partial, stop, message, error_type, any::<bool>()).prop_map(
        |(index, name, kind, input, partial, stop, message, error_type, broken)| {
            if broken {
                return "{не json".to_owned();
            }
            json!({
                "index": index,
                "content_block": {"type": kind, "name": name, "input": input},
                "delta": {"type": kind, "partial_json": partial, "stop_reason": stop},
                "error": {"type": error_type, "message": message},
            })
            .to_string()
        },
    )
}

fn arb_stream() -> impl Strategy<Value = Vec<u8>> {
    let name = prop::sample::select(vec![
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
        "error",
        "ping",
        "неизвестное",
    ]);
    prop::collection::vec((name, arb_data()), 0..24).prop_map(|events| {
        events
            .into_iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect::<Vec<_>>()
            .concat()
            .into_bytes()
    })
}

fn watch_events(chunks: &[&[u8]]) -> Vec<GatewayEvent> {
    let mut watch = StreamWatch::default();
    let mut events = Vec::new();
    for chunk in chunks {
        watch.feed(chunk, |event| events.push(event));
    }
    events
}

/// Тело ошибки: испорченный JSON с полями, которые смотрит разбор, или любые байты.
fn arb_error_body() -> impl Strategy<Value = Vec<u8>> {
    let kind = field(vec![
        json!("authentication_error"),
        json!("rate_limit_error"),
        json!("overloaded_error"),
        json!("x y"),
        json!(1),
        Value::Null,
    ]);
    let message = field(vec![json!("prompt is too long: 1 tokens"), json!("x"), json!(2), Value::Null]);
    let structured = (kind, message).prop_map(|(kind, message)| {
        json!({"type": "error", "error": {"type": kind, "message": message}})
            .to_string()
            .into_bytes()
    });
    prop_oneof![structured, prop::collection::vec(any::<u8>(), 0..64)]
}

/// Какое событие допустимо у статуса: отказ в учётных данных и 5xx решает
/// статус, остальное — тело.
fn fits_status(status: u16, event: Option<&GatewayEvent>) -> bool {
    use GatewayEvent::{ContextOverflow, RequestRateLimited, ServerError, Unauthorized};
    let kind_fits = match status {
        401 | 403 => matches!(event, Some(Unauthorized { .. })),
        500..=599 => matches!(event, None | Some(ContextOverflow | RequestRateLimited | ServerError { .. })),
        _ => matches!(event, None | Some(ContextOverflow | RequestRateLimited)),
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

/// Заголовки лимита: статус и сбросы окон, в том числе испорченные.
fn arb_limit_headers() -> impl Strategy<Value = HeaderMap> {
    let status = prop::sample::select(vec!["rejected", "allowed", "allowed_warning", "Rejected"]);
    let reset = prop::sample::select(vec!["1790900000", "-1", "1.5", "18446744073709551616", "", "завтра"]);
    let window = prop::sample::select(vec!["5h", "7d", "7d_opus", ""]);
    let windows = prop::collection::vec((window, status.clone(), reset.clone()), 0..4);
    (status, prop::option::of(reset), windows).prop_map(|(status, reset, windows)| {
        let mut headers = HeaderMap::new();
        let mut put = |name: String, value: &str| {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        };
        put(format!("{UNIFIED}status"), status);
        if let Some(reset) = reset {
            put(format!("{UNIFIED}reset"), reset);
        }
        for (window, status, reset) in windows {
            put(format!("{UNIFIED}{window}-status"), status);
            put(format!("{UNIFIED}{window}-reset"), reset);
        }
        headers
    })
}

#[test]
fn limit_headers_decide_between_subscription_and_rate_limit_without_panic() {
    if !slow_tests_enabled() {
        return;
    }
    proptest!(|(headers in arb_limit_headers())| {
        let rejected = headers.get(format!("{UNIFIED}status")).is_some_and(|value| value == "rejected");
        match limit_event(&headers) {
            GatewayEvent::SubscriptionLimitReached { reset } => {
                prop_assert!(rejected, "лимит подписки без rejected");
                prop_assert!(matches!(reset, None | Some(LimitReset::At(_))), "сброс {reset:?}");
            }
            GatewayEvent::RequestRateLimited => prop_assert!(!rejected, "rejected принят за частоту"),
            other => prop_assert!(false, "событие {other:?} у 429"),
        }
    });
}

#[test]
fn malformed_api_events_do_not_panic_and_do_not_depend_on_chunking() {
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
fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            axum::http::HeaderName::from_bytes(format!("{UNIFIED}{name}").as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn usage_windows_come_from_utilization_and_reset_headers_ordered_by_length() {
    let headers = headers_of(&[
        ("status", "allowed"),
        ("7d-utilization", "0.5"),
        ("7d-reset", "1790900000"),
        ("5h-utilization", "0.25"),
        ("5h-reset", "1790800000"),
        ("7d_opus-utilization", "0.9"),
    ]);

    let windows = usage_windows(&headers);

    let window = |duration, scope: Option<&str>, used, resets_at| UsageWindow {
        duration: Some(duration),
        scope: scope.map(str::to_owned),
        used,
        resets_at,
    };
    assert_eq!(
        windows,
        [
            window(18_000, None, 0.25, Some(1_790_800_000)),
            window(604_800, None, 0.5, Some(1_790_900_000)),
            window(604_800, Some("opus"), 0.9, None),
        ]
    );
}

#[test]
fn usage_windows_skip_service_groups_and_unparsable_values() {
    let headers = headers_of(&[
        ("overage-utilization", "0.3"),
        ("grace-5h-utilization", "0.3"),
        ("7d_oi-utilization", "0.3"),
        ("5h-utilization", "много"),
        ("1d-utilization", "NaN"),
        ("0h-utilization", "0.1"),
        ("7d_-utilization", "0.1"),
        ("99999999999999999999d-utilization", "0.1"),
        ("3h-utilization", "1.7"),
    ]);

    let windows = usage_windows(&headers);

    assert_eq!(
        windows.iter().map(|window| (window.duration, window.used)).collect::<Vec<_>>(),
        [(Some(10_800), 1.0)],
        "осталось не одно окно 3h с долей, прижатой к 1"
    );
}

#[test]
fn usage_windows_do_not_panic_and_stay_within_bounds() {
    if !slow_tests_enabled() {
        return;
    }
    let name = prop::sample::select(vec!["5h", "7d", "7d_opus", "x", "", "_", "9h_a_b", "18446744073709551615d"]);
    let value = prop::sample::select(vec!["0.5", "-3", "1e40", "inf", "NaN", "", "1790900000", "завтра"]);
    let suffix = prop::sample::select(vec!["utilization", "reset", "status"]);
    proptest!(|(entries in prop::collection::vec((name, suffix, value), 0..8))| {
        let mut headers = HeaderMap::new();
        for (name, suffix, value) in entries {
            if let Ok(header) = axum::http::HeaderName::from_bytes(format!("{UNIFIED}{name}-{suffix}").as_bytes())
                && let Ok(value) = axum::http::HeaderValue::from_str(value)
            {
                headers.insert(header, value);
            }
        }
        for window in usage_windows(&headers) {
            prop_assert!((0.0..=1.0).contains(&window.used), "доля {} вне 0..1", window.used);
            prop_assert!(window.duration.is_some_and(|seconds| seconds > 0), "длина окна {:?}", window.duration);
        }
    });
}
