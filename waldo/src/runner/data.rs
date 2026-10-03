//! Канал данных на стороне runner: SSH-соединение к назначенной сессии и обслуживание её запросов.
//!
//! runner подключается, когда server сообщил о начале сессии, и держит соединение до её конца. Обрыв —
//! явная ошибка для всех запросов и команд сессии; runner переподключается сам: у каждой попытки свой таймаут,
//! между попытками пауза растёт до предела. Открывшийся канал начинается с `Unwatched` по всем наблюдаемым
//! каталогам: что изменилось за время обрыва, сессия не знает.

mod jobs;
mod serve;

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use nerve::connect::session::{Connection, Target, connect};
use nerve::mux::Mux;
use nerve::protocol::Assignment;
use russh::keys::PublicKey;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};
use tokio::time::timeout;

use super::exec::Executor;
use super::fs::Workdir;
use super::identity::Identity;
use super::mcp::Mcp;
use jobs::Jobs;
use serve::MAX_IN_FLIGHT;

/// Сколько длится одна попытка подключиться, от TCP до открытого канала данных.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Пауза перед следующей попыткой: растёт вдвое после каждой неудачи до предела и сбрасывается после удачи.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(15);

/// Сколько сессия может не быть на связи, прежде чем runner завершит процессы её команд.
const ABSENT_LIMIT: Duration = Duration::from_mins(5);

/// Куда и от чьего имени подключаться.
#[derive(Debug)]
pub(super) struct Settings {
    /// Адрес SSH orbit server, `хост:порт`.
    pub(super) jump: String,
    /// Открытый ключ центра сертификации orbit server: по нему проверяются хост-сертификаты jump и сессии.
    pub(super) ca: PublicKey,
    pub(super) identity: Arc<Identity>,
}

impl Settings {
    /// Имя jump в его хост-сертификате: хост из адреса.
    fn jump_host(&self) -> &str {
        self.jump.rsplit_once(':').map_or(self.jump.as_str(), |(host, _)| host)
    }
}

/// Нужно ли соединение: его держат, пока идёт назначение. Новое назначение требует нового соединения, даже если прежнее
/// живо.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Control {
    assignment: Option<Assignment>,
}

/// Канал данных runner.
#[derive(Debug)]
pub(super) struct Data {
    control: watch::Sender<Control>,
    link: watch::Sender<Option<Mux>>,
    /// Команды текущего назначения: переживают обрыв канала.
    jobs: Mutex<Arc<Jobs>>,
    /// MCP-серверы проекта: вызовы к ним приходят по этому каналу.
    mcp: Arc<Mcp>,
    /// Разрешения на файловые запросы: все свободны, когда ни одного запроса не выполняется.
    fs_permits: Arc<Semaphore>,
    /// `$HOME` runner, если он отдаётся сессии.
    home: Option<Arc<Workdir>>,
    /// Занят, пока цикл `run` пробует подключиться или обслуживает соединение.
    connecting: AsyncMutex<()>,
}

impl Data {
    pub(super) fn new(home: Option<Arc<Workdir>>) -> Self {
        Self {
            home,
            control: watch::channel(Control { assignment: None }).0,
            link: watch::channel(None).0,
            jobs: Mutex::new(Arc::new(Jobs::new(Assignment(0)))),
            mcp: Arc::default(),
            fs_permits: Arc::new(Semaphore::new(usize::from(MAX_IN_FLIGHT))),
            connecting: AsyncMutex::new(()),
        }
    }

    fn jobs(&self) -> Arc<Jobs> {
        Arc::clone(&self.jobs.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(super) fn mcp(&self) -> &Arc<Mcp> {
        &self.mcp
    }

    /// Начало назначения открывает соединение. Прежнее назначение к этому времени остановлено (`stop`).
    pub(super) fn start(&self, assignment: Assignment) {
        *self.jobs.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(Jobs::new(assignment));
        self.control.send_replace(Control {
            assignment: Some(assignment),
        });
    }

    /// Конец назначения: закрывает соединение, прекращает приём запросов и дожидается выполняющихся файловых операций;
    /// итоги команд забывает. Команды останавливает исполнитель.
    pub(super) async fn stop(&self) {
        self.control.send_replace(Control { assignment: None });
        drop(self.connecting.lock().await);
        drop(self.fs_permits.acquire_many(u32::from(MAX_IN_FLIGHT)).await);
        self.jobs().clear();
        if let Some(home) = &self.home {
            home.end_session();
        }
        tracing::info!("канал данных закрыт, файловые операции завершены");
    }

    /// Идущее назначение; `None` — сессии нет.
    pub(super) fn assignment(&self) -> Option<Assignment> {
        self.control.borrow().assignment
    }

    /// Текущий канал для отправки изменений; `None`, пока соединения нет.
    pub(super) fn link(&self) -> watch::Receiver<Option<Mux>> {
        self.link.subscribe()
    }

    /// Держит соединение, пока сессия идёт; возвращается только с ошибкой.
    pub(super) async fn run(&self, settings: &Settings, workdir: Arc<Workdir>, executor: Arc<Executor>) -> anyhow::Result<()> {
        tokio::select! {
            result = self.hold(settings, workdir, Arc::clone(&executor)) => result,
            never = self.abandon_when_absent(&executor) => match never {},
        }
    }

    /// Если сессия назначения не на связи дольше [`ABSENT_LIMIT`], завершает процессы её команд: фоновые процессы
    /// иначе жили бы без срока. Вернувшаяся раньше сессия ничего не теряет, отсчёт каждый раз начинается заново.
    async fn abandon_when_absent(&self, executor: &Executor) -> Infallible {
        let mut control = self.control.subscribe();
        let mut link = self.link.subscribe();
        loop {
            let Ok(wanted) = control
                .wait_for(|control| control.assignment.is_some())
                .await
                .map(|control| *control)
            else {
                return std::future::pending().await;
            };
            let absent = async {
                loop {
                    link.wait_for(Option::is_none).await.map(drop).unwrap_or_default();
                    if timeout(ABSENT_LIMIT, link.wait_for(Option::is_some)).await.is_err() {
                        tracing::warn!(limit = ?ABSENT_LIMIT, "сессия не вернулась, процессы её команд завершаются");
                        executor.abandon().await;
                        link.wait_for(Option::is_some).await.map(drop).unwrap_or_default();
                    }
                }
            };
            tokio::select! {
                () = absent => {}
                current = control.wait_for(|current| *current != wanted) => current.map(drop).unwrap_or_default(),
            }
        }
    }

    /// Цикл подключений: держит соединение, пока сессия идёт.
    async fn hold(&self, settings: &Settings, workdir: Arc<Workdir>, executor: Arc<Executor>) -> anyhow::Result<()> {
        let mut control = self.control.subscribe();
        let mut delay = RETRY_MIN;
        loop {
            let wanted = *control
                .wait_for(|control| control.assignment.is_some())
                .await
                .context("управление каналом данных закрыто")?;
            {
                // `stop` дожидается выхода отсюда: после него соединения этого назначения нет.
                let _connecting = self.connecting.lock().await;
                let attempt = tokio::select! {
                    attempt = timeout(ATTEMPT_TIMEOUT, self.attempt(settings)) => Some(attempt),
                    _ = control.wait_for(|current| *current != wanted) => None,
                };
                match attempt {
                    Some(Ok(Ok(connection))) => {
                        delay = RETRY_MIN;
                        self.serve(connection, &mut control, wanted, &workdir, &executor).await;
                    }
                    Some(Ok(Err(error))) => tracing::warn!(error = format!("{error:#}"), "подключение к сессии не удалось"),
                    Some(Err(_)) => tracing::warn!(timeout = ?ATTEMPT_TIMEOUT, "подключение к сессии не уложилось в срок"),
                    None => {}
                }
            }
            // Пауза прерывается, если сессия кончилась или началась другая: ждать перед нужным соединением незачем.
            let paused = timeout(delay, control.wait_for(|current| *current != wanted)).await;
            if paused.is_err() {
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
    }

    async fn attempt(&self, settings: &Settings) -> anyhow::Result<Connection> {
        let certificate = settings
            .identity
            .certificate()
            .context("у узла нет действующего сертификата: он не зарегистрирован или не продлён")?;
        let target = Target {
            jump: &settings.jump,
            jump_host: settings.jump_host(),
            ca: &settings.ca,
        };
        connect(&target, &settings.identity.private_key(), &certificate).await
    }

    /// Обслуживает соединение, пока оно живо и сессия та же.
    async fn serve(
        &self,
        connection: Connection,
        control: &mut watch::Receiver<Control>,
        wanted: Control,
        workdir: &Arc<Workdir>,
        executor: &Arc<Executor>,
    ) {
        // Остальные поля соединения — SSH-соединения; они живут до конца функции.
        let Connection { mux, requests, .. } = connection;
        // Подписки сбрасываются до первого запроса сессии: иначе `Unwatched` пришёл бы после её `ReadDir` и отнял
        // бы у только что прочитанного каталога наблюдение.
        workdir.resync_watch();
        if let Some(home) = &self.home {
            home.resync_watch();
        }
        self.link.send_replace(Some(mux.clone()));
        tracing::info!("канал данных открыт");
        // Первой проверяется смена назначения: запрос, пришедший вместе с ней, не обслуживается.
        tokio::select! {
            biased;
            _ = control.wait_for(|current| *current != wanted) => tracing::info!("сессия сменилась или кончилась, канал данных закрывается"),
            () = serve::serve(requests, Arc::clone(workdir), self.home.clone(), Arc::clone(executor), self.jobs(), Arc::clone(&self.fs_permits), Arc::clone(&self.mcp)) => tracing::info!("session закрыла поток запросов, канал данных закрывается"),
            closed = mux.closed() => tracing::warn!(reason = %closed, "канал данных закрыт"),
        }
        self.link.send_replace(None);
        mux.close();
    }
}

#[cfg(test)]
#[path = "../../tests/runner/data.rs"]
mod tests;
