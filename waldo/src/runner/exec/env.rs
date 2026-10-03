//! Окружение команды: переменные системы ← `.env` ← server ← переменные сессии; поздний слой сильнее раннего.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};

use super::mask::{Secrets, is_secret_name};

/// Переменные с этим префиксом настраивают сам runner (адрес и ключ доступа к server): командам они не нужны.
const RUNNER_PREFIX: &str = "IAPETUS_";

/// Слои окружения, от слабого к сильному.
#[derive(Debug, Default)]
pub(super) struct Layers {
    pub(super) system: Vec<(OsString, OsString)>,
    pub(super) dotenv: BTreeMap<String, String>,
    pub(super) server: BTreeMap<String, String>,
    pub(super) session: BTreeMap<String, String>,
}

/// Окружение, с которым запускается команда, и секреты для маскирования вывода.
#[derive(Debug)]
pub(in crate::runner) struct Resolved {
    pub(in crate::runner) vars: BTreeMap<OsString, OsString>,
    pub(in crate::runner) secrets: Secrets,
}

/// Имя и значение, которые можно передать процессу: пустое имя, `=` и NUL в нём или NUL в значении запустить не дадут.
fn is_valid(name: &OsStr, value: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    !bytes.is_empty() && !bytes.contains(&b'=') && !bytes.contains(&0) && !value.as_encoded_bytes().contains(&0)
}

impl Layers {
    /// Сливает слои. Маскируются значения секретов (см. [`is_secret_name`]) из всех слоёв, а не только итоговые:
    /// значение, которое перебил более сильный слой, команде в окружение не попадает, но она может прочитать его
    /// там, где оно лежит, — `cat .env`, `/proc/<pid runner>/environ`. По той же причине маскируются и значения,
    /// отброшенные при слиянии: переменные runner и имена, которые процессу не передать.
    pub(super) fn resolve(self) -> Resolved {
        let mut vars: BTreeMap<OsString, OsString> = BTreeMap::new();
        let mut secrets: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut remember = |name: &OsStr, value: &OsStr| {
            if name.to_str().is_some_and(is_secret_name) {
                secrets.insert(value.as_encoded_bytes().to_vec());
            }
        };
        for (name, value) in self.system {
            remember(&name, &value);
            if !name.to_string_lossy().starts_with(RUNNER_PREFIX) && is_valid(&name, &value) {
                vars.insert(name, value);
            }
        }
        for layer in [self.dotenv, self.server, self.session] {
            for (name, value) in layer {
                let (name, value) = (OsString::from(name), OsString::from(value));
                remember(&name, &value);
                if is_valid(&name, &value) {
                    vars.insert(name, value);
                }
            }
        }
        Resolved {
            vars,
            secrets: Secrets::new(secrets),
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/exec/env.rs"]
mod tests;
