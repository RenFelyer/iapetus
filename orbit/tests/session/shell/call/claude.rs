use proptest::prelude::*;

use super::*;

// Части вызова, записанные независимо от констант разбора: так тест
// замечает, если разбор изменили вместе с шаблоном.
const SOURCE: &str = "source ";
const SNAPSHOT: &str = "/home/agent/.claude/shell-snapshots/snapshot-bash-1790822668849-janrtr.sh";
const BEFORE_EVAL: &str = " 2>/dev/null || true && shopt -u extglob 2>/dev/null || true && \
    { \\builtin unalias -- 'unsetenv'; \\builtin unset -f -- 'unsetenv'; } >/dev/null 2>&1 || true && eval ";
const AFTER_EVAL: &str = " < /dev/null && pwd -P >| ";
const CWD_FILE: &str = "/tmp/claude-d6bd-cwd";

fn call_line(word: &str) -> String {
    format!("{SOURCE}{SNAPSHOT}{BEFORE_EVAL}{word}{AFTER_EVAL}{CWD_FILE}")
}

/// Как Claude Code оформляет команду в `eval`: простое слово — как есть,
/// остальное — в одинарных кавычках, а `'` внутри — как `'"'"'`.
fn claude_quote(command: &str) -> String {
    let plain = !command.is_empty() && command.chars().all(|c| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c));
    if plain {
        command.to_owned()
    } else {
        format!("'{}'", command.replace('\'', "'\"'\"'"))
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|&item| item.to_owned()).collect()
}

fn command(text: &str) -> Invocation {
    Invocation::Command {
        text: text.to_owned(),
        cwd_file: Some(CWD_FILE.to_owned()),
    }
}

/// Команды с кавычками, `$`, переводами строки и не-ASCII.
fn nasty_command() -> impl Strategy<Value = String> {
    let special = prop_oneof![
        Just('\''),
        Just('"'),
        Just('\\'),
        Just('$'),
        Just('`'),
        Just('\n'),
        Just(' '),
        Just('é'),
        any::<char>()
    ];
    prop::collection::vec(special, 0..40).prop_map(|chars| chars.into_iter().collect())
}

#[test]
fn any_command_quoted_as_claude_code_does_is_returned_unchanged() {
    proptest!(|(text in nasty_command())| {
        let line = call_line(&claude_quote(&text));
        prop_assert_eq!(classify(&args(&["-c", &line])), Ok(command(&text)));
    });
}

#[test]
fn calls_recorded_from_claude_code_parse() {
    // Слова `eval` сняты с Claude Code 2.1.285: аргумент `-c` после снятия экранирования.
    let recorded = [
        ("'echo $0'", "echo $0"),
        ("'cd /tmp && pwd'", "cd /tmp && pwd"),
        ("pwd", "pwd"),
        (r#"'echo "it'"'"'s"'"#, r#"echo "it's""#),
        (
            r#"'printf '"'"'%s\n'"'"' '"'"'c"d'"'"' \$x "a b"'"#,
            r#"printf '%s\n' 'c"d' \$x "a b""#,
        ),
        ("'echo one\necho two'", "echo one\necho two"),
        ("'echo '\"'\"''\"'\"''\"'\"''\"'\"''", "echo ''''"),
    ];
    for (word, expected) in recorded {
        assert_eq!(
            classify(&args(&["-c", &call_line(word)])),
            Ok(command(expected)),
            "вызов `eval {word}` разобран неверно"
        );
    }
}

#[test]
fn any_deviation_from_the_call_template_is_an_error() {
    let template_parts = [SOURCE, BEFORE_EVAL, AFTER_EVAL];
    for (part_index, part) in template_parts.iter().enumerate() {
        for (position, c) in part.char_indices() {
            let removed = format!("{}{}", &part[..position], &part[position + c.len_utf8()..]);
            let replaced = format!("{}X{}", &part[..position], &part[position + c.len_utf8()..]);
            for broken in [removed, replaced] {
                let mut parts = template_parts.map(str::to_owned);
                parts[part_index] = broken;
                let line = format!("{}{SNAPSHOT}{}'x'{}{CWD_FILE}", parts[0], parts[1], parts[2]);
                assert!(
                    classify(&args(&["-c", &line])).is_err(),
                    "шаблон с порчей в части {part_index}, позиция {position}, принят: {line}"
                );
            }
        }
    }
    let trailing = format!("{} extra", call_line("x"));
    assert!(classify(&args(&["-c", &trailing])).is_err(), "лишний текст после файла cwd принят");
}

#[test]
fn unquoted_shell_syntax_in_the_command_word_is_an_error() {
    // Список записан отдельно от разбора. `\` в нём нет: это не ошибка, а
    // экранирование следующего символа, `a\b` значит `ab`.
    for special in "\"#$&'()*;<>?[]`{|}~!".chars() {
        let line = call_line(&format!("a{special}b"));
        assert!(classify(&args(&["-c", &line])).is_err(), "`{special}` вне кавычек принят в слове");
    }
    for word in [
        "a b",
        "'unclosed",
        "\"unclosed",
        "a\\",
        "\"$HOME\"",
        "\"`id`\"",
        "\"a\\\nb\"",
        "",
        "a\nb",
    ] {
        assert!(classify(&args(&["-c", &call_line(word)])).is_err(), "слово `{word}` принято");
    }
}

#[test]
fn service_calls_are_local_and_everything_else_is_an_error() {
    assert_eq!(classify(&args(&["-c", "env"])), Ok(Invocation::Local), "`-c env` — служебный вызов");
    assert_eq!(
        classify(&args(&[
            "-c",
            "-l",
            "SNAPSHOT_FILE=/home/agent/.claude/shell-snapshots/s.sh\nsource x"
        ])),
        Ok(Invocation::Local),
        "сборка снимка — служебный вызов"
    );
    let unknown: [&[&str]; 8] = [
        &[],
        &["-c"],
        &["-c", "echo hi"],
        &["-c", "env", "extra"],
        &["-c", "-l", "echo hi"],
        &["-l", "-c", "SNAPSHOT_FILE=x"],
        &["-lc", "SNAPSHOT_FILE=x"],
        &["script.sh"],
    ];
    for call in unknown {
        assert!(classify(&args(call)).is_err(), "вызов {call:?} принят, а должен быть ошибкой");
    }
}

fn own_input_line(word: &str) -> String {
    format!("{SOURCE}{SNAPSHOT}{BEFORE_EVAL}{word} && pwd -P >| {CWD_FILE}")
}

#[test]
fn commands_with_their_own_input_come_without_dev_null() {
    // Claude Code 2.1.288 не дописывает `< /dev/null`, если в команде есть `< файл` или heredoc.
    let commands = [
        "cat < /etc/hostname",
        "cat </etc/hostname",
        "wc -l <file; echo done",
        "sort < a | uniq",
        "cat <<EOF\nhi\nEOF",
        "cat <<-'EOF'\nhi\nEOF",
        "cat <<\\EOF\nhi\nEOF",
    ];
    for text in commands {
        assert_eq!(
            classify(&args(&["-c", &own_input_line(&claude_quote(text))])),
            Ok(command(text)),
            "вызов команды со своим вводом `{text}` не разобран"
        );
    }
}

#[test]
fn missing_dev_null_after_a_command_without_own_input_is_an_error() {
    let commands = [
        "echo hi",
        "echo a<b",
        "echo $((1 << 2))",
        "echo 1 << 2",
        "echo x <(true)",
        "echo <",
        "echo <<",
        "echo <<<x",
    ];
    for text in commands {
        assert!(
            classify(&args(&["-c", &own_input_line(&claude_quote(text))])).is_err(),
            "вызов команды `{text}` без `< /dev/null` принят"
        );
    }
}

#[test]
fn arbitrary_arguments_never_panic() {
    proptest!(|(garbage in prop::collection::vec(any::<String>(), 0..4), tail in nasty_command(), cut in 0..60_usize)| {
        drop(classify(&garbage));
        // Слово без разбора кавычек и с ним, обрыв вызова на произвольной границе символа.
        let line = call_line(&tail);
        drop(classify(&args(&["-c", &line])));
        let line = call_line(&claude_quote(&tail));
        let end = line.char_indices().map(|(index, _)| index).rev().nth(cut).unwrap_or(0);
        drop(classify(&args(&["-c", &line[..end]])));
        drop(classify(&args(&["-c", &format!("{SOURCE}{tail}")])));
    });
}
