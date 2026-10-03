//! Состояние сессии по событиям: шлюз, runner, процесс агента.
//!
//! Автомат [`Machine`] хранит причины, а не состояние: состояние выводится из них, поэтому порядок событий
//! не важен. В `waiting` ошибки остаются `waiting` с причиной; `failure` — только когда процесс агента завершается
//! снова и снова: единичный выход session перезапускает сама, и это `waiting` с причиной «агент перезапущен».

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve::protocol::{Cause, UsageWindow};
use nerve::session_state::SessionState;
use tokio::sync::{broadcast, mpsc, watch};

use super::data;
use super::gateway::{GatewayEvent, LimitReset, Observation, RequestKind};

#[cfg(test)]
#[path = "../../tests/session/state.rs"]
mod tests;

/// Что сообщает автомат: состояние, причина и момент сброса лимита подписки (секунды Unix).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Verdict {
    pub(super) state: SessionState,
    pub(super) reason: Option<String>,
    pub(super) cause: Option<Cause>,
    pub(super) limit_reset: Option<u64>,
    /// Последние окна лимитов подписки, которые назвал сервис.
    pub(super) limits: Vec<UsageWindow>,
    /// Перезапуск агента затребован и ждёт конца основного хода.
    pub(super) restart_pending: bool,
}

impl Verdict {
    /// Состояние до первого события: ждёт, причины нет.
    pub(super) fn waiting() -> Self {
        Self {
            state: SessionState::Waiting,
            reason: None,
            cause: None,
            limit_reset: None,
            limits: Vec::new(),
            restart_pending: false,
        }
    }
}

/// Жизнь процесса агента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentEvent {
    Started,
    /// Плановая остановка: перезапуск через `resume`, обновление. Агент поднимется снова.
    Stopped,
    /// Агент остановлен и не поднимется, пока server не назначит runner.
    Halted,
    /// Процесс завершается снова и снова сразу после запуска: session его больше не перезапускает.
    Exited,
    /// Процесс завершился сам, session запускает его заново с продолжением разговора.
    Relaunched,
    /// Перезапуск по требованию ждёт конца основного хода; снимается остановкой или выходом агента.
    RestartPending,
}

#[derive(Debug)]
pub(super) enum Input {
    Gateway(Observation),
    Runner { connected: bool },
    Agent(AgentEvent),
}

/// Где агент в своей жизни.
#[derive(Debug, Default, PartialEq, Eq)]
enum Phase {
    /// Запущен или ещё не запущен, сервис учётные данные не принимал.
    #[default]
    Starting,
    /// Сервис принял учётные данные после запуска.
    Authorized,
    /// Остановлен планово, ждёт нового запуска.
    Stopped,
    /// Остановлен и ждёт назначения runner.
    Halted,
    /// Процесс завершается снова и снова, session его не перезапускает.
    Failed,
}

/// Перезапуск агента по требованию worker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Restart {
    #[default]
    None,
    /// Ждёт конца основного хода.
    Pending,
}

/// Связь с runner: пока он не подключался ни разу, его отсутствие — ожидание, а не потеря.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum RunnerLink {
    #[default]
    Never,
    Connected,
    Lost,
}

/// Исчерпанный лимит подписки; `reset` — момент сброса (секунды Unix), если сервис его сообщил.
#[derive(Debug, Clone, Copy)]
struct Limit {
    reset: Option<u64>,
}

/// Причины `warning` и `waiting`; пусто — всё штатно.
#[derive(Debug, Default)]
pub(super) struct Machine {
    phase: Phase,
    runner: RunnerLink,
    rejected: Option<u16>,
    no_connection: bool,
    limit: Option<Limit>,
    unretried: Option<u16>,
    context_overflow: bool,
    /// Агент завершился сам и запущен заново: причина видна, пока сервис не примет его первый запрос.
    relaunched: bool,
    /// Окна лимитов переживают перезапуск агента: подписка та же.
    limits: Vec<UsageWindow>,
    restart: Restart,
}

impl Machine {
    /// `now` — секунды Unix: по нему относительный сброс лимита становится моментом.
    pub(super) fn apply(&mut self, input: Input, now: u64) {
        match input {
            Input::Agent(AgentEvent::Started) => {
                *self = Self {
                    runner: self.runner,
                    relaunched: self.relaunched,
                    limits: std::mem::take(&mut self.limits),
                    ..Self::default()
                }
            }
            Input::Agent(AgentEvent::Stopped) => {
                self.phase = Phase::Stopped;
                self.relaunched = false;
                self.restart = Restart::None;
            }
            Input::Agent(AgentEvent::Halted) => {
                self.phase = Phase::Halted;
                self.relaunched = false;
                self.restart = Restart::None;
            }
            Input::Agent(AgentEvent::Exited) => {
                self.phase = Phase::Failed;
                self.relaunched = false;
                self.restart = Restart::None;
            }
            Input::Agent(AgentEvent::Relaunched) => {
                self.phase = Phase::Stopped;
                self.relaunched = true;
                self.restart = Restart::None;
            }
            Input::Agent(AgentEvent::RestartPending) => self.restart = Restart::Pending,
            Input::Runner { connected: true } => self.runner = RunnerLink::Connected,
            Input::Runner { connected: false } => {
                if self.runner == RunnerLink::Connected {
                    self.runner = RunnerLink::Lost;
                }
            }
            Input::Gateway(Observation { request, event }) => self.gateway(&request, &event, now),
        }
    }

    fn gateway(&mut self, request: &RequestKind, event: &GatewayEvent, now: u64) {
        if matches!(self.phase, Phase::Failed | Phase::Stopped | Phase::Halted) {
            return;
        }
        let main = *request == RequestKind::Main;
        match event {
            GatewayEvent::Accepted => {
                self.phase = Phase::Authorized;
                self.relaunched = false;
                self.rejected = None;
                self.no_connection = false;
                if main {
                    self.limit = None;
                    self.unretried = None;
                    self.context_overflow = false;
                }
            }
            GatewayEvent::Unauthorized { status, .. } => self.rejected = Some(*status),
            GatewayEvent::SubscriptionLimitReached { reset } => {
                let reset = reset.map(|reset| match reset {
                    LimitReset::At(at) => at,
                    LimitReset::After(after) => now + after.as_secs(),
                });
                self.limit = Some(Limit { reset });
            }
            GatewayEvent::Limits(windows) => self.limits.clone_from(windows),
            GatewayEvent::ConnectionError { .. } => self.no_connection = true,
            GatewayEvent::UnretriedError { status, .. } if main => self.unretried = Some(*status),
            GatewayEvent::ContextOverflow if main => self.context_overflow = true,
            // Агент повторяет эти ошибки сам; остальное состояния не меняет.
            GatewayEvent::UnretriedError { .. }
            | GatewayEvent::ContextOverflow
            | GatewayEvent::ServerError { .. }
            | GatewayEvent::RequestRateLimited
            | GatewayEvent::UpstreamBroken
            | GatewayEvent::Interrupted
            | GatewayEvent::EndTurn
            | GatewayEvent::ToolUse { .. } => {}
        }
    }

    pub(super) fn verdict(&self) -> Verdict {
        let limit_reset = self.limit.and_then(|limit| limit.reset);
        let (state, cause, reason) = self.diagnosis();
        Verdict {
            state,
            reason: reason.map(str::to_owned).or_else(|| self.detail(cause)),
            cause,
            limit_reset,
            limits: self.limits.clone(),
            restart_pending: self.restart == Restart::Pending,
        }
    }

    fn diagnosis(&self) -> (SessionState, Option<Cause>, Option<&'static str>) {
        match self.phase {
            Phase::Failed => {
                return (
                    SessionState::Failure,
                    Some(Cause::AgentExited),
                    Some("агент завершается сразу после запуска, перезапуск остановлен"),
                );
            }
            Phase::Stopped if self.relaunched => return (SessionState::Waiting, Some(Cause::AgentRestarted), Some("агент перезапущен")),
            Phase::Stopped => return (SessionState::Waiting, Some(Cause::Restarting), Some("агент перезапускается")),
            Phase::Halted => return (SessionState::Waiting, Some(Cause::RunnerRequired), Some("нужно назначить runner")),
            Phase::Starting | Phase::Authorized => {}
        }
        let problem = self.problem();
        let awaiting_runner = self.runner == RunnerLink::Never;
        if self.phase == Phase::Starting || self.rejected.is_some() || awaiting_runner {
            let first = if self.relaunched {
                Cause::AgentRestarted
            } else {
                Cause::AwaitingFirstResponse
            };
            return (SessionState::Waiting, Some(problem.unwrap_or(first)), None);
        }
        match problem {
            Some(cause) => (SessionState::Warning, Some(cause), None),
            None => (SessionState::Success, None, None),
        }
    }

    /// Главная причина: нужен вход, затем потеря связи, лимит, ошибка основного хода.
    fn problem(&self) -> Option<Cause> {
        if self.rejected.is_some() {
            Some(Cause::LoginRequired)
        } else if self.runner != RunnerLink::Connected {
            Some(Cause::RunnerOffline)
        } else if self.no_connection {
            Some(Cause::NoConnection)
        } else if self.limit.is_some() {
            Some(Cause::LimitReached)
        } else if self.unretried.is_some() {
            Some(Cause::RequestFailed)
        } else if self.context_overflow {
            Some(Cause::ContextOverflow)
        } else {
            None
        }
    }

    /// Текст причины для человека.
    fn detail(&self, cause: Option<Cause>) -> Option<String> {
        Some(match cause? {
            Cause::AwaitingFirstResponse => "ждёт первого ответа сервиса".to_owned(),
            Cause::AgentRestarted => "агент перезапущен".to_owned(),
            Cause::LoginRequired => format!(
                "нужен вход: подключитесь к агенту и войдите заново ({})",
                self.rejected.unwrap_or_default()
            ),
            Cause::RunnerOffline => "runner не подключён".to_owned(),
            Cause::NoConnection => "нет соединения".to_owned(),
            Cause::LimitReached => "лимит подписки исчерпан".to_owned(),
            Cause::RequestFailed => format!("ошибка запроса ({}), агент её не повторяет", self.unretried.unwrap_or_default()),
            Cause::ContextOverflow => "контекст переполнен: нужно сжатие или новый разговор".to_owned(),
            Cause::Restarting | Cause::RunnerRequired | Cause::AgentExited => return None,
        })
    }
}

/// Ведёт автомат по событиям и выдаёт новый [`Verdict`] при каждой смене.
pub(super) async fn follow(
    mut events: broadcast::Receiver<Observation>,
    mut link: data::Link,
    mut agent: mpsc::UnboundedReceiver<AgentEvent>,
    verdict: watch::Sender<Verdict>,
) {
    let mut machine = Machine::default();
    machine.apply(
        Input::Runner {
            connected: link.is_connected(),
        },
        now(),
    );
    verdict.send_replace(machine.verdict());
    loop {
        let input = tokio::select! {
            event = events.recv() => match event {
                Ok(observation) => Input::Gateway(observation),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "события шлюза пропущены, состояние сессии может отставать");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            connected = link.changed() => match connected {
                Some(connected) => Input::Runner { connected },
                None => return,
            },
            event = agent.recv() => match event {
                Some(event) => Input::Agent(event),
                None => return,
            },
        };
        machine.apply(input, now());
        let next = machine.verdict();
        verdict.send_if_modified(|current| {
            let changed = *current != next;
            *current = next;
            changed
        });
    }
}

/// Ждёт выхода агента и сообщает его ([`record_agent_exit`]); после этого не возвращается: сессия остаётся жить
/// в `failure`, пока человек не вмешается. Так кончается агент, который завершается снова и снова.
pub(super) async fn report_agent_exit(
    exit: impl Future<Output = anyhow::Result<()>>,
    agent: &mpsc::UnboundedSender<AgentEvent>,
    running: &watch::Sender<bool>,
    planned: &AtomicBool,
) -> Infallible {
    record_agent_exit(exit.await, agent, running, planned);
    std::future::pending().await
}

/// Опускает флаг `running` и сообщает выход автомату. Если `planned` поднят, выход плановый (перезапуск, обновление) и
/// `failure` не наступает.
pub(super) fn record_agent_exit(
    result: anyhow::Result<()>,
    agent: &mpsc::UnboundedSender<AgentEvent>,
    running: &watch::Sender<bool>,
    planned: &AtomicBool,
) {
    running.send_replace(false);
    let event = if planned.swap(false, Ordering::SeqCst) {
        AgentEvent::Stopped
    } else {
        if let Err(error) = result {
            tracing::error!(error = %format!("{error:#}"), "агент упал");
        }
        AgentEvent::Exited
    };
    agent.send(event).unwrap_or_default();
}

/// Опускает флаг `running` и сообщает автомату, что процесс агента завершился сам и будет запущен заново.
pub(super) fn record_agent_relaunch(result: anyhow::Result<()>, agent: &mpsc::UnboundedSender<AgentEvent>, running: &watch::Sender<bool>) {
    running.send_replace(false);
    match result {
        Ok(()) => tracing::info!("агент завершился сам, запускается заново с продолжением разговора"),
        Err(error) => tracing::warn!(error = %format!("{error:#}"), "агент завершился сам, запускается заново с продолжением разговора"),
    }
    agent.send(AgentEvent::Relaunched).unwrap_or_default();
}

/// Следит за основным ходом: `open` поднят, пока ход идёт. Ход начинается с принятого запроса или вызова
/// инструмента и кончается его концом, обрывом или ошибкой, на которой он останавливается.
pub(super) async fn track_turn(mut events: broadcast::Receiver<Observation>, open: watch::Sender<bool>) {
    loop {
        match events.recv().await {
            Ok(Observation {
                request: RequestKind::Main,
                event,
            }) => match event {
                GatewayEvent::Accepted | GatewayEvent::ToolUse { .. } => {
                    open.send_replace(true);
                }
                GatewayEvent::EndTurn
                | GatewayEvent::Interrupted
                | GatewayEvent::UnretriedError { .. }
                | GatewayEvent::ContextOverflow
                | GatewayEvent::SubscriptionLimitReached { .. }
                | GatewayEvent::Unauthorized { .. } => {
                    open.send_replace(false);
                }
                _ => {}
            },
            Ok(_) => {}
            // Среди пропущенных мог быть конец хода: ход считается закрытым, чтобы перезапуск не ждал вечно.
            Err(broadcast::error::RecvError::Lagged(_)) => {
                open.send_replace(false);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs()
}
