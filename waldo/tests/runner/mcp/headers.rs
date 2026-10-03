//! `headersHelper`: настоящая команда в каталоге проекта, подставной сервер на `127.0.0.1:0`.

use std::fs;
use std::time::Duration;

use serde_json::{Value, json};

use crate::runner::mcp::tests::*;

/// Запись `.mcp.json` с серверами `fake` вида `kind`, у каждого свой `headersHelper`; статический заголовок
/// `Authorization` из `remote_entry` helper перекрывает.
fn write_helpers(harness: &Harness, kind: &str, fake: &Fake, helpers: &[(&str, &str)]) {
    let path = if kind == "sse" { "sse" } else { "mcp" };
    let servers: serde_json::Map<String, Value> = helpers
        .iter()
        .map(|(name, command)| {
            let mut entry = remote_entry(kind, fake.addr, path);
            entry["headers"]["X-Static"] = json!("kept");
            entry["headersHelper"] = json!(command);
            ((*name).to_owned(), entry)
        })
        .collect();
    fs::write(harness.dir.path().join(".mcp.json"), json!({"mcpServers": servers}).to_string()).unwrap();
}

const PRINT_HEADERS: &str = r#"printf '{"Authorization":"Bearer helper-secret-token-9","X-Cwd":"%s","X-Server":"%s","X-Env":"%s"}' "$PWD" "$CLAUDE_CODE_MCP_SERVER_NAME" "$GITHUB_TOKEN""#;

#[tokio::test]
async fn helper_headers_are_added_override_static_ones_and_are_masked() {
    let fake = Fake::start(Behavior::default()).await;
    let mut harness = Harness::new();
    write_helpers(&harness, "http", &fake, &[("fake", PRINT_HEADERS)]);
    assert_eq!(names(&harness.start().await), ["fake"]);

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(
        result.contains("auth=***"),
        "заголовок helper не дошёл или не замаскирован: {result}"
    );
    assert!(
        !result.contains("helper-secret-token-9"),
        "значение от helper попало в результат: {result}"
    );
    harness.mcp.stop().await;

    let seen = fake.seen();
    let call = seen.iter().find(|seen| seen.rpc() == "tools/call").unwrap();
    assert_eq!(
        call.header("authorization"),
        "Bearer helper-secret-token-9",
        "helper не перекрыл headers"
    );
    assert_eq!(call.header("x-static"), "kept", "статический заголовок потерян");
    assert_eq!(call.header("x-server"), "fake", "helper не получил имя сервера");
    assert_eq!(
        call.header("x-cwd"),
        fs::canonicalize(harness.dir.path()).unwrap().display().to_string(),
        "helper запущен не в каталоге проекта"
    );
    assert_eq!(call.header("x-env"), SESSION_SECRET, "helper не получил окружение команд проекта");
}

#[tokio::test]
async fn helper_runs_for_every_connection_including_the_renewed_session() {
    let fake = Fake::start(Behavior {
        mode: Mode::Session,
        expire_sessions: 1,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    let counter = r#"n=$(cat runs 2>/dev/null || echo 0); echo $((n+1)) > runs; printf '{"X-Run":"%s"}' $((n+1))"#;
    write_helpers(&harness, "http", &fake, &[("fake", counter)]);
    harness.start().await;
    within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    harness.mcp.stop().await;

    let seen = fake.seen();
    let runs: Vec<(String, String)> = seen
        .iter()
        .filter(|seen| seen.method == "POST")
        .map(|seen| (seen.rpc(), seen.header("x-run").to_owned()))
        .collect();
    assert_eq!(runs.first().unwrap(), &("initialize".to_owned(), "1".to_owned()));
    let renewed = runs.iter().rposition(|(rpc, _)| rpc == "initialize").unwrap();
    assert_eq!(runs[renewed].1, "2", "helper не запущен заново для новой сессии: {runs:?}");
    assert!(
        runs[renewed..].iter().all(|(_, run)| run == "2"),
        "после новой сессии ушли старые заголовки: {runs:?}"
    );
}

#[tokio::test]
async fn sse_server_gets_the_helper_headers_on_the_stream_and_on_posts() {
    let fake = Fake::start(Behavior {
        mode: Mode::Sse,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_helpers(&harness, "sse", &fake, &[("fake", PRINT_HEADERS)]);
    assert_eq!(names(&harness.start().await), ["fake"]);
    harness.mcp.stop().await;

    let seen = fake.seen();
    assert_eq!(seen[0].method, "GET");
    for request in &seen {
        assert_eq!(
            request.header("authorization"),
            "Bearer helper-secret-token-9",
            "запрос {} {} без заголовка helper",
            request.method,
            request.rpc()
        );
    }
}

#[tokio::test]
async fn failing_helper_leaves_the_server_out_and_its_output_is_masked_in_the_log_and_the_answer() {
    let (log, _guard) = capture_log();
    let fake = Fake::start(Behavior::default()).await;
    let mut harness = Harness::new();
    write_helpers(
        &harness,
        "http",
        &fake,
        &[
            (
                "bad_exit",
                r#"echo "boom $GITHUB_TOKEN" >&2; echo '{"X":"stdout-secret-value"}'; exit 4"#,
            ),
            ("bad_json", "echo 'stdout-secret-value is not json'"),
            ("good", PRINT_HEADERS),
        ],
    );
    let catalog = harness.start().await;
    assert_eq!(names(&catalog), ["good"], "сервер с упавшим helper попал в сводку");

    let text = log.wait_for("bad_json").await;
    assert!(
        text.contains("headersHelper сервера bad_exit: завершилась с") && text.contains("boom ***"),
        "ошибка helper не в журнале или stderr не замаскирован: {text}"
    );
    assert!(
        text.contains("headersHelper сервера bad_json: вывод — не JSON-объект"),
        "ошибка разбора вывода не в журнале: {text}"
    );
    for leaked in [SESSION_SECRET, "stdout-secret-value"] {
        assert!(!text.contains(leaked), "{leaked} попал в журнал: {text}");
    }
    let error = harness.mcp.call(tool_call("bad_exit")).await.unwrap_err();
    assert!(
        error.0.contains("headersHelper сервера bad_exit") && error.0.contains("boom ***") && !error.0.contains(SESSION_SECRET),
        "агент не узнал причину или получил секрет: {error:?}"
    );
    harness.mcp.stop().await;
}

#[tokio::test]
async fn helper_that_does_not_finish_in_time_is_killed_and_the_server_is_left_out() {
    let (log, _guard) = capture_log();
    let fake = Fake::start(Behavior::default()).await;
    let mut harness = Harness::new();
    write_helpers(&harness, "http", &fake, &[("fake", "echo $$ > helper.pid; sleep 1000")]);
    harness.mcp.start(ASSIGNMENT, &harness.executor);
    let pid_file = harness.dir.path().join("helper.pid");
    within("запуск helper", async {
        while fs::read_to_string(&pid_file).is_ok_and(|pid| pid.trim().is_empty()) || !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let pid: i32 = fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    let catalog = harness.catalog().await;
    assert!(catalog.servers.is_empty(), "сервер с зависшим helper попал в сводку: {catalog:?}");
    let text = log.wait_for("не завершилась за 10 с").await;
    assert!(text.contains("headersHelper сервера fake"), "журнал не называет сервер: {text}");
    within("конец helper", async {
        while is_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(fake.seen().is_empty(), "без заголовков клиент всё же пошёл в сеть");
    harness.mcp.stop().await;
}
