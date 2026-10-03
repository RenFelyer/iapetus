use insta::assert_snapshot;

use super::*;

const PROJECT: &str = "/home/agent/proj";

fn current_user(home: PathBuf) -> AgentUser {
    AgentUser {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        state_root: home,
    }
}

fn setup(root: &Path) -> Setup {
    Setup {
        gateway: "127.0.0.1:8080".parse().unwrap(),
        shell_socket: "/run/iapetus/shell.sock".into(),
        etc: root.join("etc"),
        mcp: None,
        project_mcp_port: 41000,
        user: current_user(root.join("home")),
    }
}

fn file_at<'a>(files: &'a [ConfigFile], suffix: &str) -> &'a ConfigFile {
    files
        .iter()
        .find(|file| file.path.ends_with(suffix))
        .unwrap_or_else(|| panic!("профиль не создаёт файл {suffix}"))
}

/// Кавычка или обратная косая черта в пути не должны разорвать ключ таблицы: иначе доверие каталогу не сработает.
#[test]
fn codex_config_escapes_the_trusted_path() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let files = Profile(Agent::Codex).files(&setup, Path::new(r#"/work/a"b\c"#));
    let config = &file_at(&files, ".codex/config.toml").content;
    assert!(
        config.contains(r#"[projects."/work/a\"b\\c"]"#),
        "путь в ключе таблицы не экранирован: {config}"
    );
}

#[test]
fn codex_requirements_allow_only_the_project_mcp_without_iapetus_mcp_and_belong_to_root() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::Codex).files(&setup(dir.path()), Path::new(PROJECT));
    let requirements = file_at(&files, "codex/requirements.toml");
    assert_snapshot!(requirements.content, @r#"
    [mcp_servers.iapetus-project]
    identity = { url = "http://127.0.0.1:41000/mcp" }
    "#);
    assert!(!requirements.owned_by_user, "агент мог бы изменить запрет MCP");
    assert_eq!(requirements.mode & 0o022, 0, "запрет MCP доступен на запись не только владельцу");
}

#[test]
fn prepare_writes_codex_files_privately_and_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    prepare(Profile(Agent::Codex), &setup, Path::new(PROJECT)).unwrap();

    let home = &setup.user.state_root;
    assert!(
        std::fs::read_to_string(home.join(".codex/config.toml"))
            .unwrap()
            .contains("danger-full-access")
    );
    assert_eq!(
        std::fs::read_to_string(setup.etc.join("codex/requirements.toml")).unwrap(),
        "[mcp_servers.iapetus-project]\nidentity = { url = \"http://127.0.0.1:41000/mcp\" }\n"
    );
    let mode = std::fs::metadata(home.join(".codex")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "каталог состояния агента доступен не только владельцу");
}

/// Вход выполняет пользователь в самом агенте: профиль не кладёт ни файла входа, ни токена.
#[test]
fn prepare_leaves_the_login_to_the_user() {
    for (agent, login_files) in [
        (Profile(Agent::ClaudeCode), &[".claude/.credentials.json"][..]),
        (Profile(Agent::Codex), &[".codex/auth.json"][..]),
        (
            Profile(Agent::Antigravity),
            &[".gemini/antigravity-cli/antigravity-oauth-token"][..],
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let setup = setup(dir.path());
        prepare(agent, &setup, Path::new(PROJECT)).unwrap();
        for file in login_files {
            assert!(
                !setup.user.state_root.join(file).exists(),
                "{agent:?}: профиль создал файл входа {file}"
            );
        }
    }
}

fn with_mcp(root: &Path) -> Setup {
    let mut setup = setup(root);
    setup.mcp = Some(Mcp {
        url: "http://orbit-server:2202/mcp".to_owned(),
        token: "session-1.deadbeef".parse().unwrap(),
    });
    setup
}

#[test]
fn claude_managed_mcp_allows_only_iapetus_and_belongs_to_root() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::ClaudeCode).files(&with_mcp(dir.path()), Path::new(PROJECT));
    let managed = file_at(&files, "claude-code/managed-mcp.json");
    assert_snapshot!(managed.content, @r#"
    {
      "mcpServers": {
        "iapetus-project": {
          "type": "http",
          "url": "http://127.0.0.1:41000/mcp"
        },
        "iapetus": {
          "type": "http",
          "url": "http://orbit-server:2202/mcp",
          "headers": {
            "Authorization": "Bearer session-1.deadbeef"
          }
        }
      }
    }
    "#);
    assert!(!managed.owned_by_user, "агент мог бы дописать в managed-mcp.json свои серверы");
    assert_eq!(managed.mode & 0o022, 0, "managed-mcp.json доступен на запись не только владельцу");
}

#[test]
fn claude_without_orbit_mcp_still_gets_the_project_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::ClaudeCode).files(&setup(dir.path()), Path::new(PROJECT));
    let servers = serde_json::from_str::<serde_json::Value>(&file_at(&files, "claude-code/managed-mcp.json").content).unwrap();
    assert_eq!(
        servers["mcpServers"],
        serde_json::json!({ "iapetus-project": { "type": "http", "url": "http://127.0.0.1:41000/mcp" } }),
        "без MCP Iapetus агент остался без проектного MCP или получил лишний сервер"
    );
}

/// Без этих ключей Claude Code показывает вступление и выходит с ошибкой, если нет связи с `api.anthropic.com`;
/// ключи, которые он сам дописал в томе (разговоры, настройки), переживают перезапуск контейнера.
#[test]
fn claude_first_run_keys_are_merged_into_the_state_in_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let config_dir = setup.user.state_root.join(".claude");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join(".claude.json"),
        r#"{"userID":"keep-me","projects":{"/":{"allowedTools":["x"]},"/work":{"hasTrustDialogAccepted":false}}}"#,
    )
    .unwrap();
    std::fs::write(
        config_dir.join("settings.json"),
        r#"{"model":"keep-me","permissions":{"allow":["Bash"]}}"#,
    )
    .unwrap();

    let files = Profile(Agent::ClaudeCode).files(&setup, Path::new(PROJECT));
    let state: serde_json::Value = serde_json::from_str(&file_at(&files, ".claude/.claude.json").content).unwrap();
    assert_eq!(state["userID"], "keep-me", "ключ, который Claude Code записал в томе, потерян");
    assert_eq!(
        state["projects"]["/"]["allowedTools"],
        serde_json::json!(["x"]),
        "вложенный ключ потерян"
    );
    assert_eq!(state["projects"]["/work"]["hasTrustDialogAccepted"], false, "чужой проект изменён");
    for key in ["hasCompletedOnboarding", "hasSeenAutoDefaultNudge", "hasSeenAutoDefaultNotice"] {
        assert_eq!(state[key], true, "{key} не выставлен: Claude Code покажет вступление");
    }
    assert_eq!(
        state["projects"]["/"]["hasTrustDialogAccepted"], true,
        "каталог не доверенный: Claude Code спросит"
    );
    assert_eq!(
        state["projects"]["/home/agent/proj"]["hasClaudeMdExternalIncludesApproved"], true,
        "импорты CLAUDE.md вне проекта не разрешены"
    );
    assert_eq!(
        state["projects"]["/home/agent/proj"]["hasTrustDialogAccepted"], true,
        "проект не доверенный: в репозитории и worktree доверие по `/` не действует, Claude Code спросит"
    );

    let settings: serde_json::Value = serde_json::from_str(&file_at(&files, ".claude/settings.json").content).unwrap();
    assert_eq!(settings["model"], "keep-me", "настройка из тома потеряна");
    assert_eq!(
        settings["permissions"]["allow"],
        serde_json::json!(["Bash"]),
        "вложенная настройка потеряна"
    );
    assert_eq!(settings["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
}

#[test]
fn claude_first_run_state_is_written_when_the_volume_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::ClaudeCode).files(&setup(dir.path()), Path::new(PROJECT));
    let state = file_at(&files, ".claude/.claude.json");
    assert!(
        state.owned_by_user && state.mode == 0o600,
        "состояние Claude Code должно принадлежать агенту и быть закрытым"
    );
    assert!(state.content.contains("hasCompletedOnboarding"));
}

#[test]
fn codex_requirements_pin_the_orbit_mcp_address() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::Codex).files(&with_mcp(dir.path()), Path::new(PROJECT));
    let requirements = file_at(&files, "codex/requirements.toml");
    assert_snapshot!(requirements.content, @r#"
    [mcp_servers.iapetus-project]
    identity = { url = "http://127.0.0.1:41000/mcp" }

    [mcp_servers.iapetus]
    identity = { url = "http://orbit-server:2202/mcp" }
    "#);
    let config = &file_at(&files, ".codex/config.toml").content;
    assert!(
        config.contains("[mcp_servers.iapetus-project]\nurl = \"http://127.0.0.1:41000/mcp\"\n"),
        "Codex не получил проектный MCP session: {config}"
    );
    assert!(
        config.contains("[mcp_servers.iapetus]\nurl = \"http://orbit-server:2202/mcp\"\nhttp_headers = { Authorization = \"Bearer session-1.deadbeef\" }"),
        "Codex не получил сервер Iapetus с токеном: {config}"
    );
}

#[test]
fn antigravity_global_mcp_has_the_project_and_orbit_servers() {
    let dir = tempfile::tempdir().unwrap();
    let files = Profile(Agent::Antigravity).files(&with_mcp(dir.path()), Path::new(PROJECT));
    let config = serde_json::from_str::<serde_json::Value>(&file_at(&files, ".gemini/config/mcp_config.json").content).unwrap();
    assert_eq!(
        config,
        serde_json::json!({ "mcpServers": {
            "iapetus-project": { "serverUrl": "http://127.0.0.1:41000/mcp" },
            "iapetus": {
                "serverUrl": "http://orbit-server:2202/mcp",
                "headers": { "Authorization": "Bearer session-1.deadbeef" },
            },
        } }),
        "глобальный конфиг agy не ограничен проектным MCP и сервером Iapetus"
    );
}

#[test]
fn every_agent_gets_the_project_mcp_without_iapetus_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let address = "http://127.0.0.1:41000/mcp";
    let claude = Profile(Agent::ClaudeCode).files(&setup, Path::new(PROJECT));
    assert!(
        file_at(&claude, "claude-code/managed-mcp.json").content.contains(address),
        "Claude Code без проектного MCP"
    );
    let codex = Profile(Agent::Codex).files(&setup, Path::new(PROJECT));
    for suffix in [".codex/config.toml", "codex/requirements.toml"] {
        let content = &file_at(&codex, suffix).content;
        assert!(
            content.contains("iapetus-project") && content.contains(address),
            "Codex без проектного MCP в {suffix}: {content}"
        );
    }
    let antigravity = Profile(Agent::Antigravity).files(&setup, Path::new(PROJECT));
    assert!(
        file_at(&antigravity, ".gemini/config/mcp_config.json").content.contains(address),
        "Antigravity без проектного MCP"
    );
}

#[test]
fn mcp_token_is_not_printed_by_debug() {
    let dir = tempfile::tempdir().unwrap();
    let printed = format!("{:?}", with_mcp(dir.path()));
    assert!(!printed.contains("deadbeef"), "токен MCP попал в Debug: {printed}");
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(own, _)| own == name).map(|(_, value)| value.as_str())
}

/// Окружение session агенту не передаётся целиком: токен, попавший в него, до агента не доходит.
#[test]
fn claude_token_in_the_session_environment_is_not_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let inherited = pairs(&[("PATH", "/bin"), ("CLAUDE_CODE_OAUTH_TOKEN", "from-session-environment")]);
    let env = launch_environment(Profile(Agent::ClaudeCode), &setup(dir.path()), inherited.into_iter());
    assert_eq!(
        value(&env, "CLAUDE_CODE_OAUTH_TOKEN"),
        None,
        "токен окружения session дошёл до агента"
    );
}

/// Без `CLAUDE_CONFIG_DIR` Claude Code кладёт `.claude.json` в `~` вне тома, и вступление повторяется.
#[test]
fn claude_environment_points_the_config_dir_into_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let expected = format!("{AGENT_ROOT}/.claude");
    let launched = launch_environment(Profile(Agent::ClaudeCode), &setup, std::iter::empty());
    assert_eq!(
        value(&launched, "CLAUDE_CONFIG_DIR"),
        Some(expected.as_str()),
        "очищенное окружение потеряло переменную состояния"
    );
    let inherited = pairs(&[("CLAUDE_CONFIG_DIR", "/elsewhere")]);
    let launched = launch_environment(Profile(Agent::ClaudeCode), &setup, inherited.into_iter());
    assert_eq!(
        value(&launched, "CLAUDE_CONFIG_DIR"),
        Some(expected.as_str()),
        "унаследованная переменная состояния главнее профиля"
    );
}

/// Claude Code раскрывает `@`-импорты вне проекта только по записи с точным путём проекта; своих ключей проекта,
/// которые он дописал в томе, запись не стирает.
#[test]
fn claude_approves_external_imports_by_the_exact_mount_path_and_keeps_project_keys() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let config_dir = setup.user.state_root.join(".claude");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join(".claude.json"),
        r#"{"projects":{"/home/agent/proj":{"allowedTools":["x"],"lastCost":1,"hasTrustDialogAccepted":false}}}"#,
    )
    .unwrap();

    let files = Profile(Agent::ClaudeCode).files(&setup, Path::new(PROJECT));
    let state: serde_json::Value = serde_json::from_str(&file_at(&files, ".claude/.claude.json").content).unwrap();
    let project = &state["projects"]["/home/agent/proj"];
    assert_eq!(
        project["hasClaudeMdExternalIncludesApproved"], true,
        "импорты вне проекта не разрешены"
    );
    assert_eq!(project["hasTrustDialogAccepted"], true, "отказ в доверии из тома не перезаписан");
    assert_eq!(project["allowedTools"], serde_json::json!(["x"]), "ключ проекта из тома потерян");
    assert_eq!(project["lastCost"], 1, "ключ проекта из тома потерян");
}

#[test]
fn claude_environment_disables_nonessential_traffic() {
    let dir = tempfile::tempdir().unwrap();
    let env = launch_environment(Profile(Agent::ClaudeCode), &setup(dir.path()), std::iter::empty());
    assert_eq!(
        value(&env, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
        Some("1"),
        "служебный трафик Claude Code идёт мимо шлюза прямо на api.anthropic.com"
    );
}

/// Агент ходит в сеть сам: прокси и сертификаты session перехватили бы его вызовы.
#[test]
fn proxy_and_certificate_variables_of_the_session_do_not_reach_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let names = [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
        "https_proxy",
        "http_proxy",
        "all_proxy",
        "no_proxy",
        "ssl_cert_file",
        "node_extra_ca_certs",
    ];
    for agent in [Profile(Agent::ClaudeCode), Profile(Agent::Codex), Profile(Agent::Antigravity)] {
        let inherited = names
            .iter()
            .map(|name| ((*name).to_owned(), "http://127.0.0.1:3128".to_owned()))
            .chain([("PATH".to_owned(), "/bin".to_owned())]);
        let env = launch_environment(agent, &setup(dir.path()), inherited);
        for name in names {
            assert_eq!(value(&env, name), None, "{name} session дошла до агента {agent:?}");
        }
    }
}

/// Подменённая оболочка без `IAPETUS_SHELL_CALLS` отказывает: каждый профиль обязан задать вид вызовов, который
/// оболочка разбирает.
#[test]
fn every_profile_sets_a_shell_calls_kind_the_shell_accepts() {
    let dir = tempfile::tempdir().unwrap();
    for agent in [Profile(Agent::ClaudeCode), Profile(Agent::Codex), Profile(Agent::Antigravity)] {
        let env = launch_environment(agent, &setup(dir.path()), std::iter::empty());
        let kind = value(&env, shell::CALLS_ENV);
        assert!(
            Calls::from_env(kind).is_ok(),
            "профиль {agent:?} не задал вид вызовов оболочки: {kind:?}"
        );
    }
}

#[test]
fn locale_and_time_zone_variables_are_inherited_and_others_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let inherited = pairs(&[
        ("LC_ALL", "C"),
        ("LC_MESSAGES", "ru"),
        ("LANG", "C"),
        ("TZ", "UTC"),
        ("LCX", "no"),
        ("CODEX_HOME", "/x"),
    ]);
    let env = launch_environment(Profile(Agent::Codex), &setup(dir.path()), inherited.into_iter());
    for name in ["LC_ALL", "LC_MESSAGES", "LANG", "TZ"] {
        assert!(value(&env, name).is_some(), "{name} не унаследована");
    }
    assert_eq!(value(&env, "LCX"), None, "LCX унаследована");
    assert_eq!(
        value(&env, "CODEX_HOME"),
        Some(format!("{AGENT_ROOT}/.codex").as_str()),
        "каталог состояния Codex взят из окружения session, а не из профиля"
    );
}

fn conversation(config_dir: &Path, slug: &str, file: &str) -> PathBuf {
    let dir = config_dir.join("projects").join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(file);
    std::fs::write(&path, "{}\n").unwrap();
    path
}

#[test]
fn claude_conversations_move_under_the_slug_of_the_new_project_path() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path();
    conversation(config_dir, "-srv-my-proj-v1-2", "a.jsonl");

    Profile(Agent::ClaudeCode)
        .relocate_conversations(config_dir, Path::new("/srv/my_proj.v1/2"), Path::new("/home/ü/p"))
        .unwrap();

    assert!(
        config_dir.join("projects/-home---p/a.jsonl").exists(),
        "разговор не найден под именем нового пути"
    );
    assert!(
        !config_dir.join("projects/-srv-my-proj-v1-2").exists(),
        "разговор остался под прежним путём"
    );
}

#[test]
fn claude_has_a_conversation_only_when_the_project_directory_holds_a_jsonl_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path();
    let agent = Profile(Agent::ClaudeCode);
    let project = Path::new("/srv/one");

    assert!(!agent.has_conversation(config_dir, project), "разговор найден без каталога проекта");

    std::fs::create_dir_all(config_dir.join("projects/-srv-one")).unwrap();
    std::fs::write(config_dir.join("projects/-srv-one/notes.txt"), "").unwrap();
    assert!(!agent.has_conversation(config_dir, project), "разговором сочтён файл не .jsonl");

    conversation(config_dir, "-srv-one", "a.jsonl");
    assert!(agent.has_conversation(config_dir, project), "сохранённый разговор не найден");
    assert!(
        !agent.has_conversation(config_dir, Path::new("/srv/two")),
        "разговор найден у другого проекта"
    );
}

#[test]
fn agents_without_a_conversation_check_always_resume() {
    let dir = tempfile::tempdir().unwrap();
    for agent in [Agent::Codex, Agent::Antigravity] {
        assert!(
            Profile(agent).has_conversation(dir.path(), Path::new("/srv/one")),
            "агент без проверки разговора запущен без продолжения"
        );
    }
}

#[test]
fn claude_conversations_merge_into_an_existing_directory_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path();
    conversation(config_dir, "-srv-one", "old.jsonl");
    conversation(config_dir, "-srv-one", "same.jsonl");
    let kept = conversation(config_dir, "-srv-two", "same.jsonl");
    std::fs::write(&kept, "kept").unwrap();

    Profile(Agent::ClaudeCode)
        .relocate_conversations(config_dir, Path::new("/srv/one"), Path::new("/srv/two"))
        .unwrap();

    assert!(
        config_dir.join("projects/-srv-two/old.jsonl").exists(),
        "разговор не перенесён в существующий каталог"
    );
    assert_eq!(std::fs::read_to_string(kept).unwrap(), "kept", "перенос затёр разговор нового пути");
}

#[test]
fn follow_project_moves_conversations_only_when_the_path_changed_since_the_last_start() {
    let dir = tempfile::tempdir().unwrap();
    let setup = setup(dir.path());
    let agent = Profile(Agent::ClaudeCode);
    let state_dir = setup.user.state_root.join(".claude");
    std::fs::create_dir_all(&state_dir).unwrap();

    follow_project(agent, &setup, Path::new("/srv/one")).unwrap();
    conversation(&state_dir, "-srv-one", "a.jsonl");
    follow_project(agent, &setup, Path::new("/srv/one")).unwrap();
    assert!(state_dir.join("projects/-srv-one/a.jsonl").exists(), "тот же путь сдвинул разговор");

    follow_project(agent, &setup, Path::new("/srv/two")).unwrap();
    assert!(
        state_dir.join("projects/-srv-two/a.jsonl").exists(),
        "разговор не последовал за проектом"
    );
    assert!(!state_dir.join("projects/-srv-one").exists());
}

/// Каталоги XDG агента лежат в его состоянии, а не в `$HOME` runner, и не мешают переменным состояния самого агента
/// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); их не заменяют значения из окружения session.
#[test]
fn xdg_directories_of_every_agent_point_into_its_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    for (agent, state, own) in [
        (Profile(Agent::ClaudeCode), ".claude", Some("CLAUDE_CONFIG_DIR")),
        (Profile(Agent::Codex), ".codex", Some("CODEX_HOME")),
        (Profile(Agent::Antigravity), ".gemini", None),
    ] {
        let inherited = pairs(&[("XDG_CACHE_HOME", "/root/.cache"), ("PATH", "/bin")]);
        let env = launch_environment(agent, &setup(dir.path()), inherited.into_iter());
        let state_dir = format!("{AGENT_ROOT}/{state}");
        for (variable, name) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
        ] {
            assert_eq!(
                value(&env, variable),
                Some(format!("{state_dir}/xdg/{name}").as_str()),
                "{agent:?}: {variable} не в каталоге состояния"
            );
        }
        if let Some(own) = own {
            assert_eq!(value(&env, own), Some(state_dir.as_str()), "{agent:?}: {own} потерян или изменён");
        }
        assert!(value(&env, "PATH").is_some(), "{agent:?}: PATH потерян");
    }
}
