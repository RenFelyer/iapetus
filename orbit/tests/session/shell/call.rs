use proptest::prelude::*;

use super::*;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|&item| item.to_owned()).collect()
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

/// Агент без служебных вызовов: строка после `-c` или `-lc` — команда целиком, как бы она ни была записана.
#[test]
fn plain_call_passes_any_command_unchanged() {
    proptest!(|(text in nasty_command(), login: bool)| {
        let flag = if login { "-lc" } else { "-c" };
        prop_assert_eq!(
            classify(Calls::Plain, &args(&[flag, &text])),
            Ok(Invocation::Command { text, cwd_file: None })
        );
    });
}

#[test]
fn arbitrary_arguments_never_panic_in_any_kind_of_calls() {
    proptest!(|(garbage in prop::collection::vec(any::<String>(), 0..4))| {
        drop(classify(Calls::Plain, &garbage));
        drop(classify(Calls::Claude, &garbage));
    });
}

#[test]
fn calls_kind_must_be_set_in_the_environment_and_unknown_values_are_refused() {
    assert!(Calls::from_env(None).is_err(), "оболочка без IAPETUS_SHELL_CALLS не отказала");
    for calls in [Calls::Claude, Calls::Plain] {
        assert_eq!(
            Calls::from_env(Some(calls.value())),
            Ok(calls),
            "значение вида не разбирается обратно"
        );
    }
    for value in ["", "Plain", "claude-code", "plain "] {
        assert!(Calls::from_env(Some(value)).is_err(), "значение `{value}` принято");
    }
}
