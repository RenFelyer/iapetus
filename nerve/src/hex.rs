//! Шестнадцатеричная запись байтов.

/// Строчные шестнадцатеричные цифры, по две на байт.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 15)]])
        .map(char::from)
        .collect()
}
