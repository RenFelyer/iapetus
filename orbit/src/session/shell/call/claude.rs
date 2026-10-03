//! Служебные вызовы оболочки Claude Code: `-c env`, `-c -l <скрипт снимка окружения>` и `-c <строка>`, где строка —
//! фиксированный шаблон со вставленной командой и путём файла cwd. Команде без своего ввода Claude Code
//! дописывает `< /dev/null`; команде с `< файл` или heredoc — нет.

use super::{Invocation, UnrecognizedCall, unrecognized};

/// Первая строка скрипта сборки снимка окружения.
const SNAPSHOT_SCRIPT_START: &str = "SNAPSHOT_FILE=";

/// Начало строки команды: `source <снимок>`; путь снимка читается как слово.
const COMMAND_START: &str = "source ";

/// Всё между путём снимка и командой.
const BEFORE_COMMAND: &str = " 2>/dev/null || true && shopt -u extglob 2>/dev/null || true && \
    { \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; } >/dev/null 2>&1 || true && eval ";

/// Всё между командой и путём файла cwd.
const BEFORE_CWD_FILE: &str = " < /dev/null && pwd -P >| ";

/// То же для команды со своим вводом: Claude Code не дописывает ей `< /dev/null`.
const BEFORE_CWD_FILE_OWN_INPUT: &str = " && pwd -P >| ";

pub(super) fn classify(args: &[String]) -> Result<Invocation, UnrecognizedCall> {
    match args {
        [flag, script] if flag == "-c" && script == "env" => Ok(Invocation::Local),
        [flag, login, script] if flag == "-c" && login == "-l" && script.starts_with(SNAPSHOT_SCRIPT_START) => Ok(Invocation::Local),
        [flag, line] if flag == "-c" => parse_command_line(line),
        _ => unrecognized("аргументы не `-c env`, не `-c -l <снимок>` и не `-c <строка команды>`"),
    }
}

fn parse_command_line(line: &str) -> Result<Invocation, UnrecognizedCall> {
    let Some(rest) = line.strip_prefix(COMMAND_START) else {
        return unrecognized("строка не начинается с `source <снимок>`");
    };
    let (_snapshot, rest) = take_word(rest)?;
    let Some(rest) = rest.strip_prefix(BEFORE_COMMAND) else {
        return unrecognized("после пути снимка идёт не тот текст, что ждёт Claude Code 2.1.285");
    };
    let (text, rest) = take_word(rest)?;
    let rest = if let Some(rest) = rest.strip_prefix(BEFORE_CWD_FILE) {
        rest
    } else if let Some(rest) = rest.strip_prefix(BEFORE_CWD_FILE_OWN_INPUT) {
        if !has_own_input(&text) {
            return unrecognized("команда без своего ввода идёт без `< /dev/null`");
        }
        rest
    } else {
        return unrecognized("после команды нет `< /dev/null && pwd -P >| <файл>`");
    };
    let (cwd_file, rest) = take_word(rest)?;
    if !rest.is_empty() {
        return unrecognized("после пути файла cwd есть лишний текст");
    }
    Ok(Invocation::Command {
        text,
        cwd_file: Some(cwd_file),
    })
}

/// Есть ли в команде свой ввод: перенаправление `< файл` или heredoc.
fn has_own_input(command: &str) -> bool {
    has_input_redirect(command) || has_heredoc(command)
}

/// `<` в начале команды или после пробела, `;`, `&`, `|`; за ним не `<` и не `(`, дальше непустой текст.
fn has_input_redirect(command: &str) -> bool {
    let mut previous: Option<char> = None;
    for (index, c) in command.char_indices() {
        let at_boundary = previous.is_none_or(|p| p.is_whitespace() || ";&|".contains(p));
        previous = Some(c);
        if c != '<' || !at_boundary {
            continue;
        }
        let after = &command[index + 1..];
        if !after.starts_with(['<', '(']) && !after.trim_start().is_empty() {
            return true;
        }
    }
    false
}

/// `<<` или `<<-`, затем слово, слово в кавычках или `\слово`. Арифметика (`1 << 2`, `$((a << b))`) не heredoc.
fn has_heredoc(command: &str) -> bool {
    if has_shift_of_digits(command) || command.lines().any(is_arithmetic_shift) {
        return false;
    }
    command.match_indices("<<").any(|(index, _)| {
        let after = &command[index + 2..];
        let after = after.strip_prefix('-').unwrap_or(after).trim_start();
        after.starts_with(|c: char| matches!(c, '\\' | '\'' | '"' | '_') || c.is_alphanumeric())
    })
}

/// Цифра, `<<`, цифра; между ними могут быть пробелы.
fn has_shift_of_digits(command: &str) -> bool {
    command.match_indices("<<").any(|(index, _)| {
        command[..index].trim_end().ends_with(|c: char| c.is_ascii_digit())
            && command[index + 2..].trim_start().starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Строка с `$((`, после него `<<`, после него `))`.
fn is_arithmetic_shift(line: &str) -> bool {
    let Some(start) = line.find("$((") else {
        return false;
    };
    let after_open = &line[start + 3..];
    after_open.find("<<").is_some_and(|shift| after_open[shift + 2..].contains("))"))
}

/// Символы, которые вне кавычек и без `\` оболочка понимает по-своему.
const UNQUOTED_SPECIAL: &str = "\"#$&'()*;<>?[\\]`{|}~!";

/// Читает одно слово оболочки с начала строки и возвращает его значение и
/// остаток после него.
///
/// Слово — склейка кусков без пробелов между ними: простые символы, `\x`,
/// `'…'` и `"…"`. Подстановки (`$`, `` ` ``) в слове — ошибка: значение слова
/// тогда зависело бы от окружения, а не от текста.
fn take_word(input: &str) -> Result<(String, &str), UnrecognizedCall> {
    let mut word = String::new();
    let mut rest = input;
    let mut consumed_any = false;
    while let Some(c) = rest.chars().next() {
        if c.is_ascii_whitespace() {
            break;
        }
        rest = &rest[c.len_utf8()..];
        consumed_any = true;
        match c {
            '\'' => {
                let Some(end) = rest.find('\'') else {
                    return unrecognized("одинарная кавычка не закрыта");
                };
                word.push_str(&rest[..end]);
                rest = &rest[end + 1..];
            }
            '"' => rest = take_double_quoted(rest, &mut word)?,
            '\\' => match rest.chars().next() {
                Some(escaped) if escaped != '\n' => {
                    word.push(escaped);
                    rest = &rest[escaped.len_utf8()..];
                }
                _ => return unrecognized("`\\` в конце слова или перед переводом строки"),
            },
            c if UNQUOTED_SPECIAL.contains(c) => return unrecognized("в слове есть неэкранированный спецсимвол"),
            c => word.push(c),
        }
    }
    if consumed_any {
        Ok((word, rest))
    } else {
        unrecognized("на месте слова пусто")
    }
}

/// Читает содержимое двойных кавычек (открывающая уже прочитана) и возвращает
/// остаток после закрывающей. Как в bash, `\` убирается только перед `"`, `\`,
/// `$` и `` ` ``.
fn take_double_quoted<'a>(mut rest: &'a str, word: &mut String) -> Result<&'a str, UnrecognizedCall> {
    loop {
        let Some(c) = rest.chars().next() else {
            return unrecognized("двойная кавычка не закрыта");
        };
        rest = &rest[c.len_utf8()..];
        match c {
            '"' => return Ok(rest),
            '$' | '`' => return unrecognized("в двойных кавычках есть подстановка"),
            '\\' => match rest.chars().next() {
                Some(escaped @ ('"' | '\\' | '$' | '`')) => {
                    word.push(escaped);
                    rest = &rest[1..];
                }
                Some('\n') | None => return unrecognized("`\\` перед переводом строки или в конце"),
                Some(_) => word.push('\\'),
            },
            c => word.push(c),
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/session/shell/call/claude.rs"]
mod tests;
