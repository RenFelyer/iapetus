//! Прокси из окружения шлюз не берёт. Переменные окружения задаются дочернему процессу — копии тестового
//! бинарника: `set_var` небезопасна рядом с потоками других тестов.

use std::process::Command;

use super::common::*;

const CHILD_FLAG: &str = "IAPETUS_PROXY_ENV_CHILD";
const TEST: &str = "session::gateway::tests::proxy_env::proxy_from_environment_is_not_used";

#[tokio::test(start_paused = true)]
async fn proxy_from_environment_is_not_used() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        gateway_ignores_the_proxy().await;
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", TEST, "--nocapture"]).env(CHILD_FLAG, "1");
    // Порт 9 никто не слушает: запрос через прокси завершился бы ошибкой соединения.
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        child.env(name, "http://127.0.0.1:9");
    }
    let output = child.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "дочерний процесс не прошёл проверку: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn gateway_ignores_the_proxy() {
    let (upstream, recorder) = recording_upstream().await;
    let gateway = spawn_gateway(upstream).await;

    // Через прокси ушёл бы токен Claude Code из заголовков запроса.
    client()
        .post(gateway.url("/v1/messages"))
        .header("x-api-key", "sk-ant-test")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(recorder.count(), 1, "запрос не дошёл до upstream напрямую");
}
