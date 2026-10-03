//! Разбор пути и запроса: каноничный вид операции и отказ на любом обходе.

use hyper::Method;
use proptest::prelude::*;

use super::super::Denied;
use super::{ContainerOp, Route, Routed, parse};

const ID: &str = "session-1";

fn route(method: &Method, path: &str, query: Option<&str>) -> Result<Route, Denied> {
    parse(method, path, query).map(|routed| routed.route)
}

#[track_caller]
fn assert_denied(method: &Method, path: &str, query: Option<&str>, expected: &Denied) {
    assert_eq!(route(method, path, query).as_ref(), Err(expected), "{method} {path} {query:?}");
}

fn container(op: ContainerOp, query: &[(&'static str, &str)]) -> Route {
    Route::Container {
        id: ID.to_owned(),
        op,
        query: query.iter().map(|(key, value)| (*key, (*value).to_owned())).collect(),
    }
}

#[test]
fn allowed_operations_are_recognized() {
    let cases: Vec<(Method, &str, Option<&str>, Route)> = vec![
        (Method::GET, "/_ping", None, Route::Ping),
        (
            Method::POST,
            "/containers/create",
            Some("name=session-1"),
            Route::ContainerCreate { name: ID.to_owned() },
        ),
        (Method::GET, "/containers/json", None, Route::ContainerList { all: false }),
        (
            Method::GET,
            "/containers/json",
            Some("all=true"),
            Route::ContainerList { all: true },
        ),
        (
            Method::GET,
            "/containers/session-1/json",
            None,
            container(ContainerOp::Inspect, &[]),
        ),
        (
            Method::POST,
            "/containers/session-1/start",
            None,
            container(ContainerOp::Start, &[]),
        ),
        (
            Method::POST,
            "/containers/session-1/stop",
            Some("t=10"),
            container(ContainerOp::Stop, &[("t", "10")]),
        ),
        (
            Method::DELETE,
            "/containers/session-1",
            Some("force=true&v=true"),
            container(ContainerOp::Remove, &[("force", "true"), ("v", "true")]),
        ),
        (
            Method::GET,
            "/containers/session-1/logs",
            Some("tail=50&stdout=true"),
            container(ContainerOp::Logs, &[("stdout", "true"), ("tail", "50")]),
        ),
        (
            Method::POST,
            "/containers/session-1/wait",
            Some("condition=not-running"),
            container(ContainerOp::Wait, &[("condition", "not-running")]),
        ),
        (Method::POST, "/volumes/create", None, Route::VolumeCreate),
        (
            Method::GET,
            "/volumes/claude-vol",
            None,
            Route::VolumeInspect {
                name: "claude-vol".to_owned(),
            },
        ),
        (
            Method::DELETE,
            "/volumes/claude-vol",
            None,
            Route::VolumeRemove {
                name: "claude-vol".to_owned(),
            },
        ),
    ];
    for (method, path, query, expected) in cases {
        assert_eq!(route(&method, path, query), Ok(expected), "{method} {path}");
    }
}

#[test]
fn api_version_prefix_is_kept_when_supported() {
    let routed = parse(&Method::GET, "/v1.45/containers/json", None).unwrap();
    assert_eq!(
        routed,
        Routed {
            prefix: "/v1.45".to_owned(),
            route: Route::ContainerList { all: false }
        }
    );
    assert_eq!(parse(&Method::GET, "/containers/json", None).unwrap().prefix, "");
}

#[test]
fn unsupported_api_versions_are_denied() {
    for version in ["v1.40", "v1.61", "v1.041", "v1.4x", "v1.", "v1.-1", "v1.99999999999", "v1.45.1"] {
        assert_denied(&Method::GET, &format!("/{version}/containers/json"), None, &Denied::Version);
    }
    for version in ["v2.41", "v1", "V1.45"] {
        assert_denied(&Method::GET, &format!("/{version}/containers/json"), None, &Denied::Endpoint);
    }
    assert_denied(&Method::GET, "/v1.45/v1.45/containers/json", None, &Denied::Endpoint);
}

#[test]
fn path_that_is_not_canonical_matches_nothing() {
    for path in [
        "/containers/%63reate",
        "/containers%2Fcreate",
        "/Containers/create",
        "/containers/CREATE",
        "//containers/create",
        "/containers//create",
        "/containers/create/",
        "/containers/./create",
        "/containers/session-1/../create",
        "/v1.45/../containers/create",
        "/containers/create%00",
        "/containers/create;x",
        "containers/create",
        "/",
        "",
    ] {
        assert_denied(&Method::POST, path, Some("name=a"), &Denied::Endpoint);
    }
}

#[test]
fn container_names_with_unsafe_characters_are_denied() {
    for id in ["%2e%2e", "..", ".", "a%2Fb", "a b", "-x", "a\\b", "a:b", "ключ", ""] {
        let path = format!("/containers/{id}/json");
        let result = route(&Method::GET, &path, None);
        assert!(matches!(result, Err(Denied::Name | Denied::Endpoint)), "{id:?}: {result:?}");
    }
    assert_denied(&Method::GET, "/containers/%2e%2e/json", None, &Denied::Name);
    assert_denied(&Method::GET, &format!("/containers/{}/json", "a".repeat(129)), None, &Denied::Name);
    assert_denied(
        &Method::POST,
        "/containers/create",
        Some("name=a%2Fb"),
        &Denied::Query("value of `name` has unsafe characters".to_owned()),
    );
    assert_denied(&Method::GET, "/volumes/..", None, &Denied::Name);
    for name in ["..", ".x", "-x", "_x"] {
        assert_denied(&Method::POST, "/containers/create", Some(&format!("name={name}")), &Denied::Name);
    }
}

#[test]
fn method_must_match_exactly() {
    for (method, path) in [
        ("get", "/containers/json"),
        ("Get", "/_ping"),
        ("post", "/containers/create"),
        ("HEAD", "/_ping"),
        ("PUT", "/containers/session-1/start"),
        ("PATCH", "/containers/session-1"),
        ("GET", "/containers/session-1/start"),
        ("POST", "/containers/session-1/json"),
        ("DELETE", "/containers/session-1/stop"),
    ] {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        assert_denied(&method, path, Some("name=a"), &Denied::Endpoint);
    }
}

#[test]
fn query_outside_the_allowed_list_is_denied() {
    let unknown = |key: &str| Denied::Query(format!("parameter `{key}` is not allowed"));
    assert_denied(
        &Method::POST,
        "/containers/create",
        Some("name=a&platform=linux"),
        &unknown("platform"),
    );
    assert_denied(&Method::POST, "/containers/create", Some("Name=a"), &unknown("Name"));
    assert_denied(&Method::DELETE, "/containers/session-1", Some("link=true"), &unknown("link"));
    assert_denied(&Method::POST, "/containers/session-1/stop", Some("signal=KILL"), &unknown("signal"));
    assert_denied(
        &Method::POST,
        "/containers/session-1/start",
        Some("detachKeys=x"),
        &unknown("detachKeys"),
    );
    assert_denied(&Method::GET, "/containers/json", Some("filters=x"), &unknown("filters"));
    assert_denied(&Method::GET, "/containers/json", Some("limit=1"), &unknown("limit"));
    assert_denied(&Method::GET, "/containers/session-1/logs", Some("until=1"), &unknown("until"));
    assert_denied(&Method::GET, "/volumes/v", Some("force=true"), &unknown("force"));
}

#[test]
fn malformed_query_is_denied() {
    for query in [
        "name=a&name=b",
        "name=a&",
        "&name=a",
        "name",
        "name=",
        "name=a b",
        "name=a+b",
        "name=a%20b",
        "name=a;b",
        "name==a",
        "",
    ] {
        let result = route(&Method::POST, "/containers/create", Some(query));
        assert!(matches!(result, Err(Denied::Query(_))), "{query:?}: {result:?}");
    }
    assert!(matches!(route(&Method::POST, "/containers/create", None), Err(Denied::Query(_))));
}

#[test]
fn query_values_are_canonical() {
    for (path, query) in [
        ("/containers/json", "all=1"),
        ("/containers/json", "all=TRUE"),
        ("/containers/session-1/stop", "t=-1"),
        ("/containers/session-1/stop", "t=010"),
        ("/containers/session-1/stop", "t=601"),
        ("/containers/session-1/stop", "t=99999999999"),
        ("/containers/session-1/logs", "tail=-5"),
        ("/containers/session-1/logs", "tail=ALL"),
        ("/containers/session-1/logs", "follow=yes"),
        ("/containers/session-1/wait", "condition=other"),
    ] {
        let method = if path.ends_with("json") || path.ends_with("logs") {
            Method::GET
        } else {
            Method::POST
        };
        let result = route(&method, path, Some(query));
        assert!(matches!(result, Err(Denied::Query(_))), "{path}?{query}: {result:?}");
    }
}

#[test]
fn query_is_rebuilt_not_forwarded() {
    // Порядок и вид значений каноничны, даже если клиент прислал иначе.
    let Ok(Route::Container { query, .. }) = route(
        &Method::GET,
        "/containers/session-1/logs",
        Some("timestamps=true&follow=false&tail=all"),
    ) else {
        panic!("logs не разобрались");
    };
    let rendered = super::render_query(&query);
    assert_eq!(rendered, "?follow=false&timestamps=true&tail=all");
}

#[test]
fn endpoints_outside_the_list_are_denied() {
    for (method, path) in [
        (Method::POST, "/containers/session-1/exec"),
        (Method::POST, "/exec/abc/start"),
        (Method::GET, "/exec/abc/json"),
        (Method::POST, "/containers/session-1/attach"),
        (Method::GET, "/containers/session-1/attach/ws"),
        (Method::GET, "/containers/session-1/archive"),
        (Method::PUT, "/containers/session-1/archive"),
        (Method::POST, "/build"),
        (Method::POST, "/commit"),
        (Method::POST, "/containers/session-1/kill"),
        (Method::POST, "/containers/session-1/restart"),
        (Method::POST, "/containers/session-1/update"),
        (Method::POST, "/containers/session-1/rename"),
        (Method::POST, "/containers/session-1/pause"),
        (Method::POST, "/containers/session-1/resize"),
        (Method::GET, "/containers/session-1/top"),
        (Method::GET, "/containers/session-1/changes"),
        (Method::GET, "/containers/session-1/export"),
        (Method::GET, "/containers/session-1/stats"),
        (Method::POST, "/containers/prune"),
        (Method::POST, "/volumes/prune"),
        (Method::GET, "/volumes"),
        (Method::POST, "/images/create"),
        (Method::GET, "/images/json"),
        (Method::POST, "/networks/create"),
        (Method::GET, "/networks"),
        (Method::GET, "/info"),
        (Method::GET, "/version"),
        (Method::GET, "/events"),
        (Method::GET, "/system/df"),
        (Method::POST, "/auth"),
        (Method::GET, "/swarm"),
        (Method::GET, "/plugins"),
    ] {
        assert_denied(&method, path, None, &Denied::Endpoint);
    }
}

proptest! {
    #[test]
    fn arbitrary_requests_never_panic(
        method in "[A-Za-z]{1,8}",
        path in "\\PC{0,60}",
        query in proptest::option::of("\\PC{0,40}"),
    ) {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        // Результат любой, важно, что разбор не паникует и не зависает.
        drop(parse(&method, &path, query.as_deref()));
    }

    /// Допущенная операция всегда в каноничном виде: имя безопасно, пути нет ни слэшей, ни процентов.
    #[test]
    fn allowed_routes_carry_only_safe_names(path in "/(v1\\.[0-9]{2}/)?[a-z_]{1,10}/[A-Za-z0-9._%/-]{0,20}(/[a-z]{1,6})?") {
        if let Ok(Routed { route: Route::Container { id, .. } | Route::VolumeInspect { name: id } | Route::VolumeRemove { name: id } | Route::ContainerCreate { name: id }, .. }) =
            parse(&Method::GET, &path, None)
        {
            prop_assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')), "небезопасное имя {id:?}");
            prop_assert!(!id.starts_with(['.', '-', '_']), "имя начинается со служебного символа {id:?}");
        }
    }
}
