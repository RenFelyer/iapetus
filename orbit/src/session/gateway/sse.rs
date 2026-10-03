/// Инкрементальный разбор Server-Sent Events: события отдаются по мере прихода
/// байт, в памяти держится только неоконченная строка и поля текущего события.
#[derive(Debug, Default)]
pub(super) struct SseParser {
    line: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    /// Разбирает очередной кусок потока; для каждого законченного события
    /// вызывает `on_event(имя, данные)`. Имя пусто, если `event:` не было.
    pub(super) fn feed(&mut self, chunk: &[u8], mut on_event: impl FnMut(&str, &str)) {
        for &byte in chunk {
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            self.handle_line(&mut on_event);
            self.line.clear();
        }
    }

    fn handle_line(&mut self, on_event: &mut impl FnMut(&str, &str)) {
        let line = String::from_utf8_lossy(&self.line);
        if line.is_empty() {
            if !self.data.is_empty() || !self.event.is_empty() {
                on_event(&self.event, &self.data);
            }
            self.event.clear();
            self.data.clear();
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_owned(),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/session/gateway/sse.rs"]
mod tests;
