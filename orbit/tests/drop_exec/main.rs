//! Понижение прав агента: настоящий бинарь `orbit drop-exec` запускает команду, которая печатает свои права.
//!
//! Тесты работают, только когда сами идут от root: без него `setresuid` на другого пользователя невозможен, и
//! проверять нечего. Утверждение о bounding set осмысленно, если у процесса был `CAP_SYS_ADMIN` (контейнер
//! запущен с `--cap-add SYS_ADMIN`, как `check` в `docker/docker-compose.test.yml`): иначе убирать нечего.

#![expect(
    clippy::unwrap_used,
    reason = "помощники стенда: сбой настройки должен ронять тест, как unwrap в самом тесте"
)]

use std::collections::HashMap;
use std::process::Command;

use rustix::thread::{capabilities, set_capabilities};

const AGENT_UID: &str = "65534";
const SYS_ADMIN_BIT: u64 = 21;

fn read_status(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_owned(), value.trim().to_owned()))
        .collect()
}

fn own_status() -> HashMap<String, String> {
    read_status(&std::fs::read_to_string("/proc/self/status").unwrap())
}

fn is_root() -> bool {
    own_status()["Uid"].split_whitespace().all(|id| id == "0")
}

fn has_sys_admin() -> bool {
    (u64::from_str_radix(&own_status()["CapBnd"], 16).unwrap() >> SYS_ADMIN_BIT) & 1 == 1
}

/// Права процесса, который `orbit drop-exec` запустил после понижения.
fn status_after_drop() -> HashMap<String, String> {
    // Контейнер запускает процесс с пустым inheritable: без этой подготовки очистка была бы нечем проверена.
    let mut sets = capabilities(None).unwrap();
    sets.inheritable = sets.permitted;
    set_capabilities(None, sets).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args([
            "drop-exec",
            "--uid",
            AGENT_UID,
            "--gid",
            AGENT_UID,
            "--home",
            "/tmp",
            "--cwd",
            "/tmp",
            "--",
        ])
        .args(["cat", "/proc/self/status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "drop-exec не отработал: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_status(&String::from_utf8(output.stdout).unwrap())
}

#[test]
fn sys_admin_is_removed_from_the_bounding_set() {
    if !is_root() || !has_sys_admin() {
        return;
    }
    let bounding = u64::from_str_radix(&status_after_drop()["CapBnd"], 16).unwrap();
    assert_eq!(
        (bounding >> SYS_ADMIN_BIT) & 1,
        0,
        "CAP_SYS_ADMIN остался в bounding set: {bounding:x}"
    );
}

#[test]
fn inheritable_capabilities_are_cleared() {
    if !is_root() {
        return;
    }
    assert_eq!(status_after_drop()["CapInh"], "0000000000000000");
}

#[test]
fn no_new_privs_is_set() {
    if !is_root() {
        return;
    }
    assert_eq!(status_after_drop()["NoNewPrivs"], "1");
}

#[test]
fn supplementary_groups_are_dropped() {
    if !is_root() {
        return;
    }
    assert_eq!(status_after_drop()["Groups"], "", "дополнительные группы root остались");
}

#[test]
fn all_four_uids_and_gids_are_switched() {
    if !is_root() {
        return;
    }
    let status = status_after_drop();
    for key in ["Uid", "Gid"] {
        assert!(
            status[key].split_whitespace().all(|id| id == AGENT_UID),
            "{key} не сменён во всех четырёх полях: {}",
            status[key]
        );
    }
}

#[test]
fn nothing_is_left_in_permitted_and_effective_sets() {
    if !is_root() {
        return;
    }
    let status = status_after_drop();
    assert_eq!(status["CapPrm"], "0000000000000000", "в permitted остались права после смены uid");
    assert_eq!(status["CapEff"], "0000000000000000", "в effective остались права после смены uid");
}
