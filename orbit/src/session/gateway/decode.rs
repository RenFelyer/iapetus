use std::io::{self, Write};

use flate2::write::{MultiGzDecoder, ZlibDecoder};

/// Размер внутреннего буфера распаковщика brotli.
const BROTLI_BUFFER: usize = 16 * 1024;

/// Потоковый распаковщик копии ответа: получает сжатые куски в том порядке, в
/// каком они уходят клиенту, и отдаёт всё, что удалось распаковать.
pub(super) enum Decoder {
    Gzip(MultiGzDecoder<Vec<u8>>),
    /// HTTP `deflate` — это zlib-поток.
    Deflate(ZlibDecoder<Vec<u8>>),
    Brotli(Box<brotli::DecompressorWriter<Vec<u8>>>),
    Zstd(zstd::stream::write::Decoder<'static, Vec<u8>>),
}

/// Что делать с телом ответа по заголовку `Content-Encoding`.
pub(super) enum Encoding {
    Identity,
    Compressed(Box<Decoder>),
    /// Кодировка не поддержана или цепочка из нескольких: тело не разобрать.
    Unsupported,
}

impl Encoding {
    pub(super) fn from_header(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::Identity;
        };
        let coding = value.trim().to_ascii_lowercase();
        let decoder = match coding.as_str() {
            "" | "identity" => return Self::Identity,
            "gzip" | "x-gzip" => Decoder::Gzip(MultiGzDecoder::new(Vec::new())),
            "deflate" => Decoder::Deflate(ZlibDecoder::new(Vec::new())),
            "br" => Decoder::Brotli(Box::new(brotli::DecompressorWriter::new(Vec::new(), BROTLI_BUFFER))),
            "zstd" => match zstd::stream::write::Decoder::new(Vec::new()) {
                Ok(decoder) => Decoder::Zstd(decoder),
                Err(_) => return Self::Unsupported,
            },
            _ => return Self::Unsupported,
        };
        Self::Compressed(Box::new(decoder))
    }
}

impl Decoder {
    /// Скармливает распаковщику сжатый кусок и возвращает распакованное им.
    pub(super) fn push(&mut self, chunk: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Self::Gzip(decoder) => pump(decoder, chunk, MultiGzDecoder::get_mut),
            Self::Deflate(decoder) => pump(decoder, chunk, ZlibDecoder::get_mut),
            Self::Brotli(decoder) => pump(decoder.as_mut(), chunk, brotli::DecompressorWriter::get_mut),
            Self::Zstd(decoder) => pump(decoder, chunk, zstd::stream::write::Decoder::get_mut),
        }
    }
}

/// Пишет кусок в распаковщик, сбрасывает его внутренний буфер и забирает вывод.
fn pump<D: Write>(decoder: &mut D, chunk: &[u8], output: fn(&mut D) -> &mut Vec<u8>) -> io::Result<Vec<u8>> {
    decoder.write_all(chunk)?;
    decoder.flush()?;
    Ok(std::mem::take(output(decoder)))
}
