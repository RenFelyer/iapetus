//! Выпуски агентов: разбор версии и контрольной суммы, одинаковый у server и session.

use crate::protocol::Digest;

/// Отказ session обновлять агента: выпуск предназначен другому агенту.
pub const OTHER_AGENT: &str = "обновление для другого агента";

/// Отказ session обновлять агента: у профиля её агента нет обновления.
pub const NOT_UPDATABLE: &str = "session не обновляет своего агента";

/// Отказ session обновлять агента: агент обновляемый, но эта session его не запускает (команды агента нет).
pub const NO_AGENT_COMMAND: &str = "session не запускает агента: команды агента нет";

/// Версия вида `2.1.285` числами: версии сравниваются по ним, а не как строки. Лишние части, пустые части и всё,
/// кроме цифр, — отказ.
#[must_use]
pub fn parse_version(text: &str) -> Option<[u64; 3]> {
    let mut parts = text.split('.').map(|part| {
        let digits = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        digits.then(|| part.parse().ok()).flatten()
    });
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// sha256 в шестнадцатеричной записи: 64 цифры, только строчные.
#[must_use]
pub fn is_sha256(text: &str) -> bool {
    is_lower_hex(text, 64)
}

/// sha512 в шестнадцатеричной записи: 128 цифр, только строчные.
#[must_use]
pub fn is_sha512(text: &str) -> bool {
    is_lower_hex(text, 128)
}

/// Контрольная сумма записана верно для своего алгоритма.
#[must_use]
pub fn is_well_formed(digest: &Digest) -> bool {
    match digest {
        Digest::Sha256(text) => is_sha256(text),
        Digest::Sha512(text) => is_sha512(text),
    }
}

#[cfg(test)]
#[path = "../tests/release.rs"]
mod tests;
