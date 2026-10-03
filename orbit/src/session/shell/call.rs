//! Разбор вызова оболочки: что именно просит сделать агент.
//!
//! Вид вызова задаёт профиль агента. Агент с шаблонами служебных вызовов разбирается своим разборщиком (`claude`):
//! всё, что не совпало с шаблоном целиком, — ошибка, потому что выполнить непонятную строку локально значило бы
//! запустить команду агента в контейнере, а не на runner.
//!
//! Агент без служебных вызовов (`plain`) присылает `-c <команда>` или `-lc <команда>`, и вся строка — команда для
//! runner, без файла cwd.

mod claude;

use std::fmt;

/// Переменная окружения с видом вызовов агента: `claude` или `plain`. Её задаёт профиль; без неё оболочка отказывает.
pub(crate) const CALLS_ENV: &str = "IAPETUS_SHELL_CALLS";

/// Как агент вызывает оболочку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Calls {
    /// Шаблоны служебных вызовов: `-c env`, сборка снимка и `-c <строка с eval>`.
    Claude,
    /// `-c <команда>` или `-lc <команда>`.
    Plain,
}

impl Calls {
    /// Значение [`CALLS_ENV`] для этого вида.
    pub(crate) const fn value(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Plain => "plain",
        }
    }

    /// Вид вызовов по значению [`CALLS_ENV`]; незаданное и неизвестное значение — ошибка, вида по умолчанию нет.
    pub(crate) fn from_env(value: Option<&str>) -> Result<Self, UnrecognizedCall> {
        match value {
            None => Err(UnrecognizedCall(format!("{CALLS_ENV} не задана"))),
            Some("claude") => Ok(Self::Claude),
            Some("plain") => Ok(Self::Plain),
            Some(other) => Err(UnrecognizedCall(format!("{CALLS_ENV}={other}: неизвестный вид вызовов"))),
        }
    }
}

/// Как агент вызвал оболочку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// Служебный вызов (`-c env`, сборка снимка): его выполняет локальный bash.
    Local,
    /// Команда агента: её выполняет runner.
    Command {
        /// Текст команды, как его написала модель.
        text: String,
        /// Файл, куда после успешной команды записывается cwd; его ждут только агенты с шаблонами вызовов.
        cwd_file: Option<String>,
    },
}

/// Вызов не похож ни на один известный вид.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnrecognizedCall(String);

impl fmt::Display for UnrecognizedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "вызов оболочки не распознан: {}", self.0)
    }
}

impl std::error::Error for UnrecognizedCall {}

pub(super) fn unrecognized<T>(reason: &str) -> Result<T, UnrecognizedCall> {
    Err(UnrecognizedCall(reason.to_owned()))
}

/// Определяет вид вызова по аргументам оболочки (без имени программы).
pub(crate) fn classify(calls: Calls, args: &[String]) -> Result<Invocation, UnrecognizedCall> {
    match calls {
        Calls::Claude => claude::classify(args),
        Calls::Plain => match args {
            [flag, command] if flag == "-c" || flag == "-lc" => Ok(Invocation::Command {
                text: command.clone(),
                cwd_file: None,
            }),
            _ => unrecognized("аргументы не `-c <команда>` и не `-lc <команда>`"),
        },
    }
}

#[cfg(test)]
#[path = "../../../tests/session/shell/call.rs"]
mod tests;
