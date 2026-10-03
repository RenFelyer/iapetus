//! Срок сертификата SSH и его продление, общие для waldo worker и runner.

use std::convert::Infallible;
use std::ops::ControlFlow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::keys::Certificate;

/// За сколько до `valid_before` сертификат продлевается.
const RENEW_BEFORE: Duration = Duration::from_mins(5);

/// Пауза перед повторной попыткой, когда продление не удалось.
pub(super) const RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Через сколько продлевать: сразу, если сертификата нет, он истёк или до `valid_before` осталось не больше
/// пяти минут; иначе — остаток до этого момента.
pub fn renewal_delay(certificate: Option<&Certificate>, now: SystemTime) -> Duration {
    let Some(certificate) = certificate else {
        return Duration::ZERO;
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let renew_at = certificate.valid_before().saturating_sub(RENEW_BEFORE.as_secs());
    Duration::from_secs(renew_at.saturating_sub(now))
}

/// Пора ли продлевать: сертификата нет, он истёк или до конца осталось не больше пяти минут.
pub fn renewal_due(certificate: Option<&Certificate>, now: SystemTime) -> bool {
    renewal_delay(certificate, now).is_zero()
}

/// Продлевает сертификат, пока процесс работает: спит до момента продления сертификата из `current` и вызывает `renew`.
/// Ошибка продления процесс не останавливает — сертификат действует до конца срока, и через минуту
/// продление повторяется.
pub async fn keep_fresh(current: impl Fn() -> Option<Certificate>, renew: impl AsyncFn() -> anyhow::Result<()>) -> ! {
    let never: Infallible = keep_fresh_until(current, async || renew().await.map(ControlFlow::Continue)).await;
    match never {}
}

/// То же, что [`keep_fresh`], но `renew` может остановить цикл: `Break` возвращается наружу.
pub async fn keep_fresh_until<B>(current: impl Fn() -> Option<Certificate>, renew: impl AsyncFn() -> anyhow::Result<ControlFlow<B>>) -> B {
    loop {
        tokio::time::sleep(renewal_delay(current().as_ref(), SystemTime::now())).await;
        match renew().await {
            Ok(ControlFlow::Break(stop)) => return stop,
            Ok(ControlFlow::Continue(())) => {}
            Err(error) => tracing::warn!(error = format!("{error:#}"), "сертификат не продлён"),
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
#[path = "../../tests/connect/certificate.rs"]
mod tests;
