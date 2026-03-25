use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

fn write_config(config: &serde_json::Value) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut f, config).unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn run_true() {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let config = serde_json::json!({
        "user": { "uid": uid, "gid": gid },
        "process": { "args": ["true"] },
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg(f.path())
        .assert()
        .success();
}

#[test]
fn run_echo() {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let config = serde_json::json!({
        "user": { "uid": uid, "gid": gid },
        "process": { "args": ["echo", "hello"] },
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg(f.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
fn custom_environ() {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let config = serde_json::json!({
        "user": { "uid": uid, "gid": gid },
        "process": {
            "args": ["sh", "-c", "echo $HELLO"],
            "environ": { "HELLO": "hello world" },
        },
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg(f.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}
