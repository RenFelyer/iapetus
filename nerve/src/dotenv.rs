//! Разбор файла `.env`.

use std::collections::BTreeMap;

/// Разбирает `.env`: строки `NAME=значение`, необязательное `export `, комментарии `#` и пустые строки. Значение в
/// парных одинарных или двойных кавычках берётся без них; подстановок и escape-последовательностей нет. Строка,
/// которую разобрать нельзя, пропускается.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        vars.insert(name.to_owned(), unquote(value.trim()).to_owned());
    }
    vars
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

#[cfg(test)]
#[path = "../tests/dotenv.rs"]
mod tests;
