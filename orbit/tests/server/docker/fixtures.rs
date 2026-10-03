//! Общие данные тестов: шаблон и тело `create`, которое он принимает.

use serde_json::{Value, json};

use super::template::{Limits, Template};

pub(super) const IMAGE: &str = "registry.example/iapetus/session@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(super) const OTHER_IMAGE: &str =
    "registry.example/iapetus/session@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub(super) const NETWORK: &str = "iapetus-sessions";
pub(super) const APPARMOR: &str = "iapetus-session";
pub(super) const LIMITS: Limits = Limits {
    memory: 2 << 30,
    nano_cpus: 2_000_000_000,
    pids: 512,
    work: 2 << 30,
};

pub(super) fn template() -> Template {
    Template::new(vec![IMAGE.to_owned()], NETWORK.to_owned(), LIMITS, Some(APPARMOR.to_owned())).unwrap()
}

/// Capabilities, которые шаблон добавляет поверх `CapDrop=ALL`.
pub(super) const CAP_ADD: [&str; 4] = ["SYS_ADMIN", "SETUID", "SETGID", "SETPCAP"];

/// Тело, которое шаблон принимает; тесты ломают в нём ровно одно условие.
pub(super) fn valid_create() -> Value {
    json!({
        "Image": IMAGE,
        "Labels": { "iapetus.role": "session", "iapetus.agent": "claude" },
        "Env": ["IAPETUS_TICKET=abc"],
        "HostConfig": {
            "CapAdd": CAP_ADD,
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges", "apparmor=iapetus-session"],
            "Tmpfs": {
                "/work": "rw,noexec,nosuid,nodev,mode=0700,size=1073741824",
                "/run/secrets": "rw,noexec,nosuid,nodev,mode=0700,size=1m"
            },
            "Devices": [{
                "PathOnHost": "/dev/fuse",
                "PathInContainer": "/dev/fuse",
                "CgroupPermissions": "rwm"
            }],
            "NetworkMode": NETWORK,
            "Mounts": [{ "Type": "volume", "Source": "claude-vol", "Target": "/home/agent/.claude" }],
            "Memory": 1 << 30,
            "MemorySwap": 1 << 30,
            "NanoCpus": 1_000_000_000,
            "PidsLimit": 256
        }
    })
}
