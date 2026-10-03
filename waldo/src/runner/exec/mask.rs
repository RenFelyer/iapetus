//! Маскирование значений секретов в потоке вывода.
//!
//! Вывод приходит кусками произвольного размера, и секрет может лежать на границе двух кусков. Поэтому хвост
//! куска, который может оказаться началом секрета, удерживается до следующего куска. Результат не зависит от
//! разбиения: любое разбиение потока даёт те же байты, что и вся запись целиком.

use std::ops::Range;
use std::sync::Arc;

/// Чем заменяется секрет.
const MASK: &[u8] = b"***";

/// Значения короче не маскируются: они не секреты (`1`, `true`), а замена ломала бы обычный вывод. Нижняя граница
/// длины нужна и для другого: замена на `***` никогда не удлиняет вывод, кусок не вырастает сверх предела сообщения.
const MIN_SECRET_LEN: usize = 4;

/// Имя переменной, значение которой — секрет: оканчивается на `_TOKEN`, `_KEY`, `_SECRET` или `_PASSWORD` (или
/// равно одному из них без подчёркивания). Регистр не важен.
///
/// Правило простое и предсказуемое: так называют переменные с учётными данными (`GITHUB_TOKEN`, `API_KEY`,
/// `AWS_SECRET_ACCESS_KEY`, `DB_PASSWORD`), а имена вроде `MONKEY` или `KEYBOARD` под него не попадают. Секрет
/// внутри другого значения (пароль в `DATABASE_URL`) оно не видит.
pub(in crate::runner) fn is_secret_name(name: &str) -> bool {
    const SUFFIXES: [&str; 4] = ["TOKEN", "KEY", "SECRET", "PASSWORD"];
    let name = name.to_ascii_uppercase();
    SUFFIXES
        .iter()
        .any(|suffix| name == *suffix || name.strip_suffix(suffix).is_some_and(|rest| rest.ends_with('_')))
}

/// Значения, которые нельзя показывать в выводе.
#[derive(Debug, Clone, Default)]
pub(in crate::runner) struct Secrets(Arc<[Vec<u8>]>);

impl Secrets {
    /// Слишком короткие значения отбрасываются, см. [`MIN_SECRET_LEN`].
    pub(in crate::runner) fn new(values: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self(values.into_iter().filter(|value| value.len() >= MIN_SECRET_LEN).collect())
    }

    /// Те же секреты и ещё `extra`.
    pub(in crate::runner) fn with(&self, extra: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self::new(self.0.iter().cloned().chain(extra))
    }

    /// Самый длинный секрет, начинающийся в `tail`.
    fn longest_at(&self, tail: &[u8]) -> Option<usize> {
        self.0.iter().filter(|secret| tail.starts_with(secret)).map(Vec::len).max()
    }

    /// Начало самого раннего суффикса `data`, который — собственное начало какого-то секрета: с новыми данными он
    /// может стать целым секретом.
    fn earliest_partial_suffix(&self, data: &[u8]) -> Option<usize> {
        self.0
            .iter()
            .filter_map(|secret| {
                let longest = secret.len().saturating_sub(1).min(data.len());
                (1..=longest)
                    .rev()
                    .find(|&len| data.ends_with(&secret[..len]))
                    .map(|len| data.len() - len)
            })
            .min()
    }

    /// Участки `data`, занятые секретами; пересекающиеся секреты сливаются в один участок.
    fn covered(&self, data: &[u8]) -> Vec<Range<usize>> {
        let mut covered: Vec<Range<usize>> = Vec::new();
        for start in 0..data.len() {
            let Some(len) = self.longest_at(&data[start..]) else {
                continue;
            };
            let end = start + len;
            match covered.last_mut() {
                Some(last) if start < last.end => last.end = last.end.max(end),
                _ => covered.push(start..end),
            }
        }
        covered
    }
}

/// Маскирует один поток вывода.
#[derive(Debug)]
pub(in crate::runner) struct Masker {
    secrets: Secrets,
    /// Хвост, который ещё может оказаться секретом; не отправлен.
    held: Vec<u8>,
}

impl Masker {
    pub(in crate::runner) fn new(secrets: Secrets) -> Self {
        Self { secrets, held: Vec::new() }
    }

    /// Принимает кусок вывода и возвращает то, что уже безопасно отправлять.
    pub(in crate::runner) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.secrets.0.is_empty() {
            return chunk.to_vec();
        }
        self.held.extend_from_slice(chunk);
        self.release(false)
    }

    /// Конец потока: удержанный хвост уже не станет секретом.
    pub(in crate::runner) fn finish(&mut self) -> Vec<u8> {
        self.release(true)
    }

    fn release(&mut self, end_of_stream: bool) -> Vec<u8> {
        let data = std::mem::take(&mut self.held);
        let covered = self.secrets.covered(&data);
        let mut hold_from = if !end_of_stream && let Some(partial) = self.secrets.earliest_partial_suffix(&data) {
            partial
        } else {
            data.len()
        };
        // Участок, который задевает удерживаемый хвост, удерживается целиком: его начало — часть секрета.
        if let Some(straddling) = covered.iter().find(|range| range.start < hold_from && hold_from < range.end) {
            hold_from = straddling.start;
        }
        let mut out = Vec::with_capacity(hold_from);
        let mut position = 0;
        for range in covered.iter().filter(|range| range.end <= hold_from) {
            out.extend_from_slice(&data[position..range.start]);
            out.extend_from_slice(MASK);
            position = range.end;
        }
        out.extend_from_slice(&data[position..hold_from]);
        self.held = data[hold_from..].to_vec();
        out
    }
}

#[cfg(test)]
#[path = "../../../tests/runner/exec/mask.rs"]
mod tests;
