//! Канал управления session с server: вход `session-<fp>`, состояние сессии и указания server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use nerve::connect::server::{self, Channel, Login, Server};
use nerve::fingerprint::Fingerprint;
use nerve::protocol::{
    self, Activity, AgentRelease, AgentRun, AssignedRunner, Control, ControlResponse, DocsEvent, Error, MAX_MCP_CATALOG, McpCatalog,
    SessionRequest, Status,
};
use nerve::release;
use tokio::sync::{mpsc, watch};

use super::agent::Profile;
use super::data::Data;
use super::fs::Vfs;
use super::host::{Environment, Workspace};
use super::launch::AgentCommand;
use super::mcp::Project;
use super::rules::Rules;
use super::state::Verdict;
use super::update::Offer;

/// Как долго ждать ответа server на `Hello` и `Status`.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// Канал управления session: что она сообщает server и чьи указания принимает.
pub(super) struct Link {
    pub(super) server: Server,
    pub(super) session: Fingerprint,
    pub(super) secret: String,
    pub(super) data: Data,
    /// Файловая система: при смене runner её кэш сбрасывается.
    pub(super) vfs: Arc<Vfs>,
    /// Назначенный runner: меняется указанием server `AssignRunner`.
    pub(super) runner: watch::Sender<Option<Fingerprint>>,
    pub(super) activity: watch::Receiver<Activity>,
    /// Агент запущен в pty.
    pub(super) agent: watch::Receiver<bool>,
    /// Состояние сессии по событиям: каждая смена уходит server как `Status`.
    pub(super) state: watch::Receiver<Verdict>,
    /// Файл правил агента; `None` — агента нет.
    pub(super) rules: Option<Rules>,
    /// Сводка проектных MCP-серверов runner.
    pub(super) project: Project,
    /// Указания тому, кто держит процесс агента.
    pub(super) commands: mpsc::Sender<AgentCommand>,
    /// Агент этой session: чужому выпуску она отказывает.
    pub(super) profile: Profile,
    /// Ожидающее обновление агента; `Err` — почему session его не принимает.
    pub(super) agent_update: Result<watch::Sender<Option<Offer>>, &'static str>,
    /// Файловая система проекта и окружение runner в контейнере.
    pub(super) workspace: Arc<Workspace>,
    /// Окружение последнего назначенного runner: по нему агент запускается.
    pub(super) environment: watch::Sender<Option<Environment>>,
}

impl Link {
    /// Держит канал управления, пока server не откажет session в доступе.
    pub(super) async fn run(self) -> anyhow::Error {
        let login = Login::Session {
            session: &self.session,
            secret: &self.secret,
        };
        let denied = server::keep_connected(
            async || server::open(&self.server, &login).await,
            async |channel| {
                if let Err(error) = self.serve(channel).await {
                    tracing::warn!(error = %format!("{error:#}"), "канал управления прерван");
                }
            },
        )
        .await;
        anyhow!("{denied}")
    }

    /// Работает с каналом до его закрытия: `Hello`, подписка `Docs`, затем `Status` при каждой смене и указания
    /// server.
    async fn serve(&self, channel: Channel) -> anyhow::Result<()> {
        let Channel { mux, mut requests, .. } = channel;
        let mut runner = self.runner.subscribe();
        let mut activity = self.activity.clone();
        let mut agent = self.agent.clone();
        let mut state = self.state.clone();
        let hello = Self::status(&mut runner, &mut activity, &mut agent, &mut state);
        send(&mux, SessionRequest::Hello(hello)).await?;

        let mut docs = server::subscribe::<_, DocsEvent>(&mux, &SessionRequest::Docs).await?;
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    let response = match protocol::decode_body::<Control>(request.body()) {
                        Ok(control) => self.control(control),
                        Err(error) => {
                            tracing::warn!(%error, "указание server не разобрано");
                            Err(Error::Io("указание не разобрано".to_owned()))
                        }
                    };
                    request.respond(protocol::encode(&response)?.into(), true).await?;
                }
                event = docs.next() => {
                    let Some(event) = event? else { return Ok(()) };
                    tracing::debug!(?event, "документы обновлены");
                    if let Some(rules) = &self.rules {
                        rules.apply(event).unwrap_or_else(|error| tracing::warn!(error = %format!("{error:#}"), "правила Iapetus не обновлены"));
                    }
                }
                changed = changed(&mut runner, &mut activity, &mut agent, &mut state) => {
                    changed?;
                    let status = Self::status(&mut runner, &mut activity, &mut agent, &mut state);
                    send(&mux, SessionRequest::Status(status)).await?;
                }
            }
        }
    }

    fn status(
        runner: &mut watch::Receiver<Option<Fingerprint>>,
        activity: &mut watch::Receiver<Activity>,
        agent: &mut watch::Receiver<bool>,
        state: &mut watch::Receiver<Verdict>,
    ) -> Status {
        let verdict = state.borrow_and_update().clone();
        Status {
            state: verdict.state,
            reason: verdict.reason,
            cause: verdict.cause,
            limit_reset: verdict.limit_reset,
            runner: runner.borrow_and_update().clone(),
            activity: *activity.borrow_and_update(),
            agent_running: *agent.borrow_and_update(),
            limits: verdict.limits,
            restart_pending: verdict.restart_pending,
        }
    }

    /// Ставит перезапуск агента в очередь: он ждёт конца хода, а повторное указание во время ожидания перезапускает
    /// агента сразу. Процесс в любом случае запускается один раз.
    fn resume(&self) -> ControlResponse {
        if !*self.agent.borrow() {
            return Err(Error::Io("агент не запущен или уже перезапускается".to_owned()));
        }
        match self.commands.try_send(AgentCommand::Resume) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Io("перезапуск агента недоступен".to_owned())),
        }
    }

    /// Применяет назначение: канал данных принимает только нового runner (соединение прежнего закрывается), кэш FUSE
    /// переходит к нему, правила агента переписываются. Затем агент останавливается, перезапускается или остаётся как
    /// есть, по `agent`. Без runner агент останавливается всегда.
    ///
    /// Окружение runner ставится до всего остального: путь проекта, который нельзя смонтировать, — отказ назначению, и
    /// ничего не меняется. ФС монтирует тот, кто запускает агента. Если путь проекта сменился, а указание не трогало
    /// агента, агент перезапускается с продолжением разговора: его рабочий каталог исчез.
    fn assign(&self, assigned: Option<&AssignedRunner>, agent: AgentRun) -> ControlResponse {
        let environment = assigned
            .map(|assigned| {
                let environment = self
                    .workspace
                    .environment(&assigned.host)
                    .map_err(|reason| Error::Io(format!("путь проекта runner отклонён: {reason}")))?;
                self.workspace
                    .apply(&assigned.host, &environment)
                    .map_err(|error| Error::Io(format!("окружение runner не поставлено: {error:#}")))?;
                Ok(environment)
            })
            .transpose()?;
        let moved = environment
            .as_ref()
            .is_some_and(|new| self.environment.borrow().as_ref().is_some_and(|current| current.moved_to(new)));
        if environment.is_some() {
            self.environment.send_replace(environment);
        }
        let runner = assigned.map(|assigned| assigned.runner.clone());
        let switched = runner.is_some() && *self.runner.borrow() != runner;
        // Сводка принадлежит runner: при снятии и смене назначения её нет, пока не придёт сводка нового.
        let reassigned = *self.runner.borrow() != runner;
        if reassigned {
            self.project.reset();
            if let Some(rules) = &self.rules {
                rules
                    .set_mcp(&McpCatalog::default())
                    .unwrap_or_else(|error| tracing::warn!(error = %format!("{error:#}"), "правила Iapetus не обновлены"));
            }
        }
        self.data.assign(runner.clone());
        if let (true, Some(runner)) = (switched, &runner) {
            self.vfs.switch_runner(runner);
            self.workspace.switch_runner(runner);
        }
        if let Some(rules) = &self.rules {
            rules
                .assign(assigned)
                .unwrap_or_else(|error| tracing::warn!(error = %format!("{error:#}"), "правила Iapetus не обновлены"));
        }
        self.runner.send_replace(runner.clone());
        let command = match (runner, agent) {
            (None, _) | (Some(_), AgentRun::Stop) => AgentCommand::Stop,
            (Some(_), AgentRun::Restart { resume }) => AgentCommand::Restart { resume },
            (Some(_), AgentRun::Keep) if moved => AgentCommand::Restart { resume: true },
            (Some(_), AgentRun::Keep) => return Ok(()),
        };
        self.commands
            .try_send(command)
            .map_err(|_| Error::Io("указание агенту не принято".to_owned()))
    }

    /// Запоминает сводку проектных MCP-серверов вместо прежней и дописывает их инструкции в правила. Сводка больше
    /// предела отвергается, прежняя остаётся.
    fn mcp_catalog(&self, catalog: McpCatalog) -> ControlResponse {
        let size = protocol::encode(&catalog)
            .map_err(|error| Error::Io(format!("сводка MCP не закодирована: {error}")))?
            .len();
        if size > MAX_MCP_CATALOG {
            return Err(Error::Io(format!("сводка MCP в {size} байт больше предела {MAX_MCP_CATALOG}")));
        }
        if let Some(rules) = &self.rules {
            rules
                .set_mcp(&catalog)
                .unwrap_or_else(|error| tracing::warn!(error = %format!("{error:#}"), "правила Iapetus не обновлены"));
        }
        self.project.set(catalog);
        Ok(())
    }

    /// Запоминает обновление агента как ожидающее вместо прежнего; выполнит его `Updater`, когда можно.
    fn update_agent(&self, release: &AgentRelease) -> ControlResponse {
        if release.agent != self.profile.0 {
            return Err(Error::Io(release::OTHER_AGENT.to_owned()));
        }
        let pending = self.agent_update.as_ref().map_err(|reason| Error::Io((*reason).to_owned()))?;
        let offer = Offer::choose(release).map_err(|error| Error::Io(format!("{error:#}")))?;
        // То же предложение, что уже ожидает, не трогается: идущая загрузка не прерывается.
        pending.send_if_modified(|current| {
            let changed = current.as_ref() != Some(&offer);
            if changed {
                *current = Some(offer);
            }
            changed
        });
        Ok(())
    }

    fn control(&self, control: Control) -> ControlResponse {
        match control {
            Control::AssignRunner { runner, agent } => self.assign(runner.as_ref(), agent),
            Control::McpCatalog(catalog) => self.mcp_catalog(catalog),
            Control::Resume => self.resume(),
            Control::UpdateAgent(release) => self.update_agent(&release),
        }
    }
}

async fn changed(
    runner: &mut watch::Receiver<Option<Fingerprint>>,
    activity: &mut watch::Receiver<Activity>,
    agent: &mut watch::Receiver<bool>,
    state: &mut watch::Receiver<Verdict>,
) -> anyhow::Result<()> {
    tokio::select! {
        changed = runner.changed() => changed.context("назначение runner закрыто")?,
        changed = activity.changed() => changed.context("занятость агента закрыта")?,
        changed = agent.changed() => changed.context("запуск агента закрыт")?,
        changed = state.changed() => changed.context("состояние сессии закрыто")?,
    }
    Ok(())
}

async fn send(mux: &nerve::mux::Mux, request: SessionRequest) -> anyhow::Result<()> {
    let reply = tokio::time::timeout(REPLY_TIMEOUT, server::ask::<_, Result<(), Error>>(mux, &request))
        .await
        .context("server не ответил")??;
    reply.map_err(|error| anyhow!("server отклонил сообщение: {error}"))
}

#[cfg(test)]
#[path = "../../tests/session/control.rs"]
mod tests;
