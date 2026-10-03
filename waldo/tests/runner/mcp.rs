//! MCP-серверы проекта: настоящие stdio-серверы (скрипт bash), сводка через мультиплексор над `tokio::io::duplex`.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use nerve::mux::Requests;
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::runner::fs::Workdir;

pub(super) const ASSIGNMENT: Assignment = Assignment(1);

/// Значения секретов, которые сервер выдаёт в ответах: из `env` файла и из переменных сессии.
const CONFIG_SECRET: &str = "cfg-secret-value-1";
pub(super) const SESSION_SECRET: &str = "ghp_session_secret_2";

/// Сервер MCP на bash: читает строки, отвечает по `method`. Режим — переменная `MODE`: без неё обычный сервер,
/// `exit` — завершается, ничего не сказав, `silent` — молчит, `hang` — не отвечает на `tools/call`, `probe` — в
/// ответе на `tools/call` приводит ответы клиента на `roots/list` и `sampling/createMessage`. Свой pid сервер
/// пишет в `$PIDFILE`. `LEAK` дописывается к `instructions` и описанию инструмента, `ECHO` и `PWD` выдаются в
/// результате вызова, а на `initialize` сервер пишет в stderr диагностику с `API_TOKEN`.
const SERVER: &str = r#"#!/usr/bin/env bash
[[ -n ${PIDFILE-} ]] && echo $$ > "$PIDFILE"
[[ ${MODE-} == exit ]] && exit 3
[[ ${MODE-} == silent ]] && exec sleep 1000
re='"id":([0-9]+)'
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$1"; }
while IFS= read -r line; do
    [[ $line =~ $re ]] && id=${BASH_REMATCH[1]} || id=
    case $line in
    *'"method":"initialize"'*)
        echo "diagnostics token=$API_TOKEN" >&2
        [[ $line == *'"roots":{}'* && $line != *listChanged* ]] && caps=roots-without-listChanged
        reply "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{},\"resources\":{},\"prompts\":{}},\"serverInfo\":{\"name\":\"fake\",\"version\":\"1\"},\"instructions\":\"use the fake server${LEAK-}\"}" ;;
    *'"method":"tools/list"'*)
        reply "{\"tools\":[{\"name\":\"echo\",\"description\":\"echoes${LEAK-}\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}}}}]}" ;;
    *'"method":"resources/list"'*)
        reply '{"resources":[{"uri":"file:///notes.txt","name":"notes","description":"notes","mimeType":"text/plain"}]}' ;;
    *'"method":"prompts/list"'*)
        reply '{"prompts":[{"name":"greet","description":"greeting","arguments":[{"name":"who","description":"whom","required":true}]}]}' ;;
    *'"method":"tools/call"'*)
        [[ ${MODE-} == hang ]] && continue
        probes=
        if [[ ${MODE-} == probe ]]; then
            echo '{"jsonrpc":"2.0","id":900,"method":"roots/list"}'
            IFS= read -r roots
            echo '{"jsonrpc":"2.0","id":901,"method":"sampling/createMessage","params":{}}'
            IFS= read -r sampling
            probes=" caps=${caps-none} roots=${roots//\"/\\\"} sampling=${sampling//\"/\\\"}"
        fi
        reply "{\"content\":[{\"type\":\"text\",\"text\":\"cfg=$API_TOKEN session=$GITHUB_TOKEN echo=${ECHO-} pwd=$PWD$probes\"}],\"isError\":false}" ;;
    *'"method":"resources/read"'*)
        reply '{"contents":[{"uri":"file:///notes.txt","text":"note body"}]}' ;;
    *'"method":"prompts/get"'*)
        reply '{"messages":[{"role":"user","content":{"type":"text","text":"hello"}}]}' ;;
    esac
done
"#;

/// Приёмник журнала в памяти.
#[derive(Clone, Default)]
pub(super) struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Ждёт, пока в журнале появится `needle`; журнал пишется из фоновых задач.
    pub(super) async fn wait_for(&self, needle: &str) -> String {
        within("запись в журнале", async {
            while !self.text().contains(needle) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        self.text()
    }
}

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

thread_local! {
    /// Приёмник журнала теста, идущего на этом потоке.
    static CURRENT_LOG: RefCell<Option<LogBuffer>> = const { RefCell::new(None) };
}

/// Пишет в приёмник теста своего потока; события потоков без приёмника отбрасываются.
struct ThreadLog;

impl io::Write for ThreadLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        CURRENT_LOG.with(|current| current.borrow_mut().as_mut().map(|log| log.write(bytes)).unwrap_or(Ok(bytes.len())))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for ThreadLog {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        Self
    }
}

/// Снимает приёмник теста с потока.
pub(super) struct LogGuard;

impl Drop for LogGuard {
    fn drop(&mut self) {
        CURRENT_LOG.with(|current| current.borrow_mut().take());
    }
}

/// Журнал теста. Подписчик один на процесс: подписчик на поток (`set_default`) при параллельных тестах теряет
/// события, потому что точка `tracing::debug!`, впервые сработавшая на потоке без подписчика, запоминает отказ.
/// Каждый тест идёт на своём потоке с однопоточной средой, и фоновые задачи пишут в приёмник этого потока.
pub(super) fn capture_log() -> (LogBuffer, LogGuard) {
    static SUBSCRIBER: Once = Once::new();
    SUBSCRIBER.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(ThreadLog)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    let log = LogBuffer::default();
    CURRENT_LOG.with(|current| *current.borrow_mut() = Some(log.clone()));
    (log, LogGuard)
}

pub(super) struct Harness {
    pub(super) dir: tempfile::TempDir,
    pub(super) executor: Arc<Executor>,
    pub(super) mcp: Arc<Mcp>,
    /// Сторона server: запросы runner по каналу управления.
    server: Requests,
    _server_mux: Mux,
}

impl Harness {
    /// Рабочий каталог со скриптом сервера и назначение с секретом сессии; `follow` отправляет сводку в `server`.
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("server.sh"), SERVER).unwrap();
        let workdir = Arc::new(Workdir::open(&fs::canonicalize(dir.path()).unwrap()).unwrap());
        let system = std::env::vars_os().filter(|(name, _)| name == "PATH").collect();
        let executor = Arc::new(Executor::new(workdir, dir.path().join("no-such.env"), system, None));
        executor.start(
            ASSIGNMENT,
            BTreeMap::from([
                ("GITHUB_TOKEN".to_owned(), SESSION_SECRET.to_owned()),
                ("REMOTE_AUTH".to_owned(), HEADER_SECRET.to_owned()),
                ("REMOTE_QUERY".to_owned(), URL_SECRET.to_owned()),
            ]),
        );
        let (near, far) = duplex(1 << 16);
        let (runner_mux, _runner_requests) = Mux::new(near);
        let (server_mux, server) = Mux::new(far);
        let mcp = Arc::new(Mcp::default());
        let follower = Arc::clone(&mcp);
        tokio::spawn(async move { follower.follow(&runner_mux).await });
        Self {
            dir,
            executor,
            mcp,
            server,
            _server_mux: server_mux,
        }
    }

    /// Записывает `.mcp.json`; `servers` — (имя, режим или `None`, имя файла pid).
    pub(super) fn config(&self, servers: &[(&str, Option<&str>)]) {
        let entries: serde_json::Map<String, Value> = servers
            .iter()
            .map(|(name, mode)| {
                let mut env = json!({"API_TOKEN": CONFIG_SECRET, "PIDFILE": self.pid_file(name)});
                if let Some(mode) = mode {
                    env["MODE"] = json!(mode);
                }
                let entry = json!({"command": "bash", "args": ["server.sh"], "env": env});
                ((*name).to_owned(), entry)
            })
            .collect();
        fs::write(self.dir.path().join(".mcp.json"), json!({"mcpServers": entries}).to_string()).unwrap();
    }

    fn pid_file(&self, name: &str) -> String {
        self.dir.path().join(format!("{name}.pid")).display().to_string()
    }

    /// Начинает назначение и возвращает сводку, которую server получил от runner.
    pub(super) async fn start(&mut self) -> McpCatalog {
        self.mcp.start(ASSIGNMENT, &self.executor);
        self.catalog().await
    }

    pub(super) async fn catalog(&mut self) -> McpCatalog {
        let incoming = within("сводка не пришла server", self.server.recv()).await.expect("канал закрыт");
        let RunnerRequest::Mcp(catalog) = protocol::decode_body(incoming.body()).unwrap() else {
            panic!("runner прислал не сводку MCP");
        };
        let ok: Result<(), Error> = Ok(());
        incoming.respond(protocol::encode(&ok).unwrap().into(), true).await.unwrap();
        catalog
    }

    fn pid(&self, name: &str) -> i32 {
        fs::read_to_string(self.pid_file(name)).unwrap().trim().parse().unwrap()
    }
}

pub(super) async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(120), future)
        .await
        .unwrap_or_else(|_| panic!("{what}: не дождались за 120 с"))
}

pub(super) fn is_alive(pid: i32) -> bool {
    // Зомби не жив: статус забирает runner не мгновенно.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().next()) != Some("Z"))
}

pub(super) fn tool_call(server: &str) -> McpCall {
    McpCall::Tool {
        server: server.to_owned(),
        tool: "echo".to_owned(),
        arguments: r#"{"text":"hi"}"#.to_owned(),
    }
}

pub(super) fn names(catalog: &McpCatalog) -> Vec<&str> {
    catalog.servers.iter().map(|server| server.name.as_str()).collect()
}

#[tokio::test]
async fn catalog_lists_what_the_server_declared_and_calls_reach_it_with_secrets_masked() {
    let mut harness = Harness::new();
    harness.config(&[("fake", None)]);
    let catalog = harness.start().await;

    let [server] = catalog.servers.as_slice() else {
        panic!("в сводке не один сервер: {catalog:?}");
    };
    assert_eq!(server.name, "fake");
    assert_eq!(server.instructions.as_deref(), Some("use the fake server"));
    assert_eq!(server.tools.len(), 1);
    assert_eq!(server.tools[0].name, "echo");
    assert!(
        server.tools[0].input_schema.contains(r#""properties""#),
        "схема инструмента потеряна: {}",
        server.tools[0].input_schema
    );
    assert_eq!(server.resources[0].uri, "file:///notes.txt");
    assert_eq!(server.resources[0].mime_type, "text/plain");
    assert_eq!(server.prompts[0].name, "greet");
    assert!(server.prompts[0].arguments[0].required);

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(
        result.contains("cfg=***"),
        "секрет из env файла не замаскирован или не дошёл до сервера: {result}"
    );
    assert!(
        result.contains("session=***"),
        "секрет сессии не замаскирован или не дошёл до сервера: {result}"
    );
    for secret in [CONFIG_SECRET, SESSION_SECRET] {
        assert!(!result.contains(secret), "секрет {secret} попал в результат вызова: {result}");
    }

    let read = McpCall::ReadResource {
        server: "fake".to_owned(),
        uri: "file:///notes.txt".to_owned(),
    };
    assert!(
        within("resources/read", harness.mcp.call(read))
            .await
            .unwrap()
            .contains("note body")
    );
    let prompt = McpCall::GetPrompt {
        server: "fake".to_owned(),
        name: "greet".to_owned(),
        arguments: r#"{"who":"Ada"}"#.to_owned(),
    };
    assert!(within("prompts/get", harness.mcp.call(prompt)).await.unwrap().contains("hello"));

    let unknown = harness.mcp.call(tool_call("other")).await.unwrap_err();
    assert!(unknown.0.contains("other"), "ошибка не называет неизвестный сервер: {unknown:?}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn server_that_does_not_start_or_exits_is_left_out_and_the_others_work() {
    let mut harness = Harness::new();
    harness.config(&[("dead", Some("exit")), ("good", None)]);
    // Нет такой программы и сервер другого вида: ни тот, ни другой в сводку не попадают.
    let path = harness.dir.path().join(".mcp.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["mcpServers"]["missing"] = json!({"command": "no-such-program-for-mcp"});
    config["mcpServers"]["remote"] = json!({"type": "ws", "url": "ws://127.0.0.1:1/mcp"});
    fs::write(&path, config.to_string()).unwrap();

    let catalog = harness.start().await;
    assert_eq!(names(&catalog), ["good"], "в сводке не только исправный сервер");
    within("tools/call", harness.mcp.call(tool_call("good"))).await.unwrap();
    assert!(harness.mcp.call(tool_call("dead")).await.is_err(), "вызов упавшего сервера удался");
    harness.mcp.stop().await;
}

#[tokio::test(start_paused = true)]
async fn server_that_never_answers_is_left_out_after_the_deadline() {
    let mut harness = Harness::new();
    harness.config(&[("silent", Some("silent"))]);
    let catalog = harness.start().await;
    assert!(catalog.servers.is_empty(), "молчащий сервер попал в сводку: {catalog:?}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn missing_config_gives_an_empty_catalog() {
    let mut harness = Harness::new();
    let catalog = harness.start().await;
    assert!(catalog.servers.is_empty(), "без .mcp.json сводка не пуста: {catalog:?}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn end_kills_the_servers_and_cancels_calls_in_flight() {
    let mut harness = Harness::new();
    harness.config(&[("fake", Some("hang"))]);
    harness.start().await;
    let pid = harness.pid("fake");
    assert!(is_alive(pid), "сервер не работает до конца назначения");

    let mcp = Arc::clone(&harness.mcp);
    let call = tokio::spawn(async move { mcp.call(tool_call("fake")).await });
    // Вызов дошёл до сервера и висит.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!call.is_finished(), "вызов к молчащему инструменту уже закончился");

    within("stop", harness.mcp.stop()).await;
    assert!(!is_alive(pid), "процесс сервера жив после конца назначения");
    let result = within("вызов в полёте", call).await.unwrap();
    assert!(result.is_err(), "вызов в полёте не отменён концом назначения: {result:?}");
    let after = harness.mcp.call(tool_call("fake")).await.unwrap_err();
    assert!(after.0.contains("не запущен"), "вызов после конца назначения: {after:?}");
}

#[tokio::test]
async fn start_after_end_runs_the_servers_again() {
    let mut harness = Harness::new();
    harness.config(&[("fake", None)]);
    harness.start().await;
    let first = harness.pid("fake");
    harness.mcp.stop().await;

    let catalog = harness.start().await;
    assert_eq!(names(&catalog), ["fake"]);
    assert_ne!(harness.pid("fake"), first, "сервер нового назначения — тот же процесс");
    assert!(!is_alive(first));
    within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    harness.mcp.stop().await;
}

#[test]
fn arguments_must_be_a_json_object() {
    assert_eq!(object("").unwrap(), json!({}));
    assert_eq!(object(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
    assert!(object("[1]").is_err(), "массив принят как аргументы");
    assert!(object("{").is_err(), "битый JSON принят как аргументы");
}

#[test]
fn secrets_are_masked_in_every_string_of_the_result_even_when_json_escapes_them() {
    let secrets = Secrets::new([b"pa\"ss\nword".to_vec()]);
    let mut value = json!({"pa\"ss\nword": ["x pa\"ss\nword y", 5], "n": null});
    mask_value(&secrets, &mut value);
    assert_eq!(value, json!({"***": ["x *** y", 5], "n": null}));
}
/// Запись `.mcp.json` с одним сервером `fake` на `server.sh`; `entry` дополняет и перекрывает запись.
fn write_fake(harness: &Harness, entry: &Value) {
    let mut server = json!({"command": "bash", "args": ["server.sh"], "env": {"API_TOKEN": CONFIG_SECRET}});
    for (key, value) in entry.as_object().unwrap() {
        server[key] = value.clone();
    }
    fs::write(
        harness.dir.path().join(".mcp.json"),
        json!({"mcpServers": {"fake": server}}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn variables_are_substituted_in_command_args_env_and_cwd_with_defaults() {
    let mut harness = Harness::new();
    fs::create_dir(harness.dir.path().join("sub")).unwrap();
    let path = std::env::var("PATH").unwrap();
    write_fake(
        &harness,
        &json!({
            "command": "${NO_SUCH_COMMAND:-bash}",
            "args": ["${NO_SUCH_SCRIPT:-../server.sh}"],
            "env": {"API_TOKEN": CONFIG_SECRET, "ECHO": format!("p=${o}PATH}}/${o}NO_SUCH_VAR:-fallback}}", o = '{')},
            "cwd": "${NO_SUCH_DIR:-sub}",
        }),
    );
    let catalog = harness.start().await;
    assert_eq!(names(&catalog), ["fake"], "сервер с подстановками не запущен");

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(
        result.contains(&format!("echo=p={path}/fallback")),
        "переменная сессии или значение по умолчанию не подставлены в env: {result}"
    );
    assert!(result.contains("/sub"), "cwd не подставлен: {result}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn unknown_variable_without_default_leaves_the_server_unstarted_and_logs_why() {
    let (log, _guard) = capture_log();
    let mut harness = Harness::new();
    harness.config(&[("good", None)]);
    let path = harness.dir.path().join(".mcp.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["mcpServers"]["broken"] = json!({"command": "bash", "args": ["${NO_SUCH_VAR_FOR_MCP}"]});
    fs::write(&path, config.to_string()).unwrap();

    let catalog = harness.start().await;
    assert_eq!(names(&catalog), ["good"], "сервер с неизвестной переменной запущен");
    assert!(
        log.text().contains("NO_SUCH_VAR_FOR_MCP"),
        "причина не названа в журнале: {}",
        log.text()
    );
    harness.mcp.stop().await;
}

#[tokio::test]
async fn server_stderr_goes_to_the_log_with_secrets_masked() {
    let (log, _guard) = capture_log();
    let mut harness = Harness::new();
    harness.config(&[("fake", None)]);
    harness.start().await;

    let text = log.wait_for("diagnostics").await;
    assert!(
        text.contains("diagnostics token=***"),
        "stderr сервера не в журнале или не замаскирован: {text}"
    );
    assert!(!text.contains(CONFIG_SECRET), "секрет попал в журнал: {text}");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn catalog_masks_secrets_in_instructions_and_descriptions() {
    let mut harness = Harness::new();
    write_fake(
        &harness,
        &json!({"env": {"API_TOKEN": CONFIG_SECRET, "LEAK": format!(" leak={CONFIG_SECRET}")}}),
    );
    let catalog = harness.start().await;

    let [server] = catalog.servers.as_slice() else {
        panic!("в сводке не один сервер: {catalog:?}");
    };
    assert_eq!(server.instructions.as_deref(), Some("use the fake server leak=***"));
    assert_eq!(server.tools[0].description, "echoes leak=***");
    harness.mcp.stop().await;
}

#[tokio::test]
async fn call_without_an_answer_fails_after_the_timeout_and_the_server_stays_alive() {
    let mut harness = Harness::new();
    harness.config(&[("fake", Some("hang"))]);
    harness.start().await;
    let pid = harness.pid("fake");

    // Время замораживается после запуска: ответы настоящего процесса не должны гоняться с таймером.
    tokio::time::pause();
    let mcp = Arc::clone(&harness.mcp);
    let call = tokio::spawn(async move { mcp.call(tool_call("fake")).await });
    tokio::time::sleep(CALL_TIMEOUT + Duration::from_secs(1)).await;
    let error = call.await.unwrap().unwrap_err();
    assert!(
        error.0.contains("не ответил"),
        "вызов без ответа завершился не по таймауту: {error:?}"
    );
    tokio::time::resume();

    assert!(is_alive(pid), "таймаут вызова остановил сервер");
    let read = McpCall::ReadResource {
        server: "fake".to_owned(),
        uri: "file:///notes.txt".to_owned(),
    };
    let result = within("resources/read", harness.mcp.call(read)).await;
    assert!(
        result.is_ok_and(|text| text.contains("note body")),
        "сервер не отвечает после таймаута вызова"
    );
    harness.mcp.stop().await;
}

#[tokio::test]
async fn header_and_url_secrets_do_not_reach_the_catalog_results_errors_or_log() {
    let (log, _guard) = capture_log();
    let fake = Fake::start(Behavior {
        leak: format!(" leak={HEADER_SECRET} {URL_SECRET}"),
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &fake);
    let catalog = harness.start().await;
    assert_eq!(
        catalog.servers[0].instructions.as_deref(),
        Some("use the fake server leak=*** ***"),
        "секрет попал в сводку"
    );
    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(result.contains("key=***"), "секрет из url не замаскирован: {result}");
    for secret in [HEADER_SECRET, URL_SECRET] {
        assert!(!result.contains(secret), "секрет {secret} попал в результат: {result}");
    }
    harness.mcp.stop().await;

    // Ошибка сервера с секретом в тексте.
    let failing = Fake::start(Behavior {
        fail_calls: true,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    write_remote(&harness, "http", &failing);
    harness.start().await;
    let error = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap_err();
    assert!(
        error.0.contains("denied for Bearer ***"),
        "ошибка не дошла или не замаскирована: {error:?}"
    );
    harness.mcp.stop().await;

    // Сервер, к которому нельзя подключиться: ошибка запроса несёт адрес.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let mut harness = Harness::new();
    fs::write(
        harness.dir.path().join(".mcp.json"),
        json!({"mcpServers": {"gone": remote_entry("sse", closed, "sse")}}).to_string(),
    )
    .unwrap();
    assert!(harness.start().await.servers.is_empty());
    let text = log.wait_for("MCP-сервер не подключён").await;
    for secret in [HEADER_SECRET, URL_SECRET] {
        assert!(!text.contains(secret), "секрет {secret} попал в журнал: {text}");
    }
    harness.mcp.stop().await;
}

#[tokio::test]
async fn entries_without_url_or_with_unknown_type_are_skipped_and_stdio_works_beside_remote_ones() {
    let (log, _guard) = capture_log();
    let http = Fake::start(Behavior::default()).await;
    let sse = Fake::start(Behavior {
        mode: Mode::Sse,
        ..Behavior::default()
    })
    .await;
    let mut harness = Harness::new();
    harness.config(&[("a_stdio", None)]);
    let path = harness.dir.path().join(".mcp.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["mcpServers"]["b_http"] = remote_entry("http", http.addr, "mcp");
    config["mcpServers"]["c_nourl"] = json!({"type": "http", "headers": {"X": "y"}});
    config["mcpServers"]["d_sse"] = remote_entry("sse", sse.addr, "sse");
    config["mcpServers"]["e_ws"] = json!({"type": "ws", "url": "ws://127.0.0.1:1/mcp"});
    fs::write(&path, config.to_string()).unwrap();

    let catalog = harness.start().await;
    assert_eq!(
        names(&catalog),
        ["a_stdio", "b_http", "d_sse"],
        "сводка в порядке файла без пропущенных записей"
    );
    let text = log.text();
    assert!(
        text.contains("c_nourl") && text.contains("url"),
        "пропуск записи без url не в журнале: {text}"
    );
    assert!(
        text.contains("e_ws") && text.contains("ws"),
        "пропуск неизвестного типа не в журнале: {text}"
    );
    for server in ["a_stdio", "b_http", "d_sse"] {
        within("tools/call", harness.mcp.call(tool_call(server))).await.unwrap();
    }
    harness.mcp.stop().await;
}

/// считаются секретами, раз подставлены в запись.
pub(super) const HEADER_SECRET: &str = "hdr-secret-value-3";
pub(super) const URL_SECRET: &str = "url-secret-value-4";

pub(super) const SESSION: &str = "sess-1";

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Mode {
    /// Streamable HTTP, ответы JSON.
    #[default]
    Json,
    /// Streamable HTTP: выдаёт `Mcp-Session-Id` и требует его в следующих запросах.
    Session,
    /// Streamable HTTP: отвечает потоком событий, а не JSON.
    Events,
    /// Устаревший SSE: `GET` открывает поток, ответы идут по нему.
    Sse,
}

/// Что сервер отвергает.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Deny {
    #[default]
    Nothing,
    /// Отвечает 401 с `WWW-Authenticate` на всё.
    Everything,
    /// Отвечает 403 на `tools/call`.
    Calls,
}

/// Поведение подставного сервера.
#[derive(Clone, Default)]
pub(super) struct Behavior {
    pub(super) mode: Mode,
    /// Не отвечает на `tools/call`.
    pub(super) hang: bool,
    /// Отвечает на `tools/call` ошибкой, в тексте которой заголовок `Authorization`.
    pub(super) fail_calls: bool,
    /// Дописывается к `instructions`.
    pub(super) leak: String,
    /// Отвечает на всё перенаправлением на этот адрес.
    pub(super) redirect: Option<SocketAddr>,
    /// Заменяет адрес из события `endpoint`.
    pub(super) endpoint: Option<String>,
    pub(super) deny: Deny,
    /// Сколько раз сервер забывает сессию на `tools/call`: отвечает 404 и выдаёт при `initialize` следующую.
    pub(super) expire_sessions: usize,
    /// Перед ответом на `tools/call` спрашивает у клиента `roots/list` (запрос с `id` `r1`).
    pub(super) ask_roots: bool,
}

#[derive(Clone)]
pub(super) struct Seen {
    pub(super) method: String,
    pub(super) target: String,
    headers: HashMap<String, String>,
    pub(super) body: String,
}

impl Seen {
    pub(super) fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }

    /// Метод JSON-RPC из тела запроса.
    pub(super) fn rpc(&self) -> String {
        serde_json::from_str::<Value>(&self.body)
            .ok()
            .and_then(|message| message.get("method").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default()
    }
}

struct Shared {
    behavior: Behavior,
    seen: Mutex<Vec<Seen>>,
    /// Сколько соединений клиент закрыл, пока сервер держал их без ответа.
    dropped: AtomicUsize,
    streams: Mutex<Vec<mpsc::UnboundedSender<String>>>,
    /// Сколько раз сервер забыл сессию; номер текущей сессии — на единицу больше.
    epoch: AtomicUsize,
}

impl Shared {
    fn session(&self) -> String {
        format!("sess-{}", self.epoch.load(Ordering::SeqCst) + 1)
    }
}

pub(super) struct Fake {
    pub(super) addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Fake {
    pub(super) async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            behavior,
            seen: Mutex::default(),
            dropped: AtomicUsize::new(0),
            streams: Mutex::default(),
            epoch: AtomicUsize::new(0),
        });
        let accepting = Arc::clone(&shared);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(Arc::clone(&accepting), socket));
            }
        });
        Self { addr, shared, accept }
    }

    pub(super) fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    pub(super) async fn wait_seen(&self, rpc: &str) {
        within("запрос не дошёл до сервера", async {
            while !self.seen().iter().any(|seen| seen.rpc() == rpc) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    /// Сколько запросов JSON-RPC с методом `rpc` видел сервер.
    pub(super) fn count(&self, rpc: &str) -> usize {
        self.seen().iter().filter(|seen| seen.rpc() == rpc).count()
    }

    pub(super) async fn wait_dropped(&self) {
        within("сервер не увидел закрытия соединения", async {
            while self.shared.dropped.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }
}

fn find(buf: &[u8], pattern: &[u8]) -> Option<usize> {
    buf.windows(pattern.len()).position(|window| window == pattern)
}

async fn read_request(socket: &mut TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = [0; 4096];
    let head_end = loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            break end;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, target) = (request_line.next()?.to_owned(), request_line.next()?.to_owned());
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Seen {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

async fn reply(socket: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").unwrap();
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await.unwrap_or_default();
    socket.write_all(body.as_bytes()).await.unwrap_or_default();
}

async fn serve(shared: Arc<Shared>, mut socket: TcpStream) {
    let Some(seen) = read_request(&mut socket).await else {
        return;
    };
    shared.seen.lock().unwrap().push(seen.clone());
    if let Some(trap) = shared.behavior.redirect {
        let location = format!("http://{trap}/mcp?stolen");
        reply(&mut socket, "307 Temporary Redirect", &[("Location", &location)], "").await;
        return;
    }
    match seen.method.as_str() {
        "DELETE" => reply(&mut socket, "200 OK", &[], "").await,
        "GET" => event_stream(&shared, socket).await,
        _ => post(&shared, socket, &seen).await,
    }
}

/// Ответ JSON-RPC на сообщение клиента; `None` — это уведомление.
fn rpc(behavior: &Behavior, seen: &Seen, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let result = match message.get("method")?.as_str()? {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
            "serverInfo": {"name": "fake", "version": "1"},
            "instructions": format!("use the fake server{}", behavior.leak),
        }),
        "tools/list" => json!({"tools": [{
            "name": "echo",
            "description": "echoes",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
        }]}),
        "resources/list" => json!({"resources": [{
            "uri": "file:///notes.txt", "name": "notes", "description": "notes", "mimeType": "text/plain",
        }]}),
        "prompts/list" => json!({"prompts": [{
            "name": "greet", "description": "greeting",
            "arguments": [{"name": "who", "description": "whom", "required": true}],
        }]}),
        "tools/call" if behavior.fail_calls => {
            let text = format!("denied for {}", seen.header("authorization"));
            return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -1, "message": text}}));
        }
        "tools/call" => {
            let text = format!(
                "auth={} target={} session={} version={}",
                seen.header("authorization"),
                seen.target,
                seen.header("mcp-session-id"),
                seen.header("mcp-protocol-version"),
            );
            json!({"content": [{"type": "text", "text": text}], "isError": false})
        }
        "resources/read" => json!({"contents": [{"uri": "file:///notes.txt", "text": "note body"}]}),
        "prompts/get" => json!({"messages": [{"role": "user", "content": {"type": "text", "text": "hello"}}]}),
        _ => return None,
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

async fn post(shared: &Shared, mut socket: TcpStream, seen: &Seen) {
    let behavior = &shared.behavior;
    let message: Value = serde_json::from_str(&seen.body).unwrap_or_default();
    let method = seen.rpc();
    if behavior.deny == Deny::Everything || (behavior.deny == Deny::Calls && method == "tools/call") {
        let status = if behavior.deny == Deny::Everything {
            "401 Unauthorized"
        } else {
            "403 Forbidden"
        };
        let challenge = format!("Bearer error=\"invalid_token\", received=\"{}\"", seen.header("authorization"));
        reply(&mut socket, status, &[("WWW-Authenticate", &challenge)], "denied").await;
        return;
    }
    let session = shared.session();
    if behavior.mode == Mode::Session && method != "initialize" {
        let presented = seen.header("mcp-session-id");
        if presented.is_empty() {
            reply(&mut socket, "400 Bad Request", &[], "no session").await;
            return;
        }
        let expiring = method == "tools/call" && shared.epoch.load(Ordering::SeqCst) < behavior.expire_sessions;
        if presented != session || expiring {
            if presented == session {
                shared.epoch.fetch_add(1, Ordering::SeqCst);
            }
            reply(&mut socket, "404 Not Found", &[], "unknown session").await;
            return;
        }
    }
    if behavior.hang && method == "tools/call" {
        if behavior.mode == Mode::Sse {
            reply(&mut socket, "202 Accepted", &[], "").await;
            return;
        }
        // Клиент закрыл соединение — чтение вернёт конец потока.
        let mut chunk = [0; 64];
        while socket.read(&mut chunk).await.is_ok_and(|read| read > 0) {}
        shared.dropped.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let Some(answer) = rpc(behavior, seen, &message) else {
        reply(&mut socket, "202 Accepted", &[], "").await;
        return;
    };
    if behavior.mode == Mode::Sse {
        for stream in shared.streams.lock().unwrap().iter() {
            if behavior.ask_roots && method == "tools/call" {
                stream.send(roots_request().to_string()).unwrap_or_default();
            }
            stream.send(answer.to_string()).unwrap_or_default();
        }
        reply(&mut socket, "202 Accepted", &[], "").await;
        return;
    }
    let session = [("Mcp-Session-Id", session.as_str())];
    let headers: &[(&str, &str)] = if behavior.mode == Mode::Session && method == "initialize" {
        &session
    } else {
        &[]
    };
    if behavior.mode == Mode::Events {
        let progress = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
        let roots = if behavior.ask_roots && method == "tools/call" {
            format!("event: message\ndata: {}\n\n", roots_request())
        } else {
            String::new()
        };
        let body = format!("{roots}event: message\ndata: {progress}\n\nevent: message\ndata: {answer}\n\n");
        let mut all = vec![("Content-Type", "text/event-stream")];
        all.extend_from_slice(headers);
        reply(&mut socket, "200 OK", &all, &body).await;
    } else {
        let mut all = vec![("Content-Type", "application/json")];
        all.extend_from_slice(headers);
        reply(&mut socket, "200 OK", &all, &answer.to_string()).await;
    }
}

fn roots_request() -> Value {
    json!({"jsonrpc": "2.0", "id": "r1", "method": "roots/list"})
}

/// Поток событий устаревшего SSE: `endpoint`, затем ответы, пока клиент не закроет соединение.
async fn event_stream(shared: &Shared, mut socket: TcpStream) {
    let endpoint = shared.behavior.endpoint.clone().unwrap_or_else(|| "/messages?sid=1".to_owned());
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    let first = format!("{head}event: endpoint\ndata: {endpoint}\n\n");
    if socket.write_all(first.as_bytes()).await.is_err() {
        return;
    }
    let (events, mut inbox) = mpsc::unbounded_channel::<String>();
    shared.streams.lock().unwrap().push(events);
    let mut chunk = [0; 64];
    loop {
        tokio::select! {
            Some(data) = inbox.recv() => {
                let event = format!("event: message\ndata: {data}\n\n");
                if socket.write_all(event.as_bytes()).await.is_err() {
                    break;
                }
            }
            read = socket.read(&mut chunk) => {
                if read.is_ok_and(|read| read > 0) {
                    continue;
                }
                break;
            }
        }
    }
    shared.dropped.fetch_add(1, Ordering::SeqCst);
}

/// `.mcp.json` с одним сервером `fake` вида `kind` на `fake`; в `url` и `headers` подставляются переменные сессии.
pub(super) fn write_remote(harness: &Harness, kind: &str, fake: &Fake) {
    let path = if kind == "sse" { "sse" } else { "mcp" };
    let entry = remote_entry(kind, fake.addr, path);
    fs::write(
        harness.dir.path().join(".mcp.json"),
        json!({"mcpServers": {"fake": entry}}).to_string(),
    )
    .unwrap();
}

pub(super) fn remote_entry(kind: &str, addr: SocketAddr, path: &str) -> Value {
    json!({
        "type": kind,
        "url": format!("http://{addr}/{path}?key=${{REMOTE_QUERY}}"),
        "headers": {"Authorization": "Bearer ${REMOTE_AUTH}"},
    })
}

/// Сводка и вызовы одного сервера `fake`: то, что должно работать при любом виде подключения.
pub(super) async fn check_catalog_and_calls(harness: &mut Harness) {
    let catalog = harness.start().await;
    let [server] = catalog.servers.as_slice() else {
        panic!("в сводке не один сервер: {catalog:?}");
    };
    assert_eq!(server.name, "fake");
    assert_eq!(server.instructions.as_deref(), Some("use the fake server"));
    assert_eq!(server.tools.len(), 1);
    assert_eq!(server.tools[0].name, "echo");
    assert!(
        server.tools[0].input_schema.contains(r#""properties""#),
        "схема инструмента потеряна: {}",
        server.tools[0].input_schema
    );
    assert_eq!(server.resources[0].uri, "file:///notes.txt");
    assert_eq!(server.resources[0].mime_type, "text/plain");
    assert_eq!(server.prompts[0].name, "greet");
    assert!(server.prompts[0].arguments[0].required);

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    assert!(
        result.contains("auth=Bearer ***"),
        "заголовок не дошёл до сервера или не замаскирован: {result}"
    );
    let read = McpCall::ReadResource {
        server: "fake".to_owned(),
        uri: "file:///notes.txt".to_owned(),
    };
    assert!(
        within("resources/read", harness.mcp.call(read))
            .await
            .unwrap()
            .contains("note body")
    );
    let prompt = McpCall::GetPrompt {
        server: "fake".to_owned(),
        name: "greet".to_owned(),
        arguments: r#"{"who":"Ada"}"#.to_owned(),
    };
    assert!(within("prompts/get", harness.mcp.call(prompt)).await.unwrap().contains("hello"));
}
/// Адрес каталога проекта, как его должен назвать runner.
pub(super) fn project_uri(harness: &Harness) -> String {
    reqwest::Url::from_file_path(fs::canonicalize(harness.dir.path()).unwrap())
        .unwrap()
        .into()
}

#[tokio::test]
async fn stdio_server_asking_for_roots_gets_the_project_dir_and_other_requests_are_refused() {
    let mut harness = Harness::new();
    write_fake(&harness, &json!({"env": {"API_TOKEN": CONFIG_SECRET, "MODE": "probe"}}));
    harness.start().await;

    let result = within("tools/call", harness.mcp.call(tool_call("fake"))).await.unwrap();
    let text = serde_json::from_str::<Value>(&result).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.contains("caps=roots-without-listChanged"),
        "initialize не объявил roots без listChanged: {text}"
    );
    let roots = format!(r#""roots":[{{"uri":"{}"}}]"#, project_uri(&harness));
    assert!(text.contains(&roots), "roots/list ответил не каталогом проекта {roots}: {text}");
    assert!(text.contains(r#""code":-32601"#), "sampling/createMessage не отвергнут: {text}");
    harness.mcp.stop().await;
}
