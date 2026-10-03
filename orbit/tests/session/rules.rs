use nerve::protocol::{Document, RunnerHost};

use nerve::agent::Agent;

use super::*;
use crate::session::launch::AgentUser;

const AGENTS: [Profile; 3] = [Profile(Agent::ClaudeCode), Profile(Agent::Codex), Profile(Agent::Antigravity)];

fn setup(root: &std::path::Path) -> Setup {
    Setup {
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
    }
}

fn document(name: &str, content: &str) -> Document {
    Document {
        name: name.to_owned(),
        content: content.to_owned(),
    }
}

/// Документ server должен дойти до файла правил каждого агента, а удалённый и заменённый — исчезнуть из него.
#[test]
fn server_documents_follow_the_rules_file_of_every_agent() {
    for agent in AGENTS {
        let dir = tempfile::tempdir().unwrap();
        let setup = setup(dir.path());
        let (path, _) = agent.rules_file(&setup);
        let rules = Rules::new(agent, setup, None);

        rules.apply(DocsEvent::Snapshot(vec![document("style", "пиши коротко")])).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("пиши коротко"),
            "{agent:?}: документ server не попал в файл правил: {text}"
        );
        assert!(text.contains("ECANCELED"), "{agent:?}: в правилах нет смысла ECANCELED: {text}");

        rules.apply(DocsEvent::Updated(document("style", "пиши подробно"))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("пиши подробно") && !text.contains("пиши коротко"),
            "{agent:?}: документ не заменён: {text}"
        );

        rules.apply(DocsEvent::Removed { name: "style".to_owned() }).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("пиши подробно"),
            "{agent:?}: удалённый документ остался в правилах: {text}"
        );
    }
}

/// Файл правил в проекте модель приняла бы за код: он лежит только в глобальном месте агента.
#[test]
fn rules_file_never_appears_in_the_project() {
    for agent in AGENTS {
        let dir = tempfile::tempdir().unwrap();
        let setup = setup(dir.path());
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let (path, _) = agent.rules_file(&setup);
        let rules = Rules::new(agent, setup.clone(), None);

        rules.apply(DocsEvent::Snapshot(vec![document("style", "пиши коротко")])).unwrap();

        assert!(path.exists(), "{agent:?}: файл правил не записан");
        assert!(
            !path.starts_with(&project),
            "{agent:?}: файл правил лежит в проекте: {}",
            path.display()
        );
        let entries = std::fs::read_dir(&project).unwrap().count();
        assert_eq!(entries, 0, "{agent:?}: в рабочем каталоге появились файлы");
    }
}

/// Агент узнаёт из правил, на какой машине работает и какие переменные окружения там есть.
#[test]
fn rules_name_the_runner_its_os_and_environment_variables() {
    for agent in AGENTS {
        let dir = tempfile::tempdir().unwrap();
        let setup = setup(dir.path());
        let (path, _) = agent.rules_file(&setup);
        let rules = Rules::new(agent, setup, None);

        rules
            .assign(Some(&AssignedRunner {
                runner: "ab".parse().unwrap(),
                assignment: nerve::protocol::Assignment(3),
                name: "рабочая".to_owned(),
                os: "linux".to_owned(),
                host: RunnerHost {
                    project: "/srv/мой проект".to_owned(),
                    hostname: "h".to_owned(),
                    os_release: String::new(),
                    home: String::new(),
                    home_origin: String::new(),
                    home_served: false,
                },
                env_names: vec!["EDITOR".to_owned(), "TOKEN".to_owned()],
            }))
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        for expected in ["рабочая", "linux", "EDITOR, TOKEN", "/srv/мой проект"] {
            assert!(text.contains(expected), "{agent:?}: в правилах нет «{expected}»: {text}");
        }
    }
}

/// Каталоги правил в домашнем каталоге создаются от имени пользователя агента и закрытыми: от root они остались бы
/// root'у и открытыми (0755), а агент не смог бы писать рядом свои файлы.
#[test]
fn rules_directories_in_the_agent_home_are_created_private_and_owned_by_the_user() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    for (agent, directory) in [(Profile(Agent::Codex), ".codex"), (Profile(Agent::Antigravity), ".gemini/config")] {
        let dir = tempfile::tempdir().unwrap();
        let setup = setup(dir.path());
        let created = setup.user.state_root.join(directory);
        let rules = Rules::new(agent, setup.clone(), None);

        rules.write().unwrap();

        let metadata = std::fs::metadata(&created).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o700,
            "{agent:?}: каталог {directory} открыт"
        );
        assert_eq!(
            metadata.uid(),
            setup.user.uid,
            "{agent:?}: каталог {directory} не принадлежит пользователю агента"
        );
    }
}

fn assigned(host: RunnerHost) -> AssignedRunner {
    AssignedRunner {
        runner: "ab".parse().unwrap(),
        assignment: nerve::protocol::Assignment(3),
        name: "рабочая".to_owned(),
        os: "linux".to_owned(),
        host,
        env_names: Vec::new(),
    }
}

/// С ФС `$HOME` встроенные инструменты видят файлы `$HOME` как на runner; без неё (служба выключена, `$HOME` внутри
/// проекта) правила прежние: все файлы вне проекта — контейнер.
#[test]
fn rules_tell_which_files_outside_the_project_the_builtin_tools_see() {
    let host = |project: &str, home: &str, served: bool| RunnerHost {
        project: project.to_owned(),
        hostname: "h".to_owned(),
        os_release: String::new(),
        home: home.to_owned(),
        home_origin: String::new(),
        home_served: served,
    };
    for agent in AGENTS {
        let render_for = |host: RunnerHost| {
            let dir = tempfile::tempdir().unwrap();
            let setup = setup(dir.path());
            let (path, _) = agent.rules_file(&setup);
            let rules = Rules::new(agent, setup, None);
            rules.assign(Some(&assigned(host))).unwrap();
            std::fs::read_to_string(path).unwrap()
        };
        let with_home = render_for(host("/srv/p", "/home/alice", true));
        assert!(
            with_home.contains("Файлы в `$HOME` runner (`/home/alice`)"),
            "{agent:?}: нет текста про $HOME: {with_home}"
        );
        assert!(
            !with_home.contains("видят файловую систему контейнера, а не runner: вне проекта"),
            "{agent:?}: остался прежний текст"
        );

        for without in [
            host("/srv/p", "/home/alice", false),
            host("/home/alice/p", "/home/alice/p/h", true),
            host("/srv/p", "", true),
        ] {
            let text = render_for(without.clone());
            assert!(
                text.contains("видят файловую систему контейнера, а не runner: вне проекта"),
                "{agent:?}: без ФС $HOME ({without:?}) нет прежнего текста: {text}"
            );
            assert!(!text.contains("Файлы в `$HOME` runner"), "{agent:?}: ФС $HOME обещана без неё");
        }
    }
}
