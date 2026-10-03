//! Кадры мультиплексора: разбор потока байтов, сборка сообщений и очередь отправки.
//!
//! Кадр — `длина u32 | id u32 | флаги u8 | тело`, числа big-endian, тело не длиннее [`FRAME_BODY`]. Сообщение,
//! которое не помещается в кадр, идёт несколькими кадрами с флагом [`MORE`] на всех, кроме последнего. Очередь
//! отправки берёт по одному кадру от каждого сообщения по кругу, поэтому мелкое сообщение не ждёт большое.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use bytes::{Buf, BufMut, Bytes, BytesMut};

#[cfg(test)]
#[path = "../../tests/mux/frame.rs"]
mod tests;

/// Наибольшее тело одного кадра.
pub const FRAME_BODY: usize = 16 * 1024;

pub(super) const HEADER: usize = 9;

/// Сообщение продолжается в следующем кадре с тем же id.
pub(super) const MORE: u8 = 1;
/// Последнее сообщение в ответ на запрос; у самого запроса флаг тоже стоит.
pub(super) const LAST: u8 = 2;
/// Сообщение — запрос, а не ответ: id запросов и ответов считаются раздельно.
pub(super) const REQUEST: u8 = 4;
const KNOWN: u8 = MORE | LAST | REQUEST;

/// Сколько сообщений одновременно собирается из кадров.
const MAX_PARTIAL: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Frame {
    pub id: u32,
    pub flags: u8,
    pub body: Bytes,
}

/// Поток не разбирается: дальше читать его нельзя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Заявленная длина тела больше [`FRAME_BODY`].
    TooLong(usize),
    UnknownFlags(u8),
    /// Флаги `MORE` и `LAST` вместе: сообщение не может одновременно продолжаться и быть последним.
    MoreAndLast,
    /// Собираемое сообщение длиннее предела.
    MessageTooLarge,
    TooManyPartial,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(len) => write!(f, "тело кадра {len} байт, предел {FRAME_BODY}"),
            Self::UnknownFlags(flags) => write!(f, "неизвестные флаги кадра {flags:#04x}"),
            Self::MoreAndLast => f.write_str("кадр помечен и как продолжение, и как последний"),
            Self::MessageTooLarge => f.write_str("сообщение больше предела"),
            Self::TooManyPartial => f.write_str("слишком много недособранных сообщений"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Режет поток байтов на кадры. Заголовок проверяется, как только он пришёл целиком: заявленная длина больше
/// предела — ошибка, а не ожидание гигабайт.
#[derive(Debug, Default)]
pub(super) struct FrameReader {
    buf: BytesMut,
}

impl FrameReader {
    pub(super) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub(super) fn next(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let id = u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]);
        let flags = self.buf[8];
        if len > FRAME_BODY {
            return Err(FrameError::TooLong(len));
        }
        if flags & !KNOWN != 0 {
            return Err(FrameError::UnknownFlags(flags));
        }
        if flags & MORE != 0 && flags & LAST != 0 {
            return Err(FrameError::MoreAndLast);
        }
        if self.buf.len() < HEADER + len {
            return Ok(None);
        }
        self.buf.advance(HEADER);
        let body = self.buf.split_to(len).freeze();
        Ok(Some(Frame { id, flags, body }))
    }
}

/// Сообщение, собранное из кадров.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Message {
    pub id: u32,
    pub request: bool,
    pub last: bool,
    pub body: Bytes,
}

/// Склеивает кадры одного сообщения; кадры разных сообщений могут перемежаться.
#[derive(Debug)]
pub(super) struct Assembler {
    partial: HashMap<(u32, bool), BytesMut>,
    max_message: usize,
}

impl Assembler {
    pub(super) fn new(max_message: usize) -> Self {
        Self {
            partial: HashMap::new(),
            max_message,
        }
    }

    pub(super) fn add(&mut self, frame: Frame) -> Result<Option<Message>, FrameError> {
        let request = frame.flags & REQUEST != 0;
        let key = (frame.id, request);
        let accumulated = self.partial.get(&key).map_or(0, BytesMut::len);
        if accumulated + frame.body.len() > self.max_message {
            return Err(FrameError::MessageTooLarge);
        }
        if frame.flags & MORE != 0 {
            if !self.partial.contains_key(&key) && self.partial.len() >= MAX_PARTIAL {
                return Err(FrameError::TooManyPartial);
            }
            self.partial.entry(key).or_default().extend_from_slice(&frame.body);
            return Ok(None);
        }
        let body = match self.partial.remove(&key) {
            Some(mut accumulated) => {
                accumulated.extend_from_slice(&frame.body);
                accumulated.freeze()
            }
            None => frame.body,
        };
        Ok(Some(Message {
            id: frame.id,
            request,
            last: frame.flags & LAST != 0,
            body,
        }))
    }
}

/// Сообщение в очереди отправки; `flags` — только [`REQUEST`] и [`LAST`].
#[derive(Debug)]
pub(super) struct Outgoing {
    pub id: u32,
    pub flags: u8,
    pub body: Bytes,
}

/// Сообщения одного id и направления: кадры разных сообщений в нём не смешиваются, иначе получатель собрал бы
/// их в одно.
type Key = (u32, bool);

/// Очередь сообщений, которые режутся на кадры по кругу между id; внутри одного id сообщения идут по порядку.
#[derive(Debug, Default)]
pub(super) struct Scheduler {
    lanes: HashMap<Key, VecDeque<Outgoing>>,
    /// Id с неотправленными сообщениями в порядке очереди.
    order: VecDeque<Key>,
}

impl Scheduler {
    pub(super) fn push(&mut self, message: Outgoing) {
        let key = (message.id, message.flags & REQUEST != 0);
        let lane = self.lanes.entry(key).or_default();
        if lane.is_empty() {
            self.order.push_back(key);
        }
        lane.push_back(message);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Дописывает в `out` по кадру от каждого id очереди, пока не наберётся `limit` байт. Id, до которых не дошла
    /// очередь, остаются впереди, недосланные — уходят в хвост: кадры чередуются по кругу.
    pub(super) fn round(&mut self, out: &mut BytesMut, limit: usize) {
        for _ in 0..self.order.len() {
            let Some(key) = self.order.pop_front() else {
                return;
            };
            let Some(lane) = self.lanes.get_mut(&key) else {
                continue;
            };
            let Some(message) = lane.front_mut() else {
                self.lanes.remove(&key);
                continue;
            };
            let take = message.body.len().min(FRAME_BODY);
            let more = take < message.body.len();
            let part = message.body.split_to(take);
            let flags = if more { message.flags & !LAST | MORE } else { message.flags };
            encode_frame(out, message.id, flags, &part);
            if !more {
                lane.pop_front();
            }
            if lane.is_empty() {
                self.lanes.remove(&key);
            } else {
                self.order.push_back(key);
            }
            if out.len() >= limit {
                return;
            }
        }
    }
}

pub(super) fn encode_frame(out: &mut BytesMut, id: u32, flags: u8, body: &[u8]) {
    debug_assert!(body.len() <= FRAME_BODY, "тело кадра больше предела");
    // Длина не больше FRAME_BODY, в u32 помещается.
    out.put_u32(u32::try_from(body.len()).unwrap_or(u32::MAX));
    out.put_u32(id);
    out.put_u8(flags);
    out.extend_from_slice(body);
}
