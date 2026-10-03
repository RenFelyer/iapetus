//! Указания server session: назначение runner переключает канал данных, FUSE и очередь указаний агенту.

use std::path::PathBuf;

use nerve::agent::Agent;
use nerve::protocol::{Artifact, ArtifactFormat, Assignment, Digest, Platform, Root, RunnerHost};
use russh::keys::PrivateKey;
use russh::keys::ssh_key::Algorithm;
use tempfile::TempDir;

use super::*;
use crate::session::agent::{Profile as AgentProfile, Setup};
use crate::session::fs::{Config, Epoch, Owner, Profile};
use crate::session::launch::AgentUser;
use nerve::protocol::{McpServer, McpTool};

fn fingerprint(name: &str) -> Fingerprint {
    name.parse().unwrap()
}

fn assigned(runner: &Fingerprint) -> AssignedRunner {
    AssignedRunner {
        runner: runner.clone(),
        assignment: Assignment(1),
        name: "r".to_owned(),
        os: "linux".to_owned(),
        host: host("/srv/project"),
        env_names: Vec::new(),
    }
}

/// Окружение runner без hostname, `os-release` и `HOME`: тест не меняет контейнер, в котором запущен.
fn host(project: &str) -> RunnerHost {
    RunnerHost {
        project: project.to_owned(),
        hostname: String::new(),
        os_release: String::new(),
        home: String::new(),
        home_origin: String::new(),
        home_served: false,
    }
}

fn assign(runner: Option<&Fingerprint>, agent: AgentRun) -> Control {
    Control::AssignRunner {
        runner: runner.map(assigned),
        agent,
    }
}

struct Fixture {
    link: Link,
    commands: mpsc::Receiver<AgentCommand>,
    epoch: Epoch,
    old: Fingerprint,
    new: Fingerprint,
    cache: TempDir,
}

/// Session, которой назначен runner `old`: канал данных, FUSE и очередь указаний агенту настоящие, сервера нет.
fn fixture() -> Fixture {
    let (old, new) = (fingerprint("runner-old"), fingerprint("runner-new"));
    let (data, data_link, _events) = Data::new();
    data.assign(Some(old.clone()));
    let cache = tempfile::tempdir().unwrap();
    let epoch = Epoch::new();
    let config = Config {
        profile: Profile::Rename,
        temp_name: None,
        owner: Owner { uid: 1000, gid: 1000 },
        fingerprint: old.clone(),
        root: PathBuf::from("/workspace"),
        cache_dir: cache.path().join("work"),
        max_file: 1 << 20,
        request_timeout: Duration::from_secs(30),
        hidden: Vec::new(),
        root_kind: Root::Project,
        links: Vec::new(),
    };
    let project = Project::new(data_link.clone(), 64);
    let home_config = Config {
        root_kind: Root::Home,
        root: PathBuf::from("/home/runner"),
        ..config.clone()
    };
    let home_vfs = Arc::new(Vfs::new(data_link.clone(), home_config, epoch.clone()).unwrap());
    let vfs = Arc::new(Vfs::new(data_link, config, epoch.clone()).unwrap());
    let workspace = Arc::new(Workspace::new(
        Arc::clone(&vfs),
        home_vfs,
        cache.path().join("state"),
        cache.path().join("etc"),
        1000,
        Vec::new(),
    ));
    let (commands_tx, commands) = mpsc::channel(4);
    let link = Link {
        server: Server {
            address: "127.0.0.1:1".to_owned(),
            host: "orbit".to_owned(),
            ca: PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
                .unwrap()
                .public_key()
                .clone(),
        },
        session: fingerprint("session-1"),
        secret: "secret".to_owned(),
        data,
        vfs,
        runner: watch::channel(Some(old.clone())).0,
        activity: watch::channel(Activity::default()).1,
        agent: watch::channel(false).1,
        state: watch::channel(Verdict::waiting()).1,
        rules: None,
        project,
        commands: commands_tx,
        profile: AgentProfile(Agent::ClaudeCode),
        agent_update: Ok(watch::channel(None).0),
        workspace,
        environment: watch::channel(None).0,
    };
    Fixture {
        link,
        commands,
        epoch,
        old,
        new,
        cache,
    }
}

#[test]
fn restart_for_a_new_runner_switches_data_channel_and_cache_and_queues_the_restart() {
    for resume in [true, false] {
        let mut f = fixture();
        let epoch = f.epoch.current();

        let response = f.link.control(assign(Some(&f.new), AgentRun::Restart { resume }));

        assert_eq!(response, Ok(()));
        assert!(f.link.data.is_assigned(&f.new), "канал данных не принимает нового runner");
        assert!(!f.link.data.is_assigned(&f.old), "канал данных всё ещё принимает прежнего runner");
        assert_eq!(*f.link.runner.borrow(), Some(f.new.clone()), "назначение не опубликовано");
        assert!(f.epoch.current() > epoch, "кэш FUSE не перешёл к новой эпохе");
        assert!(
            matches!(f.commands.try_recv(), Ok(AgentCommand::Restart { resume: queued }) if queued == resume),
            "агенту не поставлен перезапуск с resume = {resume}"
        );
        assert!(f.commands.try_recv().is_err(), "в очереди лишнее указание");
    }
}

#[test]
fn announcement_of_the_current_runner_touches_neither_agent_nor_cache() {
    let mut f = fixture();
    let epoch = f.epoch.current();

    let response = f.link.control(assign(Some(&f.old), AgentRun::Keep));

    assert_eq!(response, Ok(()));
    assert!(f.link.data.is_assigned(&f.old));
    assert_eq!(f.epoch.current(), epoch, "то же назначение сбросило кэш FUSE");
    assert!(f.commands.try_recv().is_err(), "то же назначение тронуло агента");
}

#[test]
fn announcement_of_another_runner_switches_without_restarting_the_agent() {
    let mut f = fixture();

    let response = f.link.control(assign(Some(&f.new), AgentRun::Keep));

    assert_eq!(response, Ok(()));
    assert!(f.link.data.is_assigned(&f.new), "канал данных не перешёл к runner из сообщения");
    assert!(f.commands.try_recv().is_err(), "сообщение о назначении перезапустило агента");
}

#[test]
fn stop_keeps_the_runner_and_unassigning_drops_it_but_both_stop_the_agent() {
    let mut f = fixture();

    assert_eq!(f.link.control(assign(Some(&f.old), AgentRun::Stop)), Ok(()));
    assert!(matches!(f.commands.try_recv(), Ok(AgentCommand::Stop)), "Stop не остановил агента");
    assert!(
        f.link.data.is_assigned(&f.old),
        "остановка агента сняла runner: вернувшийся runner не подключится"
    );

    assert_eq!(f.link.control(assign(None, AgentRun::Keep)), Ok(()));
    assert!(
        matches!(f.commands.try_recv(), Ok(AgentCommand::Stop)),
        "снятие назначения не остановило агента"
    );
    assert!(
        !f.link.data.is_assigned(&f.old),
        "после снятия назначения канал данных принимает runner"
    );
    assert_eq!(*f.link.runner.borrow(), None);
}

fn update_agent(agent: Agent, version: &str, url: &str) -> Control {
    Control::UpdateAgent(AgentRelease {
        agent,
        version: version.to_owned(),
        artifacts: vec![Artifact {
            platform: Platform::current().unwrap(),
            url: url.to_owned(),
            digest: Digest::Sha256("a".repeat(64)),
            format: ArtifactFormat::Binary,
        }],
    })
}

fn update_release(version: &str) -> Control {
    update_agent(Agent::ClaudeCode, version, &format!("https://example.org/{version}/claude"))
}

#[test]
fn agent_update_is_remembered_for_this_platform_and_a_newer_one_replaces_it() {
    let f = fixture();
    let pending = f.link.agent_update.as_ref().unwrap().subscribe();

    assert_eq!(f.link.control(update_release("2.1.285")), Ok(()));
    let offer = pending.borrow().clone().expect("указание не запомнено");
    assert_eq!(
        (offer.version.as_str(), offer.url.as_str()),
        ("2.1.285", "https://example.org/2.1.285/claude")
    );

    assert_eq!(f.link.control(update_release("2.1.286")), Ok(()));
    let offer = pending.borrow().clone().unwrap();
    assert_eq!(offer.version, "2.1.286", "новое не заменило прежнее");
}

#[test]
fn agent_update_with_an_unusable_artifact_is_refused_and_not_remembered() {
    let f = fixture();
    assert!(
        f.link
            .control(update_agent(Agent::ClaudeCode, "2.1.285", "http://example.org/claude"))
            .is_err(),
        "принят адрес без https"
    );
    assert!(
        f.link.agent_update.as_ref().unwrap().borrow().is_none(),
        "отвергнутое указание запомнено"
    );
}

#[test]
fn release_for_another_agent_is_refused_with_other_agent_and_never_remembered() {
    let f = fixture();
    let request = update_agent(Agent::Codex, "2.1.285", "https://example.org/2.1.285/codex");
    assert_eq!(f.link.control(request), Err(Error::Io(release::OTHER_AGENT.to_owned())));
    assert!(
        f.link.agent_update.as_ref().unwrap().borrow().is_none(),
        "выпуск чужого агента запомнен"
    );
}

#[test]
fn agent_update_refusal_tells_a_profile_without_updates_from_a_missing_command() {
    let mut f = fixture();
    let request = || update_release("2.1.285");

    f.link.agent_update = Err(release::NOT_UPDATABLE);
    assert_eq!(f.link.control(request()), Err(Error::Io(release::NOT_UPDATABLE.to_owned())));

    f.link.agent_update = Err(release::NO_AGENT_COMMAND);
    assert_eq!(f.link.control(request()), Err(Error::Io(release::NO_AGENT_COMMAND.to_owned())));
    assert_ne!(release::NOT_UPDATABLE, release::NO_AGENT_COMMAND, "отказы не различимы");
    assert_ne!(release::NOT_UPDATABLE, release::OTHER_AGENT, "отказы не различимы");
}

#[test]
fn the_same_agent_update_again_does_not_touch_the_pending_one_and_a_different_one_does() {
    let f = fixture();
    let mut pending = f.link.agent_update.as_ref().unwrap().subscribe();

    assert_eq!(f.link.control(update_release("2.1.285")), Ok(()));
    assert!(pending.has_changed().unwrap());
    pending.mark_unchanged();

    assert_eq!(f.link.control(update_release("2.1.285")), Ok(()));
    assert!(
        !pending.has_changed().unwrap(),
        "повторное то же предложение отменило бы идущую загрузку"
    );

    assert_eq!(f.link.control(update_release("2.1.286")), Ok(()));
    assert!(pending.has_changed().unwrap(), "другое предложение не заменило ожидающее");
}

fn server(name: &str, instructions: Option<&str>) -> McpServer {
    McpServer {
        name: name.to_owned(),
        instructions: instructions.map(str::to_owned),
        tools: vec![McpTool {
            name: "run".to_owned(),
            description: String::new(),
            input_schema: "{}".to_owned(),
        }],
        resources: Vec::new(),
        prompts: Vec::new(),
    }
}

/// Файл правил Claude Code во временном каталоге `root`, подключённый к `f`; возвращает путь файла.
fn attach_rules(f: &mut Fixture, root: &std::path::Path) -> PathBuf {
    let setup = Setup {
        gateway: "127.0.0.1:8080".parse().unwrap(),
        shell_socket: root.join("shell.sock"),
        etc: root.join("etc"),
        mcp: None,
        project_mcp_port: 41000,
        user: AgentUser {
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            state_root: root.join("home"),
        },
    };
    let (path, _) = AgentProfile(Agent::ClaudeCode).rules_file(&setup);
    let rules = Rules::new(AgentProfile(Agent::ClaudeCode), setup, Some(f.old.clone()));
    rules.write().unwrap();
    f.link.rules = Some(rules);
    path
}

/// Имена инструментов, которые проектный MCP отдаёт агенту по HTTP.
async fn tools(f: &Fixture) -> Vec<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(crate::session::mcp::serve(listener, f.link.project.clone()));
    let request = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let reply = reqwest::Client::new()
        .post(format!("http://{address}{}", crate::session::mcp::PATH))
        .header("content-type", "application/json")
        .body(request.to_string())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn mcp_catalog_gives_tools_and_instructions_and_an_empty_one_takes_them_away() {
    let mut f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = attach_rules(&mut f, dir.path());

    let catalog = McpCatalog {
        servers: vec![
            server("a", Some("Сервер a: сначала вызови setup")),
            server("b", None),
            server("c", Some("Сервер c")),
        ],
    };
    assert_eq!(f.link.control(Control::McpCatalog(catalog)), Ok(()));

    assert_eq!(
        tools(&f).await,
        ["a__run", "b__run", "c__run"],
        "инструменты сводки не доступны агенту"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let (a, c) = (
        text.find("## MCP-сервер `a`\n\nСервер a: сначала вызови setup"),
        text.find("## MCP-сервер `c`\n\nСервер c"),
    );
    assert!(
        a.is_some() && c.is_some() && a < c,
        "instructions не попали в правила по разделу на сервер: {text}"
    );
    assert!(
        !text.contains("MCP-сервер `b`"),
        "раздел появился у сервера без instructions: {text}"
    );

    assert_eq!(f.link.control(Control::McpCatalog(McpCatalog::default())), Ok(()));
    assert!(tools(&f).await.is_empty(), "пустая сводка не сняла инструменты");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("MCP-сервер") && !text.contains("Сервер a"),
        "instructions остались после снятия сводки: {text}"
    );
}

#[tokio::test]
async fn oversized_mcp_catalog_is_rejected_and_the_previous_one_stays() {
    let mut f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = attach_rules(&mut f, dir.path());
    assert_eq!(
        f.link.control(Control::McpCatalog(McpCatalog {
            servers: vec![server("a", Some("прежние инструкции"))]
        })),
        Ok(())
    );

    let huge = McpCatalog {
        servers: vec![server("big", Some(&"я".repeat(MAX_MCP_CATALOG)))],
    };
    let response = f.link.control(Control::McpCatalog(huge));

    assert!(matches!(response, Err(Error::Io(_))), "большая сводка принята: {response:?}");
    assert_eq!(tools(&f).await, ["a__run"], "отвергнутая сводка не оставила прежнюю");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("прежние инструкции") && !text.contains("big"),
        "правила изменены отвергнутой сводкой: {text}"
    );
}

#[tokio::test]
async fn another_runner_or_none_drops_the_catalog_and_its_instructions_but_the_same_runner_keeps_them() {
    for (next, dropped) in [(Some("runner-new"), true), (None, true), (Some("runner-old"), false)] {
        let mut f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = attach_rules(&mut f, dir.path());
        let catalog = McpCatalog {
            servers: vec![server("a", Some("инструкции a"))],
        };
        assert_eq!(f.link.control(Control::McpCatalog(catalog)), Ok(()));

        let next = next.map(fingerprint);
        assert_eq!(f.link.control(assign(next.as_ref(), AgentRun::Keep)), Ok(()));

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(tools(&f).await.is_empty(), dropped, "сводка и назначение {next:?} не согласованы");
        assert_eq!(
            !text.contains("инструкции a"),
            dropped,
            "instructions и назначение {next:?} не согласованы: {text}"
        );
    }
}

fn assign_project(runner: &Fingerprint, project: &str, agent: AgentRun) -> Control {
    let mut assigned = assigned(runner);
    assigned.host = host(project);
    Control::AssignRunner {
        runner: Some(assigned),
        agent,
    }
}

#[tokio::test]
async fn assignment_with_a_project_path_that_cannot_be_mounted_changes_nothing() {
    for project in ["relative/project", "/srv/../usr", "/usr/project", "<state>/project", "/"] {
        let mut f = fixture();
        let project = project.replace("<state>", f.cache.path().join("state").to_str().unwrap());
        let project = project.as_str();
        let epoch = f.epoch.current();

        let response = f.link.control(assign_project(&f.new, project, AgentRun::Restart { resume: true }));

        assert!(matches!(response, Err(Error::Io(_))), "путь {project:?} принят: {response:?}");
        assert!(
            f.link.data.is_assigned(&f.old),
            "канал данных переключён назначением с путём {project:?}"
        );
        assert_eq!(
            *f.link.runner.borrow(),
            Some(f.old.clone()),
            "назначение с путём {project:?} опубликовано"
        );
        assert_eq!(f.epoch.current(), epoch, "кэш FUSE тронут назначением с путём {project:?}");
        assert!(
            f.commands.try_recv().is_err(),
            "агенту поставлено указание из отвергнутого назначения"
        );
    }
}

#[tokio::test]
async fn announcement_with_another_project_path_restarts_the_agent_with_resume() {
    let mut f = fixture();

    let first = f.link.control(assign_project(&f.old, "/srv/one", AgentRun::Keep));
    assert_eq!(first, Ok(()));
    assert!(f.commands.try_recv().is_err(), "первое назначение тронуло агента");
    assert_eq!(f.link.control(assign_project(&f.old, "/srv/one", AgentRun::Keep)), Ok(()));
    assert!(f.commands.try_recv().is_err(), "тот же путь перезапустил агента");

    let moved = f.link.control(assign_project(&f.old, "/srv/two", AgentRun::Keep));

    assert_eq!(moved, Ok(()));
    assert!(
        matches!(f.commands.try_recv(), Ok(AgentCommand::Restart { resume: true })),
        "смена пути проекта не перезапустила агента с resume"
    );
}
