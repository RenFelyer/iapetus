//! Канал управления worker: подключение к server, ключ и сертификат этого запуска, запросы к server.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use nerve::connect::server::{self, Channel, Denied, Node, Replies, Role, Server};
use nerve::fingerprint::Fingerprint;
use nerve::mux::Mux;
use nerve::protocol::{
    Agent, CloneReply, CloneRequest, CloneResponse, EnvChange, EnvChangeResponse, EnvEntry, EnvOp, Error, NodeInfo, NodesOp, RunnerInfo,
    RunnersOp, SessionEvent, SessionInfo, SessionsOp, WorkerCloneRequest, WorkerRequest,
};
use russh::keys::ssh_key::Algorithm;
use russh::keys::{Certificate, PrivateKey, PublicKey};
use tokio::sync::watch;

use super::settings;

/// Сколько запрос ждёт готового канала управления: пауза переподключения доходит до 30 с.
const CHANNEL_WAIT: Duration = Duration::from_secs(45);

/// Соединение worker с server. Ключ создан на этот запуск и живёт только в памяти.
pub(super) struct Control {
    server: Server,
    node: Node,
    key: PrivateKey,
    pub(super) channel: watch::Sender<Option<Mux>>,
    /// `.env`, из которого берутся автор коммитов и `GITHUB_TOKEN` новых сессий.
    env_file: PathBuf,
}

impl Control {
    pub(super) fn new(orbit_url: &str, orbit_ca: &str, orbit_key: &str, name: &str, env_file: PathBuf) -> Result<Self> {
        let (host, _) = orbit_url
            .rsplit_once(':')
            .with_context(|| format!("orbit_url {orbit_url} без порта: нужен хост:порт"))?;
        let ca = PublicKey::from_openssh(orbit_ca.trim()).context("orbit_ca не разбирается как открытый ключ OpenSSH")?;
        let server = Server {
            address: orbit_url.to_owned(),
            host: host.to_owned(),
            ca,
        };
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let role = Role::Worker { name: name.to_owned() };
        Ok(Self {
            node: Node::new(server.clone(), key.clone(), orbit_key.to_owned(), role, None),
            server,
            key,
            channel: watch::channel(None).0,
            env_file,
        })
    }

    /// Держит соединение с server и сертификат свежим, пока server не откажет в доступе (`orbit_key` заменён).
    pub(super) async fn run(&self) -> Denied {
        self.node
            .run(async |channel: Channel| {
                let mux = channel.mux.clone();
                self.channel.send_replace(Some(mux.clone()));
                tracing::info!("канал управления открыт");
                mux.closed().await;
                self.channel.send_replace(None);
            })
            .await
    }

    /// Ключ и действующий сертификат этого запуска: ими worker входит через jump. Сертификата нет, пока server
    /// его не выдал.
    pub(super) fn login(&self) -> Option<(&Server, &PrivateKey, Certificate)> {
        Some((&self.server, &self.key, self.node.certificate()?))
    }

    /// Ждёт открытого канала управления.
    async fn open_channel(&self) -> Result<Mux> {
        let mut channel = self.channel.subscribe();
        let ready = tokio::time::timeout(
            CHANNEL_WAIT,
            channel.wait_for(|mux| mux.as_ref().is_some_and(|mux| !mux.is_closed())),
        )
        .await
        .map_err(|_| anyhow!("нет связи с server"))??;
        ready.clone().ok_or_else(|| anyhow!("нет связи с server"))
    }

    /// Запрос к server. После обрыва идемпотентный запрос повторяется один раз по новому каналу, остальные — ошибка:
    /// неизвестно, выполнил ли их server.
    async fn ask<Resp: serde::de::DeserializeOwned>(&self, request: &WorkerRequest, idempotent: bool) -> Result<Resp> {
        let mux = self.open_channel().await?;
        match server::ask(&mux, request).await {
            Err(error) if mux.is_closed() && idempotent => {
                tracing::warn!(error = format!("{error:#}"), "канал управления оборвался, запрос повторяется");
                let mux = self.open_channel().await?;
                server::ask(&mux, request).await
            }
            result => result,
        }
    }
}

impl Control {
    /// Сессии с состоянием и лимитами.
    pub(super) async fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let response: Result<Vec<SessionInfo>, Error> = self.ask(&WorkerRequest::Sessions(SessionsOp::List), true).await?;
        response.map_err(|e| anyhow!("список сессий не получен: {e}"))
    }

    pub(super) async fn runners(&self) -> Result<Vec<RunnerInfo>> {
        let response: Result<Vec<RunnerInfo>, Error> = self.ask(&WorkerRequest::Runners(RunnersOp::List), true).await?;
        response.map_err(|e| anyhow!("список runner не получен: {e}"))
    }

    /// Поднимает сессию: server отвечает потоком событий, `Ready` — можно входить. Повтор поднял бы вторую сессию
    /// агента, поэтому после обрыва — ошибка.
    pub(super) async fn start_session(&self, agent: Agent, runner: &Fingerprint) -> Result<Replies<SessionEvent>> {
        let request = WorkerRequest::Sessions(SessionsOp::Start {
            agent,
            runner: runner.clone(),
            identity: settings::session_identity(&self.env_file)?,
        });
        let mux = self.open_channel().await?;
        server::subscribe(&mux, &request).await
    }

    /// Завершает сессию; повтор безвреден.
    pub(super) async fn end_session(&self, session: &Fingerprint) -> Result<()> {
        let response: Result<(), Error> = self.ask(&WorkerRequest::Sessions(SessionsOp::End(session.clone())), true).await?;
        response.map_err(|e| anyhow!("сессия не завершена: {e}"))
    }

    /// Перезапускает сессию из `failure`.
    pub(super) async fn restart_session(&self, session: &Fingerprint) -> Result<()> {
        let response: Result<(), Error> = self
            .ask(&WorkerRequest::Sessions(SessionsOp::Restart(session.clone())), true)
            .await?;
        response.map_err(|e| anyhow!("сессия не перезапущена: {e}"))
    }

    /// Требует перезапустить работающего агента с продолжением разговора: первый запрос ждёт конца хода, повторный,
    /// пока перезапуск ждёт, перезапускает сразу. Повтор запроса после обрыва был бы таким повторным требованием,
    /// поэтому после обрыва — ошибка.
    pub(super) async fn restart_agent(&self, session: &Fingerprint) -> Result<()> {
        let response: Result<(), Error> = self
            .ask(&WorkerRequest::Sessions(SessionsOp::RestartAgent(session.clone())), false)
            .await?;
        response.map_err(|e| anyhow!("агент не перезапущен: {e}"))
    }

    /// Назначает сессии другой runner; `resume` — продолжить разговор агента. Повтор с тем же runner ничего не меняет,
    /// если агент работает.
    pub(super) async fn assign_runner(&self, session: &Fingerprint, runner: &Fingerprint, resume: bool) -> Result<()> {
        let request = WorkerRequest::Runners(RunnersOp::Assign {
            session: session.clone(),
            runner: runner.clone(),
            resume,
        });
        let response: Result<(), Error> = self.ask(&request, true).await?;
        response.map_err(|e| anyhow!("runner не сменён: {e}"))
    }

    /// Создаёт клон `runner` в каталоге `dir` той же машины. Повтор создал бы второй клон, поэтому после обрыва — ошибка.
    pub(super) async fn clone_runner(&self, runner: &Fingerprint, dir: &str) -> Result<CloneReply> {
        let request = WorkerCloneRequest {
            runner: runner.clone(),
            request: CloneRequest { dir: dir.to_owned() },
        };
        let response: CloneResponse = self.ask(&WorkerRequest::Runners(RunnersOp::Clone(request)), false).await?;
        response.map_err(|e| anyhow!("клон не создан: {e}"))
    }

    /// Переименовывает runner в реестре server; повтор даёт то же имя.
    pub(super) async fn rename_runner(&self, runner: &Fingerprint, name: &str) -> Result<()> {
        let request = WorkerRequest::Runners(RunnersOp::Rename {
            runner: runner.clone(),
            name: name.to_owned(),
        });
        let response: Result<(), Error> = self.ask(&request, true).await?;
        response.map_err(|e| anyhow!("runner не переименован: {e}"))
    }
}

impl Control {
    /// Узлы: runner с ОС и пометкой отзыва.
    pub(super) async fn nodes(&self) -> Result<Vec<NodeInfo>> {
        let response: Result<Vec<NodeInfo>, Error> = self.ask(&WorkerRequest::Nodes(NodesOp::List), true).await?;
        response.map_err(|e| anyhow!("список узлов не получен: {e}"))
    }

    /// Отзывает runner; повтор безвреден.
    pub(super) async fn revoke_node(&self, runner: &Fingerprint) -> Result<()> {
        let response: Result<(), Error> = self.ask(&WorkerRequest::Nodes(NodesOp::Revoke(runner.clone())), true).await?;
        response.map_err(|e| anyhow!("runner не отозван: {e}"))
    }

    /// Выдаёт новый `orbit_key` и возвращает его секрет. Повтор выдал бы ещё один ключ и обесценил первый, поэтому
    /// после обрыва — ошибка.
    pub(super) async fn new_orbit_key(&self) -> Result<String> {
        let response: Result<String, Error> = self.ask(&WorkerRequest::Nodes(NodesOp::NewOrbitKey), false).await?;
        response.map_err(|e| anyhow!("orbit_key не выдан: {e}"))
    }

    /// Записи окружения.
    pub(super) async fn env(&self) -> Result<Vec<EnvEntry>> {
        let response: Result<Vec<EnvEntry>, Error> = self.ask(&WorkerRequest::Env(EnvOp::List), true).await?;
        response.map_err(|e| anyhow!("окружение не получено: {e}"))
    }

    /// Записывает переменную окружения: `key` — `all.VAR` или `<fingerprint>.VAR`.
    pub(super) async fn set_env(&self, key: &str, value: &str) -> Result<()> {
        let change = EnvChange::Set {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        self.change_env(change).await
    }

    /// Удаляет переменную окружения.
    pub(super) async fn delete_env(&self, key: &str) -> Result<()> {
        self.change_env(EnvChange::Delete { key: key.to_owned() }).await
    }

    /// Изменение окружения идемпотентно: запись и удаление по ключу можно повторить.
    async fn change_env(&self, change: EnvChange) -> Result<()> {
        let response: EnvChangeResponse = self.ask(&WorkerRequest::Env(EnvOp::Change(change)), true).await?;
        response.map_err(|e| anyhow!("server не изменил окружение: {e}"))
    }
}
