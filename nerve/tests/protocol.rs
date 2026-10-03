use insta::assert_snapshot;
use proptest::prelude::*;

use super::*;
use crate::fingerprint::Fingerprint;

fn fingerprint(s: &str) -> Fingerprint {
    s.parse().unwrap()
}

fn assign_runner() -> Control {
    Control::AssignRunner {
        runner: Some(assigned_runner()),
        agent: AgentRun::Restart { resume: true },
    }
}

fn assigned_runner() -> AssignedRunner {
    AssignedRunner {
        runner: fingerprint("s"),
        assignment: Assignment(7),
        name: "n".into(),
        os: "linux".into(),
        host: RunnerHost {
            project: "/srv/p".into(),
            hostname: "h".into(),
            os_release: "ID=x\n".into(),
            home: "/home/u".into(),
            home_origin: String::new(),
            home_served: false,
        },
        env_names: vec!["A".into()],
    }
}

fn mcp_catalog() -> McpCatalog {
    McpCatalog {
        servers: vec![McpServer {
            name: "s".into(),
            instructions: Some("i".into()),
            tools: vec![McpTool {
                name: "t".into(),
                description: "d".into(),
                input_schema: "{}".into(),
            }],
            resources: vec![McpResource {
                uri: "u".into(),
                name: "n".into(),
                description: "d".into(),
                mime_type: "text/plain".into(),
            }],
            prompts: vec![McpPrompt {
                name: "p".into(),
                description: "d".into(),
                arguments: vec![McpPromptArgument {
                    name: "a".into(),
                    description: "d".into(),
                    required: true,
                }],
            }],
        }],
    }
}

fn mcp_calls() -> Vec<McpCall> {
    vec![
        McpCall::Tool {
            server: "s".into(),
            tool: "t".into(),
            arguments: "{}".into(),
        },
        McpCall::ReadResource {
            server: "s".into(),
            uri: "u".into(),
        },
        McpCall::GetPrompt {
            server: "s".into(),
            name: "p".into(),
            arguments: "{}".into(),
        },
    ]
}

proptest! {
    /// Мусор на входе даёт ошибку или сообщение, но не панику.
    #[test]
    fn garbage_body_never_panics(body: Vec<u8>) {
        drop(decode_body::<FsRequest>(&body));
        drop(decode_body::<ExecEvent>(&body));
        drop(decode_body::<Status>(&body));
        drop(decode_body::<McpCatalog>(&body));
        drop(decode_body::<McpCall>(&body));
        drop(decode_body::<McpResponse>(&body));
        drop(decode_body::<Control>(&body));
        drop(decode_body::<RunnerRequest>(&body));
        drop(decode_body::<DataRequest>(&body));
    }

    /// Обрезанное сообщение не принимается ни на одной длине.
    #[test]
    fn truncated_body_is_malformed(request: FsRequest) {
        let body = encode(&request).unwrap();
        for len in 0..body.len() {
            let result = decode_body::<FsRequest>(&body[..len]);
            prop_assert_eq!(result, Err(DecodeError::Malformed), "обрезка до {} байт принята", len);
        }
    }

    #[test]
    fn trailing_bytes_are_malformed(request: FsRequest, tail in proptest::collection::vec(any::<u8>(), 1..8)) {
        let mut body = encode(&request).unwrap();
        body.extend(tail);
        prop_assert_eq!(decode_body::<FsRequest>(&body), Err(DecodeError::Malformed));
    }
}

fn update_agent() -> Control {
    Control::UpdateAgent(AgentRelease {
        agent: Agent::Codex,
        version: "2.1.285".into(),
        artifacts: vec![Artifact {
            platform: Platform::LinuxArm64,
            url: "https://example.org/a".into(),
            digest: Digest::Sha512("ab".into()),
            format: ArtifactFormat::TarGz,
        }],
    })
}

/// Снимок байтов указания обновления: меняется схема или порядок вариантов — падает, и нужно поднять `VERSION`.
#[test]
fn agent_update_wire_format_is_pinned() {
    let bytes = encode(&update_agent()).unwrap();
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat();
    assert_snapshot!(hex, @"020107322e312e32383501011568747470733a2f2f6578616d706c652e6f72672f610102616201");
}

/// Снимок байтов окна лимита: меняется схема — падает, и нужно поднять `VERSION`.
#[test]
fn usage_window_wire_format_is_pinned() {
    let window = UsageWindow {
        duration: Some(18000),
        scope: Some("m".into()),
        used: 0.5,
        resets_at: Some(7),
    };
    let bytes = encode(&window).unwrap();
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat();
    assert_snapshot!(hex, @"01d08c0101016d0000003f0107");
}

/// Снимок байтов: меняется схема или порядок вариантов — падает, и нужно поднять `VERSION`.
#[test]
fn wire_format_is_pinned() {
    let attr = Attr {
        kind: Kind::File,
        size: 3,
        mtime_ns: 1,
        mode: 0o644,
    };
    let messages: Vec<(&str, Vec<u8>)> = vec![
        ("Stat", encode(&FsRequest::Stat { path: "a".into() }).unwrap()),
        (
            "Write",
            encode(&FsRequest::Write {
                path: "a".into(),
                expect: Expect::Absent,
                content: Content(vec![1, 2]),
            })
            .unwrap(),
        ),
        ("Hash", encode(&FsRequest::Hash { path: "a".into() }).unwrap()),
        ("Remove", encode(&FsRequest::Remove { path: "a".into() }).unwrap()),
        ("Err", encode::<FsResponse>(&Err(Error::Conflict)).unwrap()),
        ("NotEmpty", encode::<FsResponse>(&Err(Error::NotEmpty)).unwrap()),
        (
            "Hashed",
            encode::<FsResponse>(&Ok(FsReply::Hashed { attr, sha256: [9; 32] })).unwrap(),
        ),
        ("Removed", encode::<FsResponse>(&Ok(FsReply::Removed)).unwrap()),
        (
            "Read",
            encode::<FsResponse>(&Ok(FsReply::Read {
                attr,
                sha256: [9; 32],
                content: Content(vec![1, 2]),
            }))
            .unwrap(),
        ),
        (
            "Exit",
            encode(&ExecEvent::Exit(ExecExit {
                status: ExitStatus::Signal(9),
                cwd: "/w".into(),
            }))
            .unwrap(),
        ),
        (
            "Output",
            encode(&ExecEvent::Output {
                stream: Stream::Stderr,
                data: vec![1],
            })
            .unwrap(),
        ),
        ("Heartbeat", encode(&ExecEvent::Heartbeat).unwrap()),
        (
            "Watch",
            encode(&WatchBatch {
                root: Root::Home,
                changes: vec![
                    Change {
                        path: "a".into(),
                        kind: ChangeKind::Updated(attr),
                    },
                    Change {
                        path: "b".into(),
                        kind: ChangeKind::Removed,
                    },
                    Change {
                        path: "c".into(),
                        kind: ChangeKind::Unwatched,
                    },
                ],
            })
            .unwrap(),
        ),
        ("Control", encode(&assign_runner()).unwrap()),
        ("Refused", encode(&SessionEvent::Refused("a".into())).unwrap()),
    ];
    let dump = messages
        .iter()
        .map(|(name, bytes)| format!("{name}: {}", bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat()))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(dump, @r"
    Stat: 000161
    Write: 04016101020102
    Hash: 060161
    Remove: 070161
    Err: 0102
    NotEmpty: 0105
    Hashed: 0006000302a4030909090909090909090909090909090909090909090909090909090909090909
    Removed: 0007
    Read: 0002000302a4030909090909090909090909090909090909090909090909090909090909090909020102
    Exit: 020112022f77
    Output: 00010101
    Heartbeat: 01
    Watch: 0103016100000302a403016201016302
    Control: 0001017307016e056c696e7578062f7372762f7001680549443d780a072f686f6d652f7500000101410201
    Refused: 010161
    ");
}

/// Снимок байтов чтения ссылки и корня файловых запросов: меняется схема или порядок вариантов — падает, и нужно
/// поднять `VERSION`.
#[test]
fn link_wire_format_is_pinned() {
    let messages: Vec<(&str, Vec<u8>)> = vec![
        ("ReadLink", encode(&FsRequest::ReadLink { path: "a".into() }).unwrap()),
        ("Link", encode::<FsResponse>(&Ok(FsReply::Link("b".into()))).unwrap()),
        (
            "DataFs",
            encode(&DataRequest::Fs {
                root: Root::Home,
                request: FsRequest::Stat { path: "a".into() },
            })
            .unwrap(),
        ),
    ];
    let dump = messages
        .iter()
        .map(|(name, bytes)| format!("{name}: {}", bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat()))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(dump, @r"
    ReadLink: 030161
    Link: 00030162
    DataFs: 0001000161
    ");
}

/// Снимок байтов сообщений MCP: меняется схема или порядок вариантов — падает, и нужно поднять `VERSION`.
#[test]
fn mcp_wire_format_is_pinned() {
    let messages: Vec<(&str, Vec<u8>)> = vec![
        ("McpRunner", encode(&RunnerRequest::Mcp(mcp_catalog())).unwrap()),
        ("McpControl", encode(&Control::McpCatalog(mcp_catalog())).unwrap()),
        ("McpTool", encode(&DataRequest::Mcp(mcp_calls().remove(0))).unwrap()),
        ("McpResource", encode(&DataRequest::Mcp(mcp_calls().remove(1))).unwrap()),
        ("McpPrompt", encode(&DataRequest::Mcp(mcp_calls().remove(2))).unwrap()),
        ("McpOk", encode::<McpResponse>(&Ok("{}".into())).unwrap()),
        ("McpErr", encode::<McpResponse>(&Err(McpError("a".into()))).unwrap()),
    ];
    let dump = messages
        .iter()
        .map(|(name, bytes)| format!("{name}: {}", bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat()))
        .collect::<Vec<_>>()
        .join("\n");
    assert_snapshot!(dump, @r"
    McpRunner: 020101730101690101740164027b7d010175016e01640a746578742f706c61696e0101700164010161016401
    McpControl: 030101730101690101740164027b7d010175016e01640a746578742f706c61696e0101700164010161016401
    McpTool: 040001730174027b7d
    McpResource: 040101730175
    McpPrompt: 040201730170027b7d
    McpOk: 00027b7d
    McpErr: 010161
    ");
}

#[test]
fn secrets_are_hidden_in_debug_output() {
    let request = RegisterRequest {
        orbit_key: "tok-123".into(),
        fingerprint: fingerprint("fp"),
        public_key: "ssh-ed25519 AAAA".into(),
        name: "n".into(),
        os: "linux".into(),
        parent: None,
    };
    let session = SessionMessage::Start {
        assignment: Assignment(1),
        author: Author {
            name: "A".into(),
            email: "a@b".into(),
        },
        github_token: Some("ghp-789".into()),
    };
    let renew = RenewRequest {
        orbit_key: "renew-456".into(),
        fingerprint: fingerprint("fp"),
        timestamp: 1,
        signature: Vec::new(),
    };
    let certificate = WorkerCertificateRequest {
        orbit_key: "cert-321".into(),
        public_key: "ssh-ed25519 AAAA".into(),
        name: "alice".into(),
    };
    let env = EnvChange::Set {
        key: "all.TOKEN".into(),
        value: "env-654".into(),
    };
    let identity = SessionIdentity {
        author: None,
        github_token: Some("ghp-identity".into()),
    };
    let entry = EnvEntry {
        key: "all.TOKEN".into(),
        value: "entry-987".into(),
    };
    let text = format!("{request:?} {session:?} {renew:?} {certificate:?} {env:?} {identity:?} {entry:?}");
    for secret in [
        "tok-123",
        "ghp-789",
        "renew-456",
        "cert-321",
        "env-654",
        "ghp-identity",
        "entry-987",
    ] {
        assert!(!text.contains(secret), "секрет {secret} попал в Debug: {text}");
    }
    assert!(text.contains("all.TOKEN"), "в Debug нет ключа окружения: {text}");
}

/// Снимок байтов запроса запуска и вида причины «нужен вход»: смена схемы или порядка вариантов требует `VERSION`.
#[test]
fn session_start_and_login_cause_wire_format_is_pinned() {
    let start = WorkerRequest::Sessions(SessionsOp::Start {
        agent: Agent::Codex,
        runner: fingerprint("s"),
        identity: SessionIdentity::default(),
    });
    let hex = |bytes: Vec<u8>| bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().concat();
    let dump = format!("{}\n{}", hex(encode(&start).unwrap()), hex(encode(&Cause::LoginRequired).unwrap()));
    assert_snapshot!(dump, @r"
    01010101730000
    02
    ");
}
