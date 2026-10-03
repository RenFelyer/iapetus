//! Разбор потока server-sent events: события из кусков байтов произвольного размера.
//!
//! Результат не зависит от разбиения потока. Конец строки — `\n`, `\r` или `\r\n`; событие без `data` и неполное
//! событие в конце потока отбрасываются.

use super::client::MAX_MESSAGE;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Event {
    /// Поле `event`; без него — `message`.
    pub(super) name: String,
    pub(super) data: String,
}

#[derive(Debug, Default)]
pub(super) struct Events {
    line: Vec<u8>,
    after_cr: bool,
    name: String,
    data: String,
    has_data: bool,
}

impl Events {
    /// Принимает кусок потока и возвращает события, которые он завершил. Строка или событие больше `MAX_MESSAGE` — ошибка.
    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                self.end_line(&mut events);
            } else {
                self.line.push(byte);
            }
            if self.line.len() > MAX_MESSAGE || self.data.len() > MAX_MESSAGE {
                return Err("событие больше предела".to_owned());
            }
        }
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<Event>) {
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
        if line.is_empty() {
            if self.has_data {
                let name = std::mem::take(&mut self.name);
                events.push(Event {
                    name: if name.is_empty() { "message".to_owned() } else { name },
                    data: std::mem::take(&mut self.data),
                });
            }
            self.name.clear();
            self.has_data = false;
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => value.clone_into(&mut self.name),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/mcp/events.rs"]
mod tests;
