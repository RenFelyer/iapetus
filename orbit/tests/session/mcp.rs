//! Проектный MCP session: настоящий HTTP на `127.0.0.1:0`, runner — другая сторона `tokio::io::duplex` с мультиплексором.

use std::net::SocketAddr;
use std::sync::Mutex;

use nerve::mux::{Mux, Requests};
use nerve::protocol::{McpPrompt, McpPromptArgument, McpResource, McpTool};
use proptest::prelude::*;

use super::*;

/// Предел полного имени инструмента у агентов.
const NAME_LIMIT: usize = 64;

fn catalog() -> McpCatalog {
    McpCatalog {
        servers: vec![
            McpServer {
                name: "files".to_owned(),
                instructions: Some("Файлы читай через files".to_owned()),
                tools: vec![McpTool {
                    name: "read".to_owned(),
                    description: "читает файл".to_owned(),
                    input_schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#.to_owned(),
                }],
                resources: vec![McpResource {
                    uri: "file:///a b.txt".to_owned(),
                    name: "a".to_owned(),
                    description: "файл a".to_owned(),
                    mime_type: "text/plain".to_owned(),
                }],
                prompts: vec![McpPrompt {
                    name: "review".to_owned(),
                    description: "ревью".to_owned(),
                    arguments: vec![McpPromptArgument {
                        name: "path".to_owned(),
                        description: "путь".to_owned(),
                        required: true,
                    }],
                }],
            },
            McpServer {
                name: "db".to_owned(),
                instructions: None,
                tools: vec![McpTool {
                    name: "query".to_owned(),
                    description: "запрос".to_owned(),
                    input_schema: r#"{"type":"object"}"#.to_owned(),
                }],
                resources: vec![McpResource {
                    uri: "postgres://x/y".to_owned(),
                    name: "y".to_owned(),
                    description: String::new(),
                    mime_type: String::new(),
                }],
                prompts: Vec::new(),
            },
        ],
    }
}

/// Что runner делает с вызовом.
#[derive(Clone)]
enum Answer {
    Reply(McpResponse),
    /// Закрывает канал, не ответив. Перед этим публикует следующий канал данных, где отвечает исправный runner: повтор
    /// вызова после обрыва дошёл бы до него и был бы виден в списке вызовов.
    Hangup,
}

/// Следующий канал данных после обрыва: публикуется перед закрытием прежнего.
struct Successor {
    switch: Arc<watch::Sender<Option<Mux>>>,
    mux: Mux,
}

/// Session с проектным MCP на настоящем сокете и подставным runner.
struct Stand {
    project: Project,
    address: SocketAddr,
    calls: Arc<Mutex<Vec<McpCall>>>,
    client: reqwest::Client,
    /// Канал данных держит runner: пока стенд жив, он открыт.
    _runner: Option<Mux>,
    _switch: Arc<watch::Sender<Option<Mux>>>,
}

impl Stand {
    /// Runner отвечает `answer` на каждый вызов; `None` — канала данных нет.
    async fn start(answer: Option<Answer>) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (link, switch) = Link::fixed(None);
        let switch = Arc::new(switch);
        let runner = answer.map(|answer| {
            let successor = matches!(answer, Answer::Hangup).then(|| Successor {
                switch: Arc::clone(&switch),
                mux: connect(Answer::Reply(Ok("{}".to_owned())), Arc::clone(&calls), None).0,
            });
            let (mux, runner_mux) = connect(answer, Arc::clone(&calls), successor);
            switch.send_replace(Some(mux));
            runner_mux
        });
        let project = Project::new(link, NAME_LIMIT);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, project.clone()));
        Self {
            project,
            address,
            calls,
            client: reqwest::Client::new(),
            _runner: runner,
            _switch: switch,
        }
    }

    async fn post(&self, body: &Value) -> reqwest::Response {
        self.client
            .post(format!("http://{}{PATH}", self.address))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    /// Запрос JSON-RPC и тело ответа.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .post(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }))
            .await;
        assert_eq!(response.status(), 200, "{method}: не 200");
        assert_eq!(
            response.headers()["content-type"],
            "application/json",
            "{method}: ответ не application/json"
        );
        let reply: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(reply["id"], 7, "{method}: в ответе чужой id");
        reply
    }

    fn calls(&self) -> Vec<McpCall> {
        self.calls.lock().unwrap().clone()
    }
}

/// Канал данных к подставному runner: сторона session и сторона runner.
fn connect(answer: Answer, calls: Arc<Mutex<Vec<McpCall>>>, successor: Option<Successor>) -> (Mux, Mux) {
    let (session_side, runner_side) = tokio::io::duplex(1 << 20);
    let (mux, _requests) = Mux::new(session_side);
    let (runner_mux, requests) = Mux::new(runner_side);
    tokio::spawn(run_runner(requests, runner_mux.clone(), answer, calls, successor));
    (mux, runner_mux)
}

async fn run_runner(mut requests: Requests, mux: Mux, answer: Answer, calls: Arc<Mutex<Vec<McpCall>>>, successor: Option<Successor>) {
    while let Some(request) = requests.recv().await {
        let DataRequest::Mcp(call) = protocol::decode_body(request.body()).unwrap() else {
            panic!("runner получил не вызов MCP");
        };
        calls.lock().unwrap().push(call);
        match &answer {
            Answer::Reply(reply) => request.respond(protocol::encode(reply).unwrap().into(), true).await.unwrap(),
            Answer::Hangup => {
                if let Some(successor) = &successor {
                    successor.switch.send_replace(Some(successor.mux.clone()));
                }
                mux.close();
            }
        }
    }
}

fn tool_names(reply: &Value) -> Vec<&str> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

const LOST_TEXT: &str = "связь оборвалась, результат неизвестен";

#[tokio::test]
async fn tools_list_has_server_prefixes_and_schemas() {
    let stand = Stand::start(None).await;
    stand.project.set(catalog());

    let reply = stand.rpc("tools/list", json!({})).await;

    assert_eq!(
        tool_names(&reply),
        ["files__read", "db__query"],
        "инструменты без префиксов сервера"
    );
    assert_eq!(reply["result"]["tools"][0]["description"], "читает файл");
    assert_eq!(
        reply["result"]["tools"][0]["inputSchema"]["required"],
        json!(["path"]),
        "схема входа не дошла до агента"
    );
}

#[tokio::test]
async fn resources_and_prompts_lists_are_prefixed() {
    let stand = Stand::start(None).await;
    stand.project.set(catalog());

    let resources = stand.rpc("resources/list", json!({})).await;
    let uris: Vec<&str> = resources["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris,
        [
            "mcp-project://files/file%3A%2F%2F%2Fa%20b.txt",
            "mcp-project://db/postgres%3A%2F%2Fx%2Fy"
        ],
        "URI ресурсов не переписаны"
    );
    assert_eq!(resources["result"]["resources"][0]["mimeType"], "text/plain");

    let prompts = stand.rpc("prompts/list", json!({})).await;
    assert_eq!(
        prompts["result"]["prompts"][0]["name"], "files__review",
        "prompt без префикса сервера"
    );
    assert_eq!(prompts["result"]["prompts"][0]["arguments"][0]["required"], true);
    assert_eq!(prompts["result"]["prompts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn tool_call_reaches_runner_and_the_result_comes_back_as_is() {
    let result = r#"{"content":[{"type":"text","text":"привет"}],"structuredContent":{"n":12345678901234567890}}"#;
    let stand = Stand::start(Some(Answer::Reply(Ok(result.to_owned())))).await;
    stand.project.set(catalog());

    let response = stand
        .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "files__read", "arguments": { "path": "/a" } } }))
        .await;
    let body = response.text().await.unwrap();

    assert_eq!(
        body,
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#),
        "результат runner изменён по пути к агенту"
    );
    let calls = stand.calls();
    let [McpCall::Tool { server, tool, arguments }] = calls.as_slice() else {
        panic!("runner получил не один вызов инструмента: {calls:?}");
    };
    assert_eq!(
        (server.as_str(), tool.as_str()),
        ("files", "read"),
        "вызов ушёл не тому серверу или инструменту"
    );
    assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), json!({ "path": "/a" }));
}

#[tokio::test]
async fn tool_call_without_arguments_sends_an_empty_object() {
    let stand = Stand::start(Some(Answer::Reply(Ok("{}".to_owned())))).await;
    stand.project.set(catalog());

    stand.rpc("tools/call", json!({ "name": "db__query" })).await;

    let calls = stand.calls();
    let [McpCall::Tool { arguments, .. }] = calls.as_slice() else {
        panic!("нет вызова: {calls:?}");
    };
    assert_eq!(arguments, "{}");
}

#[tokio::test]
async fn runner_error_becomes_an_is_error_result() {
    let stand = Stand::start(Some(Answer::Reply(Err(McpError("сервер упал".to_owned()))))).await;
    stand.project.set(catalog());

    let reply = stand.rpc("tools/call", json!({ "name": "files__read", "arguments": {} })).await;

    assert!(reply.get("error").is_none(), "ошибка инструмента стала ошибкой протокола: {reply}");
    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(reply["result"]["content"][0]["text"], "сервер упал");
}

#[tokio::test]
async fn channel_lost_during_a_call_is_reported_and_the_call_is_not_repeated() {
    let stand = Stand::start(Some(Answer::Hangup)).await;
    stand.project.set(catalog());

    let reply = stand.rpc("tools/call", json!({ "name": "files__read", "arguments": {} })).await;

    assert_eq!(reply["result"]["isError"], true);
    assert_eq!(reply["result"]["content"][0]["text"], LOST_TEXT);
    tokio::task::yield_now().await;
    assert_eq!(stand.calls().len(), 1, "вызов повторён после обрыва");
}

#[tokio::test]
async fn without_a_data_channel_the_call_is_lost() {
    let stand = Stand::start(None).await;
    stand.project.set(catalog());

    let tool = stand.rpc("tools/call", json!({ "name": "files__read", "arguments": {} })).await;
    assert_eq!(tool["result"]["isError"], true);
    assert_eq!(tool["result"]["content"][0]["text"], LOST_TEXT);

    let resource = stand
        .rpc("resources/read", json!({ "uri": "mcp-project://db/postgres%3A%2F%2Fx%2Fy" }))
        .await;
    assert_eq!(
        resource["error"]["message"], LOST_TEXT,
        "обрыв при чтении ресурса не стал ошибкой JSON-RPC"
    );

    let prompt = stand.rpc("prompts/get", json!({ "name": "files__review" })).await;
    assert_eq!(prompt["error"]["message"], LOST_TEXT);
}

#[tokio::test]
async fn unknown_names_are_errors_without_asking_the_runner() {
    let stand = Stand::start(Some(Answer::Reply(Ok("{}".to_owned())))).await;
    stand.project.set(catalog());

    for (method, params) in [
        ("tools/call", json!({ "name": "read" })),
        ("tools/call", json!({ "name": "files__write" })),
        ("tools/call", json!({})),
        ("prompts/get", json!({ "name": "files__missing" })),
        ("resources/read", json!({ "uri": "file:///a b.txt" })),
        ("resources/read", json!({ "uri": "mcp-project://files/missing" })),
        ("resources/read", json!({ "uri": "mcp-project://other/file%3A%2F%2F%2Fa%20b.txt" })),
    ] {
        let reply = stand.rpc(method, params.clone()).await;
        assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{method} {params}: нет ошибки: {reply}");
    }
    tokio::task::yield_now().await;
    assert!(stand.calls().is_empty(), "неизвестное имя дошло до runner: {:?}", stand.calls());
}

#[tokio::test]
async fn resource_read_and_prompt_get_go_to_runner_with_original_names() {
    let stand = Stand::start(Some(Answer::Reply(Ok(r#"{"contents":[]}"#.to_owned())))).await;
    stand.project.set(catalog());

    let read = stand
        .rpc("resources/read", json!({ "uri": "mcp-project://files/file%3A%2F%2F%2Fa%20b.txt" }))
        .await;
    assert_eq!(read["result"], json!({ "contents": [] }));
    let get = stand
        .rpc("prompts/get", json!({ "name": "files__review", "arguments": { "path": "/a" } }))
        .await;
    assert_eq!(get["result"], json!({ "contents": [] }));

    let calls = stand.calls();
    let [
        McpCall::ReadResource { server, uri },
        McpCall::GetPrompt {
            server: prompt_server,
            name,
            arguments,
        },
    ] = calls.as_slice()
    else {
        panic!("вызовы runner не те: {calls:?}");
    };
    assert_eq!(
        (server.as_str(), uri.as_str()),
        ("files", "file:///a b.txt"),
        "URI не разобран обратно"
    );
    assert_eq!((prompt_server.as_str(), name.as_str()), ("files", "review"));
    assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), json!({ "path": "/a" }));
}

#[tokio::test]
async fn resource_read_error_of_a_server_is_a_json_rpc_error() {
    let stand = Stand::start(Some(Answer::Reply(Err(McpError("нет доступа".to_owned()))))).await;
    stand.project.set(catalog());

    let reply = stand
        .rpc("resources/read", json!({ "uri": "mcp-project://db/postgres%3A%2F%2Fx%2Fy" }))
        .await;

    assert_eq!(reply["error"]["message"], "нет доступа");
}

#[tokio::test]
async fn initialize_negotiates_the_version_and_other_methods_behave() {
    let stand = Stand::start(None).await;

    let known = stand.rpc("initialize", json!({ "protocolVersion": "2025-06-18" })).await;
    assert_eq!(
        known["result"]["protocolVersion"], "2025-06-18",
        "поддержанная версия не принята как есть"
    );
    assert_eq!(known["result"]["serverInfo"]["name"], SERVER_NAME);
    for capability in ["tools", "resources", "prompts"] {
        assert!(
            known["result"]["capabilities"].get(capability).is_some(),
            "нет возможности {capability}"
        );
    }
    let unknown = stand.rpc("initialize", json!({ "protocolVersion": "1999-01-01" })).await;
    assert_eq!(
        unknown["result"]["protocolVersion"], "2025-11-25",
        "незнакомая версия не заменена новейшей"
    );

    let initialized = stand
        .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    assert_eq!(initialized.status(), 202, "уведомление не принято с 202");
    assert!(initialized.text().await.unwrap().is_empty());

    assert_eq!(stand.rpc("ping", json!({})).await["result"], json!({}));
    let missing = stand.rpc("completion/complete", json!({})).await;
    assert_eq!(missing["error"]["code"], METHOD_NOT_FOUND);
}

#[tokio::test]
async fn request_with_origin_is_forbidden() {
    let stand = Stand::start(None).await;
    let body = json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" });
    for origin in ["https://evil.example", "http://127.0.0.1", "null"] {
        let response = stand
            .client
            .post(format!("http://{}{PATH}", stand.address))
            .header("content-type", "application/json")
            .header("origin", origin)
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "MCP сессии принял запрос с Origin {origin}");
    }
    assert_eq!(
        stand.rpc("ping", json!({})).await["result"],
        json!({}),
        "запрос без Origin не работает"
    );
}

#[tokio::test]
async fn batch_is_an_invalid_request_with_400() {
    let stand = Stand::start(None).await;
    let response = stand.post(&json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }])).await;
    assert_eq!(response.status(), 400, "пакет не отклонён статусом 400");
    let reply: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(reply["error"]["code"], INVALID_REQUEST, "пакет без кода -32600");
    assert_eq!(reply["id"], Value::Null);
}

#[tokio::test]
async fn new_catalog_replaces_the_old_one_and_empty_removes_it() {
    let stand = Stand::start(Some(Answer::Reply(Ok("{}".to_owned())))).await;
    stand.project.set(catalog());
    let mut single = catalog();
    single.servers.remove(0);
    stand.project.set(single);
    assert_eq!(
        tool_names(&stand.rpc("tools/list", json!({})).await),
        ["db__query"],
        "прежняя сводка не заменена"
    );

    stand.project.set(McpCatalog::default());
    assert!(
        tool_names(&stand.rpc("tools/list", json!({})).await).is_empty(),
        "пустая сводка не сняла инструменты"
    );
    let call = stand.rpc("tools/call", json!({ "name": "db__query" })).await;
    assert_eq!(call["error"]["code"], INVALID_PARAMS, "снятый инструмент ещё вызывается");

    stand.project.set(catalog());
    stand.project.reset();
    assert!(
        tool_names(&stand.rpc("tools/list", json!({})).await).is_empty(),
        "сброс не убрал инструменты"
    );
}

#[tokio::test(start_paused = true)]
async fn waiting_for_the_first_catalog_ends_with_it_or_after_the_limit() {
    let stand = Stand::start(None).await;

    let waiting = tokio::time::timeout(FIRST_CATALOG_WAIT / 2, stand.project.wait_first_catalog()).await;
    assert!(waiting.is_err(), "ожидание закончилось раньше предела без сводки");
    let started = tokio::time::Instant::now();
    stand.project.wait_first_catalog().await;
    assert!(started.elapsed() <= FIRST_CATALOG_WAIT, "ожидание длиннее предела");

    let project = stand.project.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        project.set(McpCatalog::default());
    });
    let started = tokio::time::Instant::now();
    stand.project.wait_first_catalog().await;
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(3),
        "ожидание не закончилось, когда пришла пустая сводка"
    );
}

fn name_of(catalog: &McpCatalog, server: &str, tool: &str) -> String {
    let index = Index::new(catalog.clone(), max_name(NAME_LIMIT));
    index
        .tools
        .iter()
        .find(|found| {
            let found_server = &index.catalog.servers[found.server];
            found_server.name == server && found_server.tools[found.item].name == tool
        })
        .unwrap_or_else(|| panic!("инструмент {server}/{tool} пропал из имён"))
        .name
        .clone()
}

fn single_tool_catalog(pairs: &[(String, String)]) -> McpCatalog {
    let mut servers: Vec<McpServer> = Vec::new();
    for (server, tool) in pairs {
        let mcp_tool = McpTool {
            name: tool.clone(),
            description: String::new(),
            input_schema: "{}".to_owned(),
        };
        match servers.iter_mut().find(|known| &known.name == server) {
            Some(known) => known.tools.push(mcp_tool),
            None => servers.push(McpServer {
                name: server.clone(),
                instructions: None,
                tools: vec![mcp_tool],
                resources: Vec::new(),
                prompts: Vec::new(),
            }),
        }
    }
    McpCatalog { servers }
}

fn valid(name: &str) -> bool {
    name.len() <= max_name(NAME_LIMIT) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn long_and_invalid_names_are_replaced_stably_without_collisions() {
    let long = "x".repeat(80);
    let pairs = [
        ("my.server", "do it"),
        ("my_server", "do_it"),
        ("a", "b__c"),
        ("a__b", "c"),
        ("сервер", "инструмент"),
        ("сервер2", "инструмент"),
        (long.as_str(), "t"),
        (long.as_str(), "u"),
        ("plain", "tool"),
    ]
    .map(|(server, tool)| (server.to_owned(), tool.to_owned()));
    let catalog = single_tool_catalog(&pairs);

    let names: Vec<String> = pairs.iter().map(|(server, tool)| name_of(&catalog, server, tool)).collect();

    for name in &names {
        assert!(valid(name), "недопустимое имя: {name}");
    }
    let unique: std::collections::BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "две разные пары дали одно имя: {names:?}");
    assert_eq!(names[8], "plain__tool", "допустимое короткое имя изменено");
    let again: Vec<String> = pairs
        .iter()
        .map(|(server, tool)| name_of(&single_tool_catalog(&pairs), server, tool))
        .collect();
    assert_eq!(names, again, "имена разошлись между сборками сводки");
    let reversed: Vec<(String, String)> = pairs.iter().rev().cloned().collect();
    let from_reversed: Vec<String> = pairs
        .iter()
        .map(|(server, tool)| name_of(&single_tool_catalog(&reversed), server, tool))
        .collect();
    assert_eq!(names, from_reversed, "имя зависит от порядка серверов в сводке");
    let index = Index::new(catalog, max_name(NAME_LIMIT));
    for ((server, tool), name) in pairs.iter().zip(&names) {
        assert_eq!(
            index.find_tool(name),
            Some((server.clone(), tool.clone())),
            "имя {name} не ведёт к исходной паре"
        );
    }
}

fn part() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!['a', 'b', '_', '-', '.', ' ', 'я']), 1..30)
        .prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    /// Имя годится для Claude Code, не зависит от порядка и однозначно ведёт к исходной паре.
    #[test]
    fn names_are_valid_unique_and_resolve_to_their_pair(pairs in prop::collection::btree_set((part(), part()), 1..12)) {
        let pairs: Vec<(String, String)> = pairs.into_iter().collect();
        let catalog = single_tool_catalog(&pairs);
        let index = Index::new(catalog, max_name(NAME_LIMIT));
        let names: Vec<&str> = index.tools.iter().map(|found| found.name.as_str()).collect();
        prop_assert_eq!(names.len(), pairs.len(), "потеряны или слиты инструменты: {:?}", names);
        for found in &index.tools {
            prop_assert!(valid(&found.name), "недопустимое имя {}", found.name);
            let server = &index.catalog.servers[found.server];
            let expected = Some((server.name.clone(), server.tools[found.item].name.clone()));
            prop_assert_eq!(index.find_tool(&found.name), expected);
        }
    }
}

#[test]
fn encode_and_decode_are_inverse_and_decode_rejects_garbage() {
    for text in ["file:///a b.txt", "日本語/路径?x=1&y=%", "", "a%b"] {
        assert_eq!(decode(&encode(text)).as_deref(), Some(text), "кодирование необратимо для {text}");
    }
    assert_eq!(decode("%zz"), None);
    assert_eq!(decode("%4"), None);
    assert_eq!(decode("%FF"), None, "не UTF-8 принято");
}
