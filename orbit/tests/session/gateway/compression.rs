//! Сжатые ответы upstream: клиент получает их байт в байт, а события хода
//! шлюз берёт из распакованной копии.

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::Response;
use flate2::write::{GzEncoder, ZlibEncoder};
use futures_util::StreamExt;
use proptest::prelude::*;

use super::common::*;
use super::{GatewayEvent, json};

#[derive(Clone, Copy, Debug)]
pub(super) enum Coding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

pub(super) const CODINGS: [Coding; 4] = [Coding::Gzip, Coding::Deflate, Coding::Brotli, Coding::Zstd];

impl Coding {
    pub(super) fn header(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
        }
    }
}

/// Кодировщик, который после каждого куска сбрасывает буфер, как сервер, шлющий SSE.
enum Encoder {
    Gzip(GzEncoder<Vec<u8>>),
    Deflate(ZlibEncoder<Vec<u8>>),
    Brotli(Box<brotli::CompressorWriter<Vec<u8>>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Encoder {
    fn new(coding: Coding) -> Self {
        match coding {
            Coding::Gzip => Self::Gzip(GzEncoder::new(Vec::new(), flate2::Compression::default())),
            Coding::Deflate => Self::Deflate(ZlibEncoder::new(Vec::new(), flate2::Compression::default())),
            Coding::Brotli => Self::Brotli(Box::new(brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22))),
            Coding::Zstd => Self::Zstd(zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap()),
        }
    }

    /// Сжатые байты, готовые после записи `plain` и сброса.
    fn push_flushed(&mut self, plain: &[u8]) -> Vec<u8> {
        fn go<W: std::io::Write>(writer: &mut W, plain: &[u8], out: fn(&mut W) -> &mut Vec<u8>) -> Vec<u8> {
            writer.write_all(plain).unwrap();
            writer.flush().unwrap();
            std::mem::take(out(writer))
        }
        match self {
            Self::Gzip(encoder) => go(encoder, plain, GzEncoder::get_mut),
            Self::Deflate(encoder) => go(encoder, plain, ZlibEncoder::get_mut),
            Self::Brotli(encoder) => go(encoder.as_mut(), plain, brotli::CompressorWriter::get_mut),
            Self::Zstd(encoder) => go(encoder, plain, zstd::stream::write::Encoder::get_mut),
        }
    }

    /// Остаток, который кодировщик отдаёт при закрытии потока.
    fn finish(self) -> Vec<u8> {
        match self {
            Self::Gzip(encoder) => encoder.finish().unwrap(),
            Self::Deflate(encoder) => encoder.finish().unwrap(),
            Self::Brotli(encoder) => encoder.into_inner(),
            Self::Zstd(encoder) => encoder.finish().unwrap(),
        }
    }
}

/// Сжатый поток: по куску на каждое событие и последний кусок с концом потока.
pub(super) fn compress_per_piece(coding: Coding, pieces: &[Piece]) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(coding);
    let mut chunks: Vec<Vec<u8>> = pieces.iter().map(|piece| encoder.push_flushed(piece.text.as_bytes())).collect();
    chunks.push(encoder.finish());
    chunks
}

pub(super) fn compress_whole(coding: Coding, plain: &[u8]) -> Vec<u8> {
    let mut encoder = Encoder::new(coding);
    let mut all = encoder.push_flushed(plain);
    all.extend(encoder.finish());
    all
}

/// Ход с чтением файла: `tool_use` и конец хода.
fn turn() -> Vec<Piece> {
    [
        vec![message_start()],
        tool_block(0, "Read", &json!({"file_path": "/work/a.txt"})),
        message_end("end_turn"),
    ]
    .concat()
}

fn turn_events() -> Vec<GatewayEvent> {
    vec![
        GatewayEvent::ToolUse {
            name: "Read".to_owned(),
            input: json!({"file_path": "/work/a.txt"}),
            path: Some("/work/a.txt".to_owned()),
        },
        GatewayEvent::EndTurn,
    ]
}

fn encoded_sse(coding: Coding, body: axum::body::Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CONTENT_ENCODING, coding.header())
        .body(body)
        .unwrap()
}

/// Прогоняет сжатые куски через шлюз по одному и возвращает то, что получил клиент,
/// и события шлюза.
async fn relay_chunks(coding: Coding, chunks: &[Vec<u8>]) -> (Vec<u8>, Vec<GatewayEvent>) {
    let (tx, body) = channel_body();
    let upstream = spawn_upstream(respond_once(encoded_sse(coding, body))).await;
    let mut gateway = spawn_gateway(upstream).await;
    let response = client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_ENCODING],
        coding.header(),
        "шлюз изменил content-encoding"
    );
    let reader = tokio::spawn(async move { response.bytes().await.unwrap() });
    for chunk in chunks {
        tx.send(Bytes::from(chunk.clone())).await.unwrap();
        settle().await;
    }
    drop(tx);
    let received = reader.await.unwrap();
    (received.to_vec(), gateway.drain_events().await)
}

#[tokio::test(start_paused = true)]
async fn compressed_stream_reaches_the_client_byte_for_byte_and_gives_events() {
    for coding in CODINGS {
        let chunks = compress_per_piece(coding, &turn());
        let (received, events) = relay_chunks(coding, &chunks).await;
        assert_eq!(received, chunks.concat(), "{coding:?}: поток дошёл не байт в байт");
        assert_eq!(events, turn_events(), "{coding:?}: события хода не распознаны");
    }
}

#[tokio::test(start_paused = true)]
async fn events_of_a_compressed_stream_come_before_the_stream_ends() {
    for coding in CODINGS {
        let pieces = turn();
        let chunks = compress_per_piece(coding, &pieces);
        let (tx, body) = channel_body();
        let upstream = spawn_upstream(respond_once(encoded_sse(coding, body))).await;
        let mut gateway = spawn_gateway(upstream).await;
        let response = client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();
        let mut received = response.bytes_stream();

        // Пять кусков: `message_start`, старт блока, две дельты и `content_block_stop`.
        for chunk in &chunks[..5] {
            tx.send(Bytes::from(chunk.clone())).await.unwrap();
        }
        let mut seen = 0;
        let expected: usize = chunks[..5].iter().map(Vec::len).sum();
        while seen < expected {
            seen += received.next().await.unwrap().unwrap().len();
        }
        assert_eq!(
            gateway.drain_events().await,
            &turn_events()[..1],
            "{coding:?}: tool_use ждёт конца потока"
        );
        drop(tx);
    }
}

#[tokio::test(start_paused = true)]
async fn compressed_error_body_is_still_classified() {
    for coding in CODINGS {
        let body =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;
        let compressed = compress_whole(coding, body.as_bytes());
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, coding.header())
            .body(compressed.clone().into())
            .unwrap();
        let upstream = spawn_upstream(respond_once(response)).await;
        let mut gateway = spawn_gateway(upstream).await;

        let response = client().post(gateway.url("/v1/messages")).body(MAIN_REQUEST).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.bytes().await.unwrap(),
            compressed,
            "{coding:?}: ошибка дошла не байт в байт"
        );
        assert_eq!(gateway.drain_events().await, [GatewayEvent::ContextOverflow], "{coding:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn undecodable_stream_still_reaches_the_client_whole() {
    // Заголовок обещает gzip, а в теле мусор: копия не разберётся, клиент об этом не узнает.
    let garbage = b"event: message_start\ndata: {}\n\nnot gzip at all".to_vec();
    let (received, events) = relay_chunks(Coding::Gzip, std::slice::from_ref(&garbage)).await;
    assert_eq!(received, garbage, "мусорное тело искажено");
    assert_none(&events);
}

#[tokio::test(start_paused = true)]
async fn stream_corrupted_midway_still_reaches_the_client_whole() {
    let mut chunks = compress_per_piece(Coding::Gzip, &turn());
    // Порча в середине потока: до неё события есть, после — тело идёт клиенту как есть.
    let broken = chunks.len() - 3;
    chunks[broken] = vec![0xff; 40];
    let (received, _) = relay_chunks(Coding::Gzip, &chunks).await;
    assert_eq!(received, chunks.concat(), "поток после порчи искажён");
}

/// Любое разбиение сжатого потока на куски даёт те же байты у клиента и те же события.
#[test]
fn any_chunking_of_a_compressed_stream_gives_the_same_bytes_and_events() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        return;
    }
    let streams: Vec<(Coding, Vec<u8>)> = CODINGS
        .into_iter()
        .map(|coding| (coding, compress_per_piece(coding, &turn()).concat()))
        .collect();
    proptest!(|(coding in 0..CODINGS.len(), cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..12))| {
        let (coding, stream) = &streams[coding];
        let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(stream.len() + 1)).collect();
        points.sort_unstable();
        let mut chunks = Vec::new();
        let mut start = 0;
        for point in points {
            chunks.push(stream[start..point].to_vec());
            start = point;
        }
        chunks.push(stream[start..].to_vec());
        // Рантайм на каждый случай: задачи шлюза и сокеты закрываются с ним, а не копятся до лимита дескрипторов.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let (received, events) = runtime.block_on(relay_chunks(*coding, &chunks));
        drop(runtime);
        prop_assert_eq!(&received, stream, "{:?}: байты клиента изменены", coding);
        prop_assert_eq!(events, turn_events(), "{:?}: события зависят от разбиения", coding);
    });
}
