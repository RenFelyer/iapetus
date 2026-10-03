//! Каждое условие шаблона нарушается отдельным тестом, остальные при этом верны. Верное тело
//! принимается — иначе отказ ничего не доказывал бы, — и отказ называет именно нарушенное условие.

use nerve::agent::{AGENT_ROOT, Agent};
use proptest::prelude::*;
use serde_json::{Value, json};

use super::super::Denied;
use super::super::fixtures::{APPARMOR, CAP_ADD, IMAGE, LIMITS, NETWORK, OTHER_IMAGE, template, valid_create};
use super::{CheckedCreate, SessionSpec, Template};

fn check(body: &Value) -> Result<CheckedCreate, Denied> {
    template().check_create(body.to_string().as_bytes())
}

#[track_caller]
fn denied(body: &Value) -> Denied {
    check(body).expect_err("тело с нарушением принято шаблоном")
}

/// Верное тело с одним изменением: `value` по пути `pointer` (родитель должен существовать).
fn put(pointer: &str, value: Value) -> Value {
    let mut body = valid_create();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    let parent = body.pointer_mut(parent).unwrap();
    match parent {
        Value::Array(items) => items[key.parse::<usize>().unwrap()] = value,
        parent => parent[key] = value,
    }
    body
}

fn without(pointer: &str) -> Value {
    let mut body = valid_create();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    body.pointer_mut(parent).unwrap().as_object_mut().unwrap().remove(key).unwrap();
    body
}

#[track_caller]
fn assert_malformed(result: Result<CheckedCreate, Denied>, expected: &str) {
    match result {
        Err(Denied::Malformed(why)) => assert!(why.contains(expected), "причина отказа {why:?} не содержит {expected:?}"),
        other => panic!("ждали Malformed с {expected:?}, получили {other:?}"),
    }
}

#[test]
fn valid_body_is_accepted_and_rebuilt_from_the_template_only() {
    let checked = check(&valid_create()).unwrap();
    assert_eq!(checked.volume, "claude-vol");
    let rebuilt: Value = serde_json::from_slice(&checked.body).unwrap();
    insta::assert_snapshot!(serde_json::to_string_pretty(&rebuilt).unwrap(), @r#"
    {
      "Image": "registry.example/iapetus/session@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "Labels": {
        "iapetus.agent": "claude",
        "iapetus.role": "session"
      },
      "Env": [
        "IAPETUS_TICKET=abc"
      ],
      "HostConfig": {
        "CapAdd": [
          "SYS_ADMIN",
          "SETUID",
          "SETGID",
          "SETPCAP"
        ],
        "CapDrop": [
          "ALL"
        ],
        "SecurityOpt": [
          "no-new-privileges",
          "apparmor=iapetus-session"
        ],
        "Tmpfs": {
          "/run/secrets": "rw,noexec,nosuid,nodev,mode=0700,size=1m",
          "/work": "rw,noexec,nosuid,nodev,mode=0700,size=1073741824"
        },
        "Devices": [
          {
            "PathOnHost": "/dev/fuse",
            "PathInContainer": "/dev/fuse",
            "CgroupPermissions": "rwm"
          }
        ],
        "NetworkMode": "iapetus-sessions",
        "Mounts": [
          {
            "Type": "volume",
            "Source": "claude-vol",
            "Target": "/home/agent/.claude"
          }
        ],
        "Memory": 1073741824,
        "MemorySwap": 1073741824,
        "NanoCpus": 1000000000,
        "PidsLimit": 256
      }
    }
    "#);
}

#[test]
fn explicitly_empty_dangerous_fields_are_accepted_but_not_forwarded() {
    let mut body = valid_create();
    let host = body["HostConfig"].as_object_mut().unwrap();
    host.insert("Privileged".into(), json!(false));
    host.insert("Binds".into(), json!([]));
    host.insert("PidMode".into(), json!(""));
    host.insert("IpcMode".into(), Value::Null);
    let rebuilt = String::from_utf8(check(&body).unwrap().body).unwrap();
    for field in ["Privileged", "Binds", "PidMode", "IpcMode"] {
        assert!(!rebuilt.contains(field), "{field} попало в пересобранное тело");
    }
}

#[test]
fn own_create_request_is_accepted_by_the_template() {
    let spec = SessionSpec {
        image: IMAGE,
        labels: std::collections::BTreeMap::from([("iapetus.session".to_owned(), "abc".to_owned())]),
        agent: Agent::ClaudeCode,
        env: vec!["IAPETUS_SESSION=abc".to_owned()],
        volume: "state-abc",
        work_bytes: 4096,
    };
    let checked = check(&template().create_request(&spec)).unwrap_or_else(|why| panic!("собственный запрос server отвергнут: {why:?}"));
    assert_eq!(checked.volume, "state-abc");
}

#[test]
fn privileged_is_denied() {
    assert_eq!(denied(&put("/HostConfig/Privileged", json!(true))), Denied::Privileged);
}

#[test]
fn extra_capability_is_denied() {
    let mut caps = CAP_ADD.to_vec();
    caps.push("NET_ADMIN");
    assert_eq!(denied(&put("/HostConfig/CapAdd", json!(caps))), Denied::Capabilities);
}

#[test]
fn every_template_capability_is_required() {
    for missing in CAP_ADD {
        let caps: Vec<&str> = CAP_ADD.into_iter().filter(|cap| *cap != missing).collect();
        assert_eq!(
            denied(&put("/HostConfig/CapAdd", json!(caps))),
            Denied::Capabilities,
            "без {missing}"
        );
    }
}

#[test]
fn replaced_capability_is_denied() {
    for caps in [json!(["ALL"]), json!(["NET_ADMIN"]), json!(["CAP_SYS_ADMIN"]), json!([])] {
        assert_eq!(denied(&put("/HostConfig/CapAdd", caps.clone())), Denied::Capabilities, "{caps}");
    }
}

#[test]
fn capability_names_in_other_case_or_order_are_denied() {
    let mut reversed = CAP_ADD.to_vec();
    reversed.reverse();
    let lower: Vec<String> = CAP_ADD.iter().map(|cap| cap.to_lowercase()).collect();
    for caps in [json!(reversed), json!(lower)] {
        assert_eq!(denied(&put("/HostConfig/CapAdd", caps.clone())), Denied::Capabilities, "{caps}");
    }
}

#[test]
fn missing_capability_is_denied() {
    assert_eq!(denied(&without("/HostConfig/CapAdd")), Denied::Capabilities);
}

#[test]
fn cap_drop_must_be_exactly_all() {
    for drop in [
        json!([]),
        json!(["NET_RAW"]),
        json!(["ALL", "KILL"]),
        json!(["all"]),
        json!(["KILL", "ALL"]),
    ] {
        assert_eq!(denied(&put("/HostConfig/CapDrop", drop.clone())), Denied::Capabilities, "{drop}");
    }
    assert_eq!(denied(&without("/HostConfig/CapDrop")), Denied::Capabilities);
}

#[test]
fn tmpfs_must_be_exactly_the_template_mounts() {
    let work = "rw,noexec,nosuid,nodev,mode=0700,size=1073741824";
    let secrets = "rw,noexec,nosuid,nodev,mode=0700,size=1m";
    for tmpfs in [
        json!({}),
        json!({ "/work": work }),
        json!({ "/run/secrets": secrets }),
        json!({ "/work": work, "/run/secrets": secrets, "/tmp": "rw,noexec,nosuid,nodev" }),
        json!({ "/work": work, "/run/secrets": secrets, "/home/agent": work }),
        json!({ "/work": work, "/run/other": secrets }),
        json!({ "/work/": work, "/run/secrets": secrets }),
    ] {
        assert_eq!(denied(&put("/HostConfig/Tmpfs", tmpfs.clone())), Denied::Tmpfs, "{tmpfs}");
    }
    assert_eq!(denied(&without("/HostConfig/Tmpfs")), Denied::Tmpfs);
}

#[test]
fn tmpfs_options_must_equal_the_template() {
    let secrets = "rw,noexec,nosuid,nodev,mode=0700,size=1m";
    let work = "rw,noexec,nosuid,nodev,mode=0700,size=1073741824";
    // Каждый вариант отличается от шаблона ровно одним параметром.
    for other in [
        "rw,nosuid,nodev,mode=0700,size=1073741824",
        "rw,noexec,nodev,mode=0700,size=1073741824",
        "rw,noexec,nosuid,mode=0700,size=1073741824",
        "rw,noexec,nosuid,nodev,mode=0755,size=1073741824",
        "rw,noexec,nosuid,nodev,mode=0700,size=2g",
        "rw,noexec,nosuid,nodev,mode=0700",
        "rw,noexec,nosuid,nodev,mode=0700,size=1073741824,exec",
        "rw,noexec,nosuid,nodev,mode=0700,size=1073741824,uid=0",
        "",
    ] {
        let tmpfs = json!({ "/work": other, "/run/secrets": secrets });
        assert_eq!(denied(&put("/HostConfig/Tmpfs", tmpfs)), Denied::Tmpfs, "/work: {other:?}");
    }
    for other in [
        "rw,noexec,nosuid,nodev,mode=0700,size=2m",
        "rw,nosuid,nodev,mode=0700,size=1m",
        "rw,noexec,nosuid,nodev,mode=0777,size=1m",
    ] {
        let tmpfs = json!({ "/work": work, "/run/secrets": other });
        assert_eq!(denied(&put("/HostConfig/Tmpfs", tmpfs)), Denied::Tmpfs, "/run/secrets: {other:?}");
    }
}

#[test]
fn work_size_is_a_canonical_byte_count_within_the_limit() {
    let secrets = "rw,noexec,nosuid,nodev,mode=0700,size=1m";
    let tmpfs = |size: &str| json!({ "/work": format!("rw,noexec,nosuid,nodev,mode=0700,size={size}"), "/run/secrets": secrets });
    let max = LIMITS.work.to_string();
    for size in ["1", "4096", max.as_str()] {
        let checked = check(&put("/HostConfig/Tmpfs", tmpfs(size))).unwrap_or_else(|why| panic!("размер {size} отвергнут: {why:?}"));
        assert!(
            String::from_utf8(checked.body).unwrap().contains(&format!("size={size}\"")),
            "размер {size} потерян"
        );
    }
    let above = (LIMITS.work + 1).to_string();
    for size in ["0", "-1", above.as_str(), "99999999999999999999"] {
        let why = denied(&put("/HostConfig/Tmpfs", tmpfs(size)));
        assert!(matches!(why, Denied::Limit(_) | Denied::Tmpfs), "размер {size}: {why:?}");
    }
    for size in ["", "+5", "05", "1k", "1.5", " 5"] {
        assert_eq!(denied(&put("/HostConfig/Tmpfs", tmpfs(size))), Denied::Tmpfs, "размер {size:?}");
    }
}

#[test]
fn security_opt_requires_no_new_privileges() {
    assert_eq!(denied(&without("/HostConfig/SecurityOpt")), Denied::SecurityOpt);
    for opts in [
        json!([]),
        json!(["apparmor=iapetus-session"]),
        json!(["no-new-privileges:true"]),
        json!(["no-new-privileges=true"]),
    ] {
        assert_eq!(denied(&put("/HostConfig/SecurityOpt", opts.clone())), Denied::SecurityOpt, "{opts}");
    }
}

#[test]
fn foreign_security_options_are_denied() {
    for extra in [
        "seccomp=unconfined",
        "seccomp=/etc/evil.json",
        "apparmor=unconfined",
        "apparmor=other",
        "apparmor=",
        "label=disable",
        "systempaths=unconfined",
        "writable-cgroups=true",
        "credentialspec=file://x",
    ] {
        let opts = json!(["no-new-privileges", "apparmor=iapetus-session", extra]);
        assert_eq!(denied(&put("/HostConfig/SecurityOpt", opts)), Denied::SecurityOpt, "{extra}");
        let without_profile = json!(["no-new-privileges", extra]);
        assert_eq!(
            denied(&put("/HostConfig/SecurityOpt", without_profile)),
            Denied::SecurityOpt,
            "{extra} без профиля"
        );
    }
}

#[test]
fn repeated_security_options_are_denied() {
    for opts in [
        json!(["no-new-privileges", "no-new-privileges"]),
        json!(["no-new-privileges", "apparmor=iapetus-session", "apparmor=iapetus-session"]),
    ] {
        assert_eq!(denied(&put("/HostConfig/SecurityOpt", opts.clone())), Denied::SecurityOpt, "{opts}");
    }
}

#[test]
fn apparmor_profile_is_optional_and_taken_from_the_template() {
    let rebuilt = |opts: Value| -> Value {
        let body = check(&put("/HostConfig/SecurityOpt", opts)).unwrap().body;
        serde_json::from_slice::<Value>(&body).unwrap()["HostConfig"]["SecurityOpt"].clone()
    };
    assert_eq!(rebuilt(json!(["no-new-privileges"])), json!(["no-new-privileges"]));
    let both = json!(["no-new-privileges", format!("apparmor={APPARMOR}")]);
    assert_eq!(rebuilt(both.clone()), both);
    // Порядок в запросе не важен: в Docker уходит порядок шаблона.
    assert_eq!(rebuilt(json!([format!("apparmor={APPARMOR}"), "no-new-privileges"])), both);
}

#[test]
fn apparmor_option_is_denied_when_the_template_has_no_profile() {
    let template = Template::new(vec![IMAGE.to_owned()], NETWORK.to_owned(), LIMITS, None).unwrap();
    let body = valid_create().to_string();
    assert_eq!(template.check_create(body.as_bytes()).unwrap_err(), Denied::SecurityOpt);
    let plain = put("/HostConfig/SecurityOpt", json!(["no-new-privileges"])).to_string();
    assert!(template.check_create(plain.as_bytes()).is_ok());
}

#[test]
fn extra_device_is_denied() {
    let mut devices = valid_create()["HostConfig"]["Devices"].clone();
    devices.as_array_mut().unwrap().push(json!({
        "PathOnHost": "/dev/kvm", "PathInContainer": "/dev/kvm", "CgroupPermissions": "rwm"
    }));
    assert_eq!(denied(&put("/HostConfig/Devices", devices)), Denied::Devices);
}

#[test]
fn other_device_is_denied() {
    for (host, container, perms) in [
        ("/dev/kvm", "/dev/kvm", "rwm"),
        ("/dev/fuse", "/dev/other", "rwm"),
        ("/dev/fuse", "/dev/fuse", "r"),
        ("/dev/fuse/..", "/dev/fuse", "rwm"),
    ] {
        let device = json!([{ "PathOnHost": host, "PathInContainer": container, "CgroupPermissions": perms }]);
        assert_eq!(
            denied(&put("/HostConfig/Devices", device)),
            Denied::Devices,
            "{host} {container} {perms}"
        );
    }
}

#[test]
fn missing_or_empty_devices_are_denied() {
    assert_eq!(denied(&without("/HostConfig/Devices")), Denied::Devices);
    assert_eq!(denied(&put("/HostConfig/Devices", json!([]))), Denied::Devices);
}

#[test]
fn binds_are_denied() {
    let body = put("/HostConfig/Binds", json!(["/:/host"]));
    assert_eq!(denied(&body), Denied::Binds);
}

#[test]
fn bind_mount_is_denied() {
    let mount = json!([{ "Type": "bind", "Source": "/", "Target": "/home/agent/.claude" }]);
    assert_eq!(denied(&put("/HostConfig/Mounts", mount)), Denied::Mounts);
}

#[test]
fn other_mounts_are_denied() {
    let target = |source: &str, kind: &str, target: &str| json!({ "Type": kind, "Source": source, "Target": target });
    for mounts in [
        json!([target("claude-vol", "tmpfs", "/home/agent/.claude")]),
        json!([target("claude-vol", "volume", "/")]),
        json!([target("claude-vol", "volume", "/home/agent/.claude/")]),
        json!([target("", "volume", "/home/agent/.claude")]),
        json!([target("../x", "volume", "/home/agent/.claude")]),
        json!([target("a", "volume", "/home/agent/.claude"), target("b", "volume", "/data")]),
        json!([]),
    ] {
        assert_eq!(denied(&put("/HostConfig/Mounts", mounts.clone())), Denied::Mounts, "{mounts}");
    }
    assert_eq!(denied(&without("/HostConfig/Mounts")), Denied::Mounts);
}

const AGENT_DIRS: [(&str, &str); 3] = [
    ("claude", "/home/agent/.claude"),
    ("codex", "/home/agent/.codex"),
    ("antigravity", "/home/agent/.gemini"),
];

#[test]
fn agent_directories_follow_the_agent_profile() {
    for agent in Agent::ALL {
        let expected = format!("{AGENT_ROOT}/{}", agent.state_dir());
        let (_, dir) = AGENT_DIRS.iter().find(|(id, _)| *id == agent.id()).unwrap();
        assert_eq!(*dir, expected, "{}", agent.id());
    }
}

fn create_for(agent: &str, target: &str) -> Value {
    let mut body = put("/Labels/iapetus.agent", json!(agent));
    body["HostConfig"]["Mounts"][0]["Target"] = json!(target);
    body
}

#[test]
fn volume_is_mounted_into_the_directory_of_its_agent() {
    for (agent, dir) in AGENT_DIRS {
        let checked = check(&create_for(agent, dir)).unwrap_or_else(|why| panic!("{agent}: {why:?}"));
        let rebuilt: Value = serde_json::from_slice(&checked.body).unwrap();
        assert_eq!(rebuilt["HostConfig"]["Mounts"][0]["Target"], json!(dir), "{agent}");
        assert_eq!(rebuilt["HostConfig"]["Mounts"].as_array().unwrap().len(), 1, "{agent}");
    }
}

#[test]
fn volume_mounted_into_another_agents_directory_is_denied() {
    for (agent, _) in AGENT_DIRS {
        for (other, dir) in AGENT_DIRS.iter().filter(|(other, _)| *other != agent) {
            assert_eq!(denied(&create_for(agent, dir)), Denied::Mounts, "{agent} с каталогом {other}");
        }
    }
}

#[test]
fn unknown_or_missing_agent_label_is_denied() {
    for labels in [
        json!({ "iapetus.role": "session" }),
        json!({ "iapetus.role": "session", "iapetus.agent": "gemini" }),
        json!({ "iapetus.role": "session", "iapetus.agent": "Codex" }),
    ] {
        assert_eq!(denied(&put("/Labels", labels.clone())), Denied::Label, "{labels}");
    }
}

#[test]
fn own_create_request_mounts_the_directory_of_its_agent() {
    for agent in Agent::ALL {
        let spec = SessionSpec {
            image: IMAGE,
            labels: std::collections::BTreeMap::new(),
            agent,
            env: vec![],
            volume: "state-abc",
            work_bytes: 4096,
        };
        let request = template().create_request(&spec);
        let dir = format!("{AGENT_ROOT}/{}", agent.state_dir());
        assert_eq!(request["HostConfig"]["Mounts"][0]["Target"], json!(dir), "{}", agent.id());
        assert_eq!(request["Labels"]["iapetus.agent"], json!(agent.id()));
        check(&request).unwrap_or_else(|why| panic!("{}: собственный запрос отвергнут: {why:?}", agent.id()));
    }
}

#[test]
fn host_modes_are_denied() {
    for field in ["PidMode", "IpcMode", "UTSMode", "UsernsMode", "CgroupnsMode"] {
        for mode in ["host", "container:abc", "private"] {
            let body = put(&format!("/HostConfig/{field}"), json!(mode));
            let Denied::HostMode(named) = denied(&body) else {
                panic!("{field}={mode} не отвергнут как режим хоста");
            };
            assert_eq!(named, field);
        }
    }
}

#[test]
fn other_networks_are_denied() {
    for network in ["host", "bridge", "none", "default", "container:abc", "other-net", ""] {
        let body = put("/HostConfig/NetworkMode", json!(network));
        assert_eq!(denied(&body), Denied::Network, "{network:?}");
    }
    assert_eq!(denied(&without("/HostConfig/NetworkMode")), Denied::Network);
}

#[test]
fn image_must_be_an_allowed_digest() {
    for image in [
        "registry.example/iapetus/session:latest",
        "registry.example/iapetus/session",
        "registry.example/iapetus/session:1@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        OTHER_IMAGE,
        "evil/session@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "",
    ] {
        assert_eq!(denied(&put("/Image", json!(image))), Denied::Image, "{image:?}");
    }
}

#[test]
fn every_limit_is_required_and_bounded() {
    let cases: [(&'static str, i64); 3] = [
        ("Memory", LIMITS.memory),
        ("NanoCpus", LIMITS.nano_cpus),
        ("PidsLimit", LIMITS.pids),
    ];
    for (field, max) in cases {
        let pointer = format!("/HostConfig/{field}");
        assert_eq!(denied(&without(&pointer)), Denied::Limit(field), "{field} отсутствует");
        for bad in [0, -1, max + 1] {
            let mut body = put(&pointer, json!(bad));
            if field == "Memory" {
                // Подкачка совпадает с памятью, чтобы нарушалась только граница памяти.
                body["HostConfig"]["MemorySwap"] = json!(bad);
            }
            assert_eq!(denied(&body), Denied::Limit(field), "{field}={bad}");
        }
    }
}

#[test]
fn swap_must_equal_memory() {
    assert_eq!(denied(&without("/HostConfig/MemorySwap")), Denied::Limit("MemorySwap"));
    let more = put("/HostConfig/MemorySwap", json!(1_500_000_000_i64));
    assert_eq!(denied(&more), Denied::Limit("MemorySwap"));
    let unlimited = put("/HostConfig/MemorySwap", json!(-1));
    assert_eq!(denied(&unlimited), Denied::Limit("MemorySwap"));
}

#[test]
fn role_label_is_required_and_exact() {
    assert_eq!(denied(&without("/Labels")), Denied::Label);
    for labels in [
        json!({}),
        json!({ "iapetus.role": "worker" }),
        json!({ "iapetus.role": "Session" }),
        json!({ "Iapetus.role": "session" }),
        json!({ "role": "session" }),
    ] {
        assert_eq!(denied(&put("/Labels", labels.clone())), Denied::Label, "{labels}");
    }
}

#[test]
fn too_many_labels_and_variables_are_denied() {
    let mut labels = serde_json::Map::new();
    labels.insert("iapetus.role".into(), json!("session"));
    for n in 0..16 {
        labels.insert(format!("iapetus.l{n}"), json!("x"));
    }
    assert_eq!(denied(&put("/Labels", Value::Object(labels))), Denied::Label);
    let env: Vec<String> = (0..65).map(|n| format!("IAPETUS_V{n}=1")).collect();
    assert!(matches!(denied(&put("/Env", json!(env))), Denied::Env(_)));
}

#[test]
fn foreign_labels_are_denied() {
    let labels = json!({ "iapetus.role": "session", "traefik.enable": "true" });
    assert_eq!(denied(&put("/Labels", labels)), Denied::Label);
}

#[test]
fn environment_is_limited_to_iapetus_variables() {
    for env in [
        json!(["PATH=/evil"]),
        json!(["LD_PRELOAD=/lib/x.so"]),
        json!(["iapetus_lower=1"]),
        json!(["IAPETUS_="]),
        json!(["IAPETUS_NO_EQUALS"]),
        json!(["IAPETUS_A=1", "IAPETUS_A=2"]),
        json!(["IAPETUS_A=nul\u{0}"]),
    ] {
        assert!(matches!(denied(&put("/Env", env.clone())), Denied::Env(_)), "{env}");
    }
}

#[test]
fn unknown_fields_are_denied_at_every_level() {
    for (pointer, value) in [
        ("/Cmd", json!(["sh"])),
        ("/Entrypoint", json!(["sh"])),
        ("/User", json!("root")),
        ("/NetworkingConfig", json!({})),
        ("/ExposedPorts", json!({})),
        ("/HostConfig/VolumesFrom", json!(["other"])),
        ("/HostConfig/PortBindings", json!({})),
        ("/HostConfig/ReadonlyRootfs", json!(false)),
        ("/HostConfig/CgroupParent", json!("/")),
        ("/HostConfig/Sysctls", json!({})),
        ("/HostConfig/Devices/0/Extra", json!(1)),
        ("/HostConfig/Mounts/0/BindOptions", json!({})),
        ("/HostConfig/Mounts/0/VolumeOptions", json!({ "DriverConfig": { "Name": "x" } })),
    ] {
        let mut body = valid_create();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        let parent = if parent.is_empty() {
            &mut body
        } else {
            body.pointer_mut(parent).unwrap()
        };
        parent[key] = value;
        assert_malformed(check(&body), "unknown field");
    }
}

#[test]
fn field_names_in_other_case_are_unknown() {
    // Docker (Go) читает ключи без учёта регистра, поэтому `privileged` для него — `Privileged`.
    for (from, to) in [("Privileged", "privileged"), ("Image", "image"), ("HostConfig", "hostconfig")] {
        let text = if from == "Privileged" {
            valid_create().to_string().replace("\"CapAdd\"", "\"privileged\":true,\"CapAdd\"")
        } else {
            valid_create().to_string().replace(&format!("\"{from}\""), &format!("\"{to}\""))
        };
        let result = template().check_create(text.as_bytes());
        assert!(matches!(result, Err(Denied::Malformed(_))), "{to} принят: {result:?}");
    }
}

#[test]
fn repeated_keys_are_denied() {
    let valid = valid_create().to_string();
    let repeated = [
        valid.replace("\"HostConfig\":{", "\"HostConfig\":{\"Privileged\":false,\"Privileged\":true,"),
        valid.replace("\"HostConfig\":{", "\"HostConfig\":{\"Privileged\":true,\"Privileged\":false,"),
        valid.replacen('{', &format!("{{\"Image\":\"{OTHER_IMAGE}\","), 1),
        valid.replace("\"Labels\":{", "\"Labels\":{\"iapetus.role\":\"x\","),
        valid.replace(
            "\"PathOnHost\":\"/dev/fuse\"",
            "\"PathOnHost\":\"/dev/kvm\",\"PathOnHost\":\"/dev/fuse\"",
        ),
        valid.replace("\"Memory\":1073741824,", "\"Memory\":1073741824,\"Memory\":1,"),
    ];
    for text in repeated {
        assert_malformed(template().check_create(text.as_bytes()), "duplicate");
    }
}

#[test]
fn input_that_is_not_a_template_object_is_denied() {
    for text in ["", "null", "[]", "\"x\"", "{", "{}", "{}x", "{\"Image\":}", "\u{0}"] {
        assert!(
            matches!(template().check_create(text.as_bytes()), Err(Denied::Malformed(_))),
            "{text:?}"
        );
    }
    let trailing = format!("{}x", valid_create());
    assert_malformed(template().check_create(trailing.as_bytes()), "trailing");
}

#[test]
fn template_rejects_unsafe_settings() {
    let limits = LIMITS;
    let digest = |image: &str| Template::new(vec![image.to_owned()], NETWORK.to_owned(), limits, None);
    assert!(digest(IMAGE).is_ok());
    for image in [
        "repo:latest",
        "repo@sha256:abc",
        "repo@sha512:aaaa",
        "@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(digest(image).is_err(), "{image}");
    }
    assert!(Template::new(Vec::new(), NETWORK.to_owned(), limits, None).is_err());
    for profile in ["unconfined", "", "a/b", "-x", "a b"] {
        assert!(
            Template::new(vec![IMAGE.to_owned()], NETWORK.to_owned(), limits, Some(profile.to_owned())).is_err(),
            "{profile:?}"
        );
    }
    for network in ["host", "none", "bridge", "container:x", ""] {
        assert!(
            Template::new(vec![IMAGE.to_owned()], network.to_owned(), limits, None).is_err(),
            "{network}"
        );
    }
}

fn volume(extra: &str) -> String {
    format!("{{\"Name\":\"claude-vol\",\"Labels\":{{\"iapetus.role\":\"session\"}}{extra}}}")
}

#[test]
fn volume_is_rebuilt_with_name_and_session_label_only() {
    let body = Template::check_volume(volume("").as_bytes()).unwrap();
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"Name":"claude-vol","Labels":{"iapetus.role":"session"}}"#
    );
}

#[test]
fn volume_with_driver_or_options_is_denied() {
    for extra in [
        ",\"DriverOpts\":{\"type\":\"none\",\"o\":\"bind\",\"device\":\"/\"}",
        ",\"DriverOpts\":{}",
        ",\"Driver\":\"local\"",
        ",\"Driver\":\"other\"",
    ] {
        assert_eq!(
            Template::check_volume(volume(extra).as_bytes()),
            Err(Denied::VolumeDriver),
            "{extra}"
        );
    }
}

#[test]
fn volume_needs_exactly_the_session_label() {
    for body in [
        r#"{"Name":"v"}"#,
        r#"{"Name":"v","Labels":{}}"#,
        r#"{"Name":"v","Labels":{"iapetus.role":"x"}}"#,
        r#"{"Name":"v","Labels":{"iapetus.role":"session","other":"1"}}"#,
    ] {
        assert_eq!(Template::check_volume(body.as_bytes()), Err(Denied::Label), "{body}");
    }
}

#[test]
fn volume_keeps_iapetus_labels() {
    let body = json!({ "Name": "v", "Labels": { "iapetus.role": "session", "iapetus.agent": "codex" } }).to_string();
    let rebuilt = String::from_utf8(Template::check_volume(body.as_bytes()).unwrap()).unwrap();
    assert!(rebuilt.contains(r#""iapetus.agent":"codex""#), "{rebuilt}");
}

#[test]
fn volume_name_and_fields_are_checked() {
    for name in ["", "../x", "a/b", "-x"] {
        let body = json!({ "Name": name, "Labels": { "iapetus.role": "session" } }).to_string();
        assert_eq!(Template::check_volume(body.as_bytes()), Err(Denied::Name), "{name:?}");
    }
    let unknown = volume(",\"ClusterVolumeSpec\":{}");
    assert!(matches!(Template::check_volume(unknown.as_bytes()), Err(Denied::Malformed(_))));
    let repeated = volume(",\"Name\":\"other\"");
    assert!(matches!(Template::check_volume(repeated.as_bytes()), Err(Denied::Malformed(_))));
}

/// Независимая от `check_create` проверка того, что ушло бы в Docker: только поля шаблона.
#[track_caller]
fn assert_only_template(body: &Value) {
    let keys = |value: &Value| -> Vec<String> { value.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(body), ["Image", "Labels", "Env", "HostConfig"], "верхний уровень");
    let host = &body["HostConfig"];
    assert_eq!(
        keys(host),
        [
            "CapAdd",
            "CapDrop",
            "SecurityOpt",
            "Tmpfs",
            "Devices",
            "NetworkMode",
            "Mounts",
            "Memory",
            "MemorySwap",
            "NanoCpus",
            "PidsLimit"
        ],
        "HostConfig"
    );
    assert_eq!(host["CapAdd"], json!(CAP_ADD));
    assert_eq!(host["CapDrop"], json!(["ALL"]));
    assert_eq!(host["SecurityOpt"], json!(["no-new-privileges", "apparmor=iapetus-session"]));
    assert_eq!(host["Tmpfs"].as_object().unwrap().len(), 2);
    assert_eq!(host["NetworkMode"], json!(NETWORK));
    assert_eq!(host["Devices"].as_array().unwrap().len(), 1);
    assert_eq!(host["Devices"][0]["PathOnHost"], json!("/dev/fuse"));
    assert_eq!(keys(&host["Mounts"][0]), ["Type", "Source", "Target"]);
    assert_eq!(host["Mounts"][0]["Type"], json!("volume"));
    assert_eq!(host["Memory"], host["MemorySwap"]);
    assert_eq!(body["Labels"]["iapetus.role"], json!("session"));
    assert_eq!(body["Image"], json!(IMAGE));
}

fn arbitrary_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        prop_oneof![
            Just("host"),
            Just("/"),
            Just("ALL"),
            Just("bind"),
            Just("SYS_ADMIN"),
            Just("IAPETUS_X=1"),
            Just(IMAGE),
            Just(NETWORK)
        ]
        .prop_map(Value::from),
    ];
    leaf.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
            prop::collection::btree_map("[A-Za-z]{1,10}", inner, 0..3).prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

proptest! {
    /// Что бы ни вставили в верное тело, принятое после пересборки состоит только из шаблона.
    #[test]
    fn accepted_bodies_are_rebuilt_to_the_template(
        parent in prop::sample::select(vec![
            "", "/HostConfig", "/HostConfig/Devices/0", "/HostConfig/Mounts/0", "/Labels",
        ]),
        key in prop::sample::select(vec![
            "Privileged", "Binds", "CapDrop", "PidMode", "Cmd", "SecurityOpt", "Tmpfs", "Memory", "Env",
            "NetworkMode", "Image", "Labels", "Extra", "CapAdd",
        ]),
        value in arbitrary_json(),
    ) {
        let mut body = valid_create();
        body.pointer_mut(parent).unwrap()[key] = value;
        if let Ok(checked) = check(&body) {
            let rebuilt: Value = serde_json::from_slice(&checked.body).unwrap();
            assert_only_template(&rebuilt);
        }
    }

    #[test]
    fn garbage_is_denied_without_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        assert!(template().check_create(&bytes).is_err(), "мусор принят как шаблон");
        assert!(Template::check_volume(&bytes).is_err(), "мусор принят как том");
    }

    #[test]
    fn arbitrary_json_is_denied_without_panic(value in arbitrary_json()) {
        assert!(check(&value).is_err(), "произвольный JSON принят как шаблон");
    }
}
