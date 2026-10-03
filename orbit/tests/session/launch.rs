//! Окружение настоящего процесса агента: запуск идёт через бинарь `orbit drop-exec`.
//!
//! Понижение прав работает, только когда тесты идут от root: иначе тест ничего не проверяет и выходит.

use super::*;

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// Бинарь `orbit` рядом с тестовым: тесты бинарного крейта лежат в `target/<профиль>/deps`, бинарь — выше.
fn orbit_binary() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    test.parent().unwrap().parent().unwrap().join("orbit")
}

#[tokio::test]
async fn agent_does_not_inherit_secrets_or_session_variables() {
    let inherited = pairs(&[
        ("PATH", "/usr/bin:/bin"),
        ("IAPETUS_SESSION_SECRET_CONTENT", "secret-value"),
        ("IAPETUS_DATA_HOST_KEY_CONTENT", "host-key-value"),
        ("IAPETUS_DATA_HOST_CERT_CONTENT", "cert-value"),
        ("IAPETUS_CA_PUBLIC_KEY_CONTENT", "ca-value"),
        ("IAPETUS_SESSION_SECRET_FILE", "/run/secrets/session-secret"),
        ("IAPETUS_ORBIT_URL", "orbit:2201"),
    ]);
    let env = agent_environment(inherited.into_iter(), &pairs(&[("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080")]));
    let user = AgentUser {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        state_root: "/tmp".into(),
    };
    if user.uid != 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("environ");
    let cwd_out = dir.path().join("cwd");
    let script = format!("pwd -P > {}; exec cat /proc/self/environ > {}", cwd_out.display(), out.display());
    let (mut child, _pty) = spawn(
        &orbit_binary(),
        &user,
        Path::new("/tmp"),
        dir.path(),
        &env,
        &["sh".to_owned(), "-c".to_owned(), script],
    )
    .unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success(), "агент не отработал: {status}");

    let cwd = std::fs::read_to_string(&cwd_out).unwrap();
    assert_eq!(
        cwd.trim(),
        dir.path().canonicalize().unwrap().to_str().unwrap(),
        "агент запущен не в рабочем каталоге"
    );
    let environ = std::fs::read(&out).unwrap();
    let names: Vec<String> = environ
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).split('=').next().unwrap().to_owned())
        .collect();
    for name in &names {
        assert!(!name.starts_with("IAPETUS_"), "агент получил переменную сессии {name}");
        assert!(!name.starts_with("CARGO_"), "агент унаследовал окружение процесса session: {name}");
    }
    for name in ["PATH", "ANTHROPIC_BASE_URL", "TERM", "HOME"] {
        assert!(names.iter().any(|own| own == name), "агенту не хватает {name}: {names:?}");
    }
}

#[test]
fn profile_variables_override_inherited_ones() {
    let env = agent_environment(pairs(&[("PATH", "/usr/bin")]).into_iter(), &pairs(&[("PATH", "/opt/agy")]));
    assert_eq!(env, pairs(&[("PATH", "/opt/agy")]), "PATH профиля не заменил унаследованный");
}

/// Через сколько после подключения runner агент стартует, если сводка проектных MCP приходит через `catalog_after`
/// (`None` — не приходит); часы остановлены. Старт — заглушка, которая сразу завершает `run_after_runner` ошибкой.
async fn agent_start_delay(catalog_after: Option<Duration>) -> Duration {
    let (session_side, runner_side) = tokio::io::duplex(1024);
    let (mux, _requests) = nerve::mux::Mux::new(session_side);
    let _runner = nerve::mux::Mux::new(runner_side);
    let (mut link, _switch) = Link::fixed(Some(mux));
    let project = Project::new(link.clone(), 64);
    if let Some(after) = catalog_after {
        let project = project.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            project.set(nerve::protocol::McpCatalog::default());
        });
    }
    let (_commands, commands) = mpsc::channel(1);
    let (_turn, turn) = watch::channel(false);
    let (_viewers, viewers) = watch::channel(0);
    let planned = AtomicBool::new(false);
    let restarts = Restarts {
        commands,
        turn,
        viewers,
        planned_exit: &planned,
        project,
        environment: watch::channel(Some(environment())).1,
    };
    let began = Instant::now();
    let started = std::cell::Cell::new(None);
    let start = |_resumed: bool| {
        started.set(Some(began.elapsed()));
        Err(anyhow::anyhow!("заглушка вместо агента"))
    };
    let events = mpsc::unbounded_channel().0;
    let launched = run_after_runner(
        &mut link,
        start,
        false,
        &watch::channel(None).0,
        &watch::channel(false).0,
        &events,
        restarts,
    )
    .await;
    assert!(launched.is_err(), "заглушка не остановила запуск");
    started.get().expect("агент не запущен")
}

#[tokio::test(start_paused = true)]
async fn agent_starts_when_the_first_catalog_arrives_and_without_it_after_the_limit() {
    assert_eq!(
        agent_start_delay(Some(Duration::ZERO)).await,
        Duration::ZERO,
        "готовая сводка задержала запуск агента"
    );
    assert_eq!(
        agent_start_delay(Some(Duration::from_secs(5))).await,
        Duration::from_secs(5),
        "агент стартовал не по приходу сводки"
    );
    assert_eq!(
        agent_start_delay(None).await,
        crate::session::mcp::FIRST_CATALOG_WAIT,
        "без сводки агент стартовал не по пределу ожидания"
    );
}

fn environment() -> Environment {
    Environment {
        project: "/srv/project".into(),
        home: "/home/runner".into(),
        home_fs: false,
        home_alias: None,
    }
}

#[tokio::test(start_paused = true)]
async fn three_exits_within_the_window_are_a_loop_and_spaced_ones_are_not() {
    let mut exits = Exits::default();
    let begin = Instant::now();
    assert!(!exits.record(begin));
    assert!(!exits.record(begin + Duration::from_secs(30)));
    assert!(
        exits.record(begin + Duration::from_secs(59)),
        "три выхода за минуту не признаны циклом"
    );

    let mut exits = Exits::default();
    for minute in 0..10 {
        assert!(
            !exits.record(begin + Duration::from_secs(61 * minute)),
            "редкие выходы признаны циклом на выходе {minute}"
        );
    }
}

/// Процесс, который сразу завершается успешно, в своём pty.
fn exiting_agent() -> anyhow::Result<(tokio::process::Child, Pty)> {
    let (pty, pts) = pty_process::open()?;
    let child = pty_process::Command::new("true").spawn(pts)?;
    Ok((child, pty))
}

#[tokio::test(start_paused = true)]
async fn agent_that_exits_by_itself_is_restarted_with_resume_until_it_loops() {
    let (session_side, runner_side) = tokio::io::duplex(1024);
    let (mux, _requests) = nerve::mux::Mux::new(session_side);
    let _runner = nerve::mux::Mux::new(runner_side);
    let (mut link, _switch) = Link::fixed(Some(mux));
    let project = Project::new(link.clone(), 64);
    project.set(nerve::protocol::McpCatalog::default());
    let (_commands, commands) = mpsc::channel(1);
    let (_turn, turn) = watch::channel(false);
    let (_viewers, viewers) = watch::channel(0);
    let planned = AtomicBool::new(false);
    let restarts = Restarts {
        commands,
        turn,
        viewers,
        planned_exit: &planned,
        project,
        environment: watch::channel(Some(environment())).1,
    };
    let starts = std::cell::RefCell::new(Vec::new());
    let start = |resumed: bool| {
        starts.borrow_mut().push(resumed);
        exiting_agent()
    };
    let (events, mut seen) = mpsc::unbounded_channel();
    let terminal = watch::channel(None).0;
    let running = watch::channel(false).0;
    let run = run_after_runner(&mut link, start, false, &terminal, &running, &events, restarts);
    let mut history = Vec::new();
    tokio::select! {
        never = run => { never.unwrap(); }
        () = async {
            while let Some(event) = seen.recv().await {
                history.push(event);
                if event == AgentEvent::Exited {
                    break;
                }
            }
        } => {}
    }
    assert_eq!(
        history,
        [
            AgentEvent::Started,
            AgentEvent::Relaunched,
            AgentEvent::Started,
            AgentEvent::Relaunched,
            AgentEvent::Started,
            AgentEvent::Exited,
        ],
        "агент не перезапускался до цикла"
    );
    assert_eq!(*starts.borrow(), [false, true, true], "перезапуск не продолжил разговор");
}
