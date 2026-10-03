//! Общие края тестов шлюза: поддельный upstream на `127.0.0.1:0`, настоящий
//! шлюз и построители SSE-ответов Messages API.
//!
//! Время в тестах стоит. Часы tokio заморожены (`start_paused = true`) и сверх
//! того лишены автоматического хода: tokio двигает замороженные часы вперёд
//! при каждой остановке рантайма, в том числе пока тест ждёт настоящий сокет, и
//! таймер `connect_timeout` шлюза ловил бы такие скачки. Пока жива задача
//! `spawn_blocking`, автоматический ход выключен, поэтому `FrozenClock` держит
//! такую задачу: время идёт только через `tokio::time::advance`, остальное
//! ожидание — настоящий ввод-вывод. Та же задача — сторожевая: тест, который
//! завис, обрывает процесс с именем теста, вместо вечного ожидания.

use std::convert::Infallible;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::session::gateway::{Api, Config, GatewayEvent, Observation, serve};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures_util::stream;
use nerve::protocol::Activity;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

/// Сколько настоящего времени тест может работать, прежде чем его сочтут зависшим.
const HANG_LIMIT: Duration = Duration::from_secs(20);

/// Выключает автоматический ход часов tokio, пока жив.
pub(crate) struct FrozenClock {
    _done: std::sync::mpsc::Sender<()>,
}

#[expect(clippy::exit, reason = "сторожевая задача обрывает процесс: зависший тест иначе не упал бы")]
pub(crate) fn freeze_clock() -> FrozenClock {
    let test = std::thread::current().name().unwrap_or("тест").to_owned();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        if finished.recv_timeout(HANG_LIMIT) == Err(RecvTimeoutError::Timeout) {
            // Мимо перехвата вывода libtest: иначе сообщение пропадёт вместе с процессом.
            writeln!(std::io::stderr(), "{test}: завис дольше {HANG_LIMIT:?}").unwrap_or_default();
            std::process::exit(1);
        }
    });
    FrozenClock { _done: done }
}

/// Даёт всем готовым задачам дойти до ожидания: шлюз, upstream и клиент живут
/// в одном рантайме, на loopback данные доходят сразу.
pub(crate) async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

pub(crate) struct Gateway {
    _clock: FrozenClock,
    pub(crate) addr: SocketAddr,
    pub(crate) events: broadcast::Receiver<Observation>,
    activity: watch::Receiver<Activity>,
}

impl Gateway {
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Занятость агента, когда шлюз и остальные задачи дошли до ожидания.
    pub(crate) async fn activity(&self) -> Activity {
        settle().await;
        *self.activity.borrow()
    }

    pub(crate) async fn next_event(&mut self) -> GatewayEvent {
        self.events.recv().await.unwrap().event
    }

    /// Все события, которые шлюз успел разослать, пока остальные задачи простаивали.
    pub(crate) async fn drain_events(&mut self) -> Vec<GatewayEvent> {
        self.drain_observations().await.into_iter().map(|seen| seen.event).collect()
    }

    /// То же, что `drain_events`, вместе с видом запроса каждого события.
    pub(crate) async fn drain_observations(&mut self) -> Vec<Observation> {
        settle().await;
        let mut seen = Vec::new();
        while let Ok(observation) = self.events.try_recv() {
            seen.push(observation);
        }
        seen
    }
}

#[track_caller]
pub(crate) fn assert_none(events: &[GatewayEvent]) {
    assert!(events.is_empty(), "лишние события шлюза: {events:?}");
}

pub(crate) async fn spawn_gateway(upstream: String) -> Gateway {
    spawn_gateway_for(Api::Messages, upstream).await
}

pub(crate) async fn spawn_gateway_for(api: Api, upstream: String) -> Gateway {
    let clock = freeze_clock();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, events_rx) = broadcast::channel(4096);
    let config = Config::new(api, &upstream);
    let activity = config.activity.subscribe();
    tokio::spawn(serve(listener, config, events));
    Gateway {
        activity,
        _clock: clock,
        addr,
        events: events_rx,
    }
}

pub(crate) async fn spawn_upstream(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// Адрес, где никто не слушает: соединение отвергается сразу.
pub(crate) async fn dead_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

pub(crate) fn sse(event: &str, data: impl Into<Value>) -> String {
    let data: Value = data.into();
    format!("event: {event}\ndata: {data}\n\n")
}

pub(crate) fn sse_response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap()
}

/// Upstream, который на любой запрос отдаёт заготовленный ответ один раз.
pub(crate) fn respond_once(response: Response) -> Router {
    let slot = Arc::new(Mutex::new(Some(response)));
    Router::new().fallback(move || async move { slot.lock().unwrap().take().unwrap() })
}

/// Тело, которое отдаёт то, что положено в канал, пока отправитель жив.
pub(crate) fn channel_body() -> (mpsc::Sender<Bytes>, Body) {
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    let body = Body::from_stream(stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|chunk| (Ok::<_, Infallible>(chunk), rx))
    }));
    (tx, body)
}

/// Одно событие потока вместе с тем, что шлюз обязан разослать, когда оно
/// пришло целиком.
#[derive(Clone)]
pub(crate) struct Piece {
    pub(crate) text: String,
    pub(crate) emits: Option<GatewayEvent>,
}

pub(crate) fn message_start() -> Piece {
    Piece {
        text: sse(
            "message_start",
            json!({"type": "message_start", "message": {"id": "m", "content": [], "stop_reason": null}}),
        ),
        emits: None,
    }
}

/// Блок `tool_use`: вход приходит двумя кусками `input_json_delta`, посреди JSON.
/// Событие шлюза ждёт `content_block_stop`.
pub(crate) fn tool_block(index: u64, name: &str, input: &Value) -> Vec<Piece> {
    let json = input.to_string();
    let (head, tail) = json.split_at(json.len() / 2);
    let path = match name {
        "Read" | "Edit" | "Write" => input["file_path"].as_str().map(str::to_owned),
        _ => None,
    };
    let delta = |part: &str| Piece {
        text: sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index,
                   "delta": {"type": "input_json_delta", "partial_json": part}}),
        ),
        emits: None,
    };
    vec![
        Piece {
            text: sse(
                "content_block_start",
                json!({"type": "content_block_start", "index": index,
                       "content_block": {"type": "tool_use", "id": "t", "name": name, "input": {}}}),
            ),
            emits: None,
        },
        delta(head),
        delta(tail),
        Piece {
            text: sse("content_block_stop", json!({"type": "content_block_stop", "index": index})),
            emits: Some(GatewayEvent::ToolUse {
                name: name.to_owned(),
                input: input.clone(),
                path,
            }),
        },
    ]
}

/// Конец сообщения: `message_delta` с причиной остановки и `message_stop`.
pub(crate) fn message_end(stop_reason: &str) -> Vec<Piece> {
    vec![
        Piece {
            text: sse(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}}),
            ),
            emits: (stop_reason == "end_turn").then_some(GatewayEvent::EndTurn),
        },
        Piece {
            text: sse("message_stop", json!({"type": "message_stop"})),
            emits: None,
        },
    ]
}

pub(crate) fn text_of(pieces: &[Piece]) -> String {
    pieces.iter().map(|piece| piece.text.as_str()).collect()
}

/// Читаемый дамп событий: по строке на событие, без нестабильного.
pub(crate) fn dump(events: &[GatewayEvent]) -> String {
    events
        .iter()
        .map(|event| match event {
            GatewayEvent::EndTurn => "end_turn".to_owned(),
            GatewayEvent::ToolUse { name, input, path } => {
                let path = path.as_deref().unwrap_or("-");
                format!("tool_use {name} path={path} input={input}")
            }
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Дамп событий с видом запроса: `[вид] событие`.
pub(crate) fn dump_observed(observations: &[Observation]) -> String {
    observations
        .iter()
        .map(|seen| format!("[{:?}] {}", seen.request, dump(std::slice::from_ref(&seen.event))))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Запросы, дошедшие до upstream: метод, путь с запросом, заголовки, тело.
#[derive(Clone, Default)]
pub(crate) struct Recorder(pub Arc<Mutex<Vec<Recorded>>>);

pub(crate) struct Recorded {
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Recorder {
    pub(crate) fn only_request(&self) -> Recorded {
        let mut seen = self.0.lock().unwrap();
        assert_eq!(seen.len(), 1, "до upstream должен дойти ровно один запрос");
        seen.remove(0)
    }

    pub(crate) fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

async fn note(recorder: &Recorder, request: Request) {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    recorder.0.lock().unwrap().push(Recorded {
        headers: parts.headers,
        body: body.to_vec(),
    });
}

pub(crate) fn ok_reply() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-upstream", "1")
        .body(Body::from(r#"{"ok":true}"#))
        .unwrap()
}

/// Приложение, которое записывает каждый запрос и отвечает `reply()`.
pub(crate) fn recording_app(recorder: &Recorder, reply: fn() -> Response) -> Router {
    let recorder = recorder.clone();
    Router::new().fallback(move |request: Request| async move {
        note(&recorder, request).await;
        reply()
    })
}

pub(crate) async fn replying_upstream(reply: fn() -> Response) -> (String, Recorder) {
    let recorder = Recorder::default();
    let upstream = spawn_upstream(recording_app(&recorder, reply)).await;
    (upstream, recorder)
}

pub(crate) async fn recording_upstream() -> (String, Recorder) {
    replying_upstream(ok_reply).await
}

/// Блок `tool_use` без `input_json_delta`: вход целиком в `content_block_start`.
pub(crate) fn tool_block_without_deltas(index: u64, name: &str) -> Vec<Piece> {
    vec![
        Piece {
            text: sse(
                "content_block_start",
                json!({"type": "content_block_start", "index": index,
                       "content_block": {"type": "tool_use", "id": "t", "name": name, "input": {}}}),
            ),
            emits: None,
        },
        Piece {
            text: sse("content_block_stop", json!({"type": "content_block_stop", "index": index})),
            emits: Some(GatewayEvent::ToolUse {
                name: name.to_owned(),
                input: json!({}),
                path: None,
            }),
        },
    ]
}

/// Запрос основного хода Claude Code: с инструментами и без маркеров служебных запросов.
pub(crate) const MAIN_REQUEST: &str = r#"{"model":"m","tools":[{"name":"Read"}],"messages":[{"role":"user","content":"привет"}]}"#;

/// Запрос основного хода Antigravity CLI в обёртке Code Assist.
pub(crate) const GEMINI_MAIN_REQUEST: &str =
    r#"{"requestType":"agent","request":{"contents":[],"tools":[{"functionDeclarations":[{"name":"view_file"}]}]}}"#;
