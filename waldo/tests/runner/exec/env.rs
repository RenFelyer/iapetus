use super::*;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn system(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

#[track_caller]
fn value_of(resolved: &Resolved, name: &str) -> Option<String> {
    resolved
        .vars
        .get(OsStr::new(name))
        .map(|value| value.to_string_lossy().into_owned())
}

#[test]
fn runner_own_variables_are_not_passed_to_commands() {
    let resolved = Layers {
        system: system(&[("IAPETUS_ORBIT_KEY", "secret-key"), ("HOME", "/home/waldo")]),
        ..Layers::default()
    }
    .resolve();
    assert_eq!(
        value_of(&resolved, "IAPETUS_ORBIT_KEY"),
        None,
        "настройка runner попала в окружение команды"
    );
    assert!(value_of(&resolved, "HOME").is_some(), "обычная системная переменная потеряна");
}

#[test]
fn secrets_come_from_every_layer_by_name() {
    let resolved = Layers {
        system: system(&[("SYS_API_KEY", "system-secret-value")]),
        dotenv: map(&[("ENV_PASSWORD", "dotenv-secret-value")]),
        server: map(&[("SERVER_TOKEN", "server-secret-value")]),
        session: map(&[("GITHUB_TOKEN", "github-secret-value")]),
    }
    .resolve();
    let masked = resolved
        .secrets
        .mask_all(b"system-secret-value dotenv-secret-value server-secret-value github-secret-value");
    assert_eq!(
        String::from_utf8_lossy(&masked),
        "*** *** *** ***",
        "значение секрета из одного из слоёв не маскируется"
    );
}

#[test]
fn overridden_secret_values_are_masked_too() {
    // Перебитое значение команде в окружение не попадает, но лежит там, откуда она может его прочитать: `cat .env`.
    let cases = [
        (
            "`.env` под server",
            Layers {
                dotenv: map(&[("API_KEY", "dotenv-key-value")]),
                server: map(&[("API_KEY", "server-key-value")]),
                ..Layers::default()
            },
            "dotenv-key-value",
        ),
        (
            "`.env` под сессией",
            Layers {
                dotenv: map(&[("GITHUB_TOKEN", "dotenv-github-value")]),
                session: map(&[("GITHUB_TOKEN", "session-github-value")]),
                ..Layers::default()
            },
            "dotenv-github-value",
        ),
        (
            "server под сессией",
            Layers {
                server: map(&[("GITHUB_TOKEN", "server-github-value")]),
                session: map(&[("GITHUB_TOKEN", "session-github-value")]),
                ..Layers::default()
            },
            "server-github-value",
        ),
        (
            "система под `.env`",
            Layers {
                system: system(&[("DB_PASSWORD", "system-password-value")]),
                dotenv: map(&[("DB_PASSWORD", "dotenv-password-value")]),
                ..Layers::default()
            },
            "system-password-value",
        ),
        (
            "переменная runner, которую команде не передают",
            Layers {
                system: system(&[("IAPETUS_ORBIT_KEY", "runner-own-key-value")]),
                ..Layers::default()
            },
            "runner-own-key-value",
        ),
    ];
    for (case, layers, hidden) in cases {
        let masked = layers.resolve().secrets.mask_all(format!("[{hidden}]").as_bytes());
        assert_eq!(
            String::from_utf8_lossy(&masked),
            "[***]",
            "значение секрета не маскируется, если оно не итоговое: {case}"
        );
    }
}

#[test]
fn dotenv_is_parsed_line_by_line() {
    let parsed = nerve::dotenv::parse_dotenv(
        "# комментарий\n\nA=1\nexport B = two \nC=\"quoted value\"\nD='single'\nE=a=b\nbroken line\n=nameless\nF=\"open\n",
    );
    assert_eq!(
        parsed,
        map(&[
            ("A", "1"),
            ("B", "two"),
            ("C", "quoted value"),
            ("D", "single"),
            ("E", "a=b"),
            ("F", "\"open")
        ]),
        "`.env` разобран неверно"
    );
}
