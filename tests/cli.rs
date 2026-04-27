use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

fn write_config(config: &serde_json::Value) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut f, config).unwrap();
    f.flush().unwrap();
    f
}

// cbuildrt must work without any subcommand for backwards compatibility with old xbstrap.
#[test]
fn no_subcommand() {
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
        .args(["run"])
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
        .args(["run"])
        .arg(f.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
fn auto_subuid_subgid() {
    let ws = tempfile::tempdir().unwrap();
    Command::cargo_bin("cbuildrt")
        .unwrap()
        .args(["--workspace"])
        .arg(ws.path())
        .arg("init")
        .assert()
        .success();

    let config = serde_json::json!({
        "user": { "uid": 0, "gid": 0 },
        "process": { "args": ["/usr/bin/id"] },
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg("--workspace")
        .arg(ws.path())
        .arg("run")
        .arg(f.path())
        .assert()
        .success()
        .stdout(predicate::str::starts_with("uid=0(root) gid=0(root)"));
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
        .args(["run"])
        .arg(f.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

// Test if we can write to an overlayfs with an upper layer (with subuid/subgid).
// This is a regression test for breakage when using certain AppArmor profiles.
#[test]
fn subid_overlay_writes() {
    let ws = tempfile::tempdir().unwrap();
    println!("workspace is at: {:?}", ws.path());

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg("--workspace")
        .arg(ws.path())
        .arg("init")
        .assert()
        .success();

    let lower = tempfile::tempdir().unwrap();
    let extract = tempfile::Builder::new().suffix(".tar").tempfile().unwrap();

    let config = serde_json::json!({
        "user": { "uid": 0, "gid": 0 },
        "process": { "args": ["touch", "hello"] },
        "rootfs": {
            "layers": [lower.path()],
            "withUpper": true,
            "extractUpper": extract.path(),
        },
        "noChroot": true,
        "noSystemMounts": true,
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg("--workspace")
        .arg(ws.path())
        .arg("run")
        .arg(f.path())
        .assert()
        .success();

    let mut archive = tar::Archive::new(
        std::fs::File::open(extract).expect("failed to open extracted upper tar"),
    );
    let found_file = archive
        .entries()
        .expect("failed to read tar entries")
        .any(|e| {
            let entry = e.expect("failed to read tar entry");
            entry.path().unwrap().as_os_str() == "hello"
        });
    assert!(
        found_file,
        "file created by touch is not present in extracted upper dir"
    );
}

// Test that importUpper seeds the overlay's upper layer from a tar archive.
#[test]
fn overlay_import_upper() {
    let lower = tempfile::tempdir().unwrap();

    // Build a tar containing a single file.
    let import_tar = tempfile::Builder::new().suffix(".tar").tempfile().unwrap();
    {
        let tar_file = std::fs::File::create(import_tar.path()).unwrap();
        let mut builder = tar::Builder::new(tar_file);
        let data = b"world\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("hello").unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        builder.finish().unwrap();
    }

    let config = serde_json::json!({
        "user": { "uid": 0, "gid": 0 },
        "process": { "args": ["cat", "hello"] },
        "rootfs": {
            "layers": [lower.path()],
            "withUpper": true,
            "importUpper": import_tar.path(),
        },
        "noChroot": true,
        "noSystemMounts": true,
        "bindMounts": [],
    });
    let f = write_config(&config);

    Command::cargo_bin("cbuildrt")
        .unwrap()
        .arg("run")
        .arg(f.path())
        .assert()
        .success();
}
