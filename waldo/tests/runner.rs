//! Запуск runner: проверки рабочего каталога и каталога личности до подключения к сети.

use std::fs;

use clap::Parser;

use super::*;
use crate::testing;

/// Настройки runner из командной строки и окружения, как их разбирает waldo.
fn parse(command_line: &[&str]) -> Args {
    match crate::Cli::try_parse_from(["waldo", "runner"].into_iter().chain(command_line.iter().copied()))
        .unwrap()
        .mode
    {
        crate::Mode::Runner(args) => args,
        crate::Mode::Worker(_) => unreachable!("разобран не runner"),
    }
}

/// Настройки runner без сети: до server ни одна проверка запуска не доходит.
fn offline(workdir: &str, identity: &Path, extra: &[&str]) -> Args {
    let identity = identity.to_str().unwrap();
    let mut argv = vec![
        workdir,
        "--orbit-url",
        "127.0.0.1:1",
        "--orbit-ca",
        "/nonexistent/ca.pub",
        "--identity-dir",
        identity,
    ];
    argv.extend_from_slice(extra);
    parse(&argv)
}

#[tokio::test]
async fn runner_on_a_container_path_does_not_start_also_through_a_link_or_dotdot() {
    let dir = testing::workdir();
    let root = fs::canonicalize(dir.path()).unwrap();
    std::os::unix::fs::symlink("/usr/share", root.join("link_usr")).unwrap();
    let up = "../".repeat(root.components().count() - 1);
    let identity = root.join("identity");
    let cases = [
        ("совпадает", "/tmp".to_owned()),
        ("ссылка внутрь", root.join("link_usr").to_string_lossy().into_owned()),
        ("через ..", root.join(up).join("tmp").to_string_lossy().into_owned()),
    ];
    for (name, workdir) in cases {
        let error = run(offline(&workdir, &identity, &[])).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("контейнера session"),
            "runner в каталоге на пути контейнера ({name}, {workdir}) не отклонён как таковой: {error}"
        );
        assert!(
            !identity.exists(),
            "runner занял каталог личности до проверки рабочего каталога ({name})"
        );
    }
}

#[tokio::test]
async fn clone_without_an_identity_dir_from_its_parent_does_not_start() {
    // Без каталога от родителя клон в каталоге runner занял бы его личность, а при остановке удалил бы её.
    let dir = testing::workdir();
    let work = dir.path().join("work");
    fs::create_dir(&work).unwrap();
    let mut args = offline(work.to_str().unwrap(), &dir.path().join("identity"), &["--temporary"]);
    args.identity_dir = None;
    let error = run(args).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("--identity-dir"),
        "клон без каталога личности от родителя запустился или ошибка не о нём: {error:#}"
    );
}

/// Родитель запускает клон с `--home on|off`: флаг без значения отвергал бы его, и клон не запускался бы.
#[test]
fn home_flag_takes_on_and_off_as_a_value() {
    let network = ["--orbit-url", "x:1", "--orbit-ca", "ca"];
    let with = |value: &[&str]| parse(&[value, &network[..]].concat());
    assert!(!with(&["--home", "off"]).home, "--home off не выключил отдачу $HOME");
    assert!(with(&["--home", "on"]).home, "--home on не включил отдачу $HOME");
    assert!(with(&[]).home, "по умолчанию $HOME не отдаётся");
}
