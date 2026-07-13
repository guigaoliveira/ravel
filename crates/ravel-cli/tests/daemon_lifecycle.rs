use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn command(binary: &str, root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(binary)
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn concurrent_start_shares_one_ready_daemon_and_cli_uses_it() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/base.ts"),
        "export function daemonBase() { return 1; }\n",
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    assert!(command(binary, root.path(), &["index"]).status.success());

    let mut first = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["daemon", "start"])
        .spawn()
        .unwrap();
    let mut second = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["daemon", "start"])
        .spawn()
        .unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let watch_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join(".ravel/watch.lock"))
        .unwrap();
    assert!(
        !fs4::fs_std::FileExt::try_lock_exclusive(&watch_lock).unwrap(),
        "daemon must own the cross-process watcher leadership lock"
    );

    let status = command(binary, root.path(), &["daemon", "status"]);
    assert!(status.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"],
        true
    );

    fs::write(
        root.path().join("src/added.ts"),
        "export const daemonAdded = daemonBase();\n",
    )
    .unwrap();
    assert!(
        command(binary, root.path(), &["sync", "src/added.ts"])
            .status
            .success()
    );
    let context = command(binary, root.path(), &["context", "daemonAdded"]);
    assert!(context.status.success());
    assert!(String::from_utf8_lossy(&context.stdout).contains("daemonAdded"));

    fs::write(
        root.path().join("src/watched.ts"),
        "export const watcherFreshness = 42;\n",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let context = command(binary, root.path(), &["context", "watcherFreshness"]);
        if context.status.success()
            && String::from_utf8_lossy(&context.stdout).contains("watcherFreshness")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon watcher did not publish the edit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    fs::write(
        root.path().join("src/parallel-a.ts"),
        "export const parallelA = 1;\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/parallel-b.ts"),
        "export const parallelB = 2;\n",
    )
    .unwrap();
    let mut sync_a = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["sync", "src/parallel-a.ts"])
        .spawn()
        .unwrap();
    let mut sync_b = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["sync", "src/parallel-b.ts"])
        .spawn()
        .unwrap();
    assert!(sync_a.wait().unwrap().success());
    assert!(sync_b.wait().unwrap().success());
    let context_a = command(binary, root.path(), &["context", "parallelA"]);
    let context_b = command(binary, root.path(), &["context", "parallelB"]);
    assert!(String::from_utf8_lossy(&context_a.stdout).contains("parallelA"));
    assert!(String::from_utf8_lossy(&context_b.stdout).contains("parallelB"));

    let stop = command(binary, root.path(), &["daemon", "stop"]);
    assert!(stop.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stop.stdout).unwrap()["stopped"],
        true
    );
}

#[test]
fn transient_daemon_exits_after_mcp_lease_disconnects() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/base.ts"), "export const base = 1;\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    assert!(command(binary, root.path(), &["index"]).status.success());

    let mut mcp = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = command(binary, root.path(), &["daemon", "status"]);
        if status.status.success()
            && serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"] == true
        {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "MCP daemon never became ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    mcp.kill().unwrap();
    mcp.wait().unwrap();

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = command(binary, root.path(), &["daemon", "status"]);
        let running = serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"] == true;
        if !running {
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "daemon outlived its final MCP lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn explicit_start_promotes_a_transient_daemon() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/base.ts"), "export const base = 1;\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    assert!(command(binary, root.path(), &["index"]).status.success());
    let mut mcp = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !command(binary, root.path(), &["daemon", "status"])
        .stdout
        .starts_with(b"{\"running\":true")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        command(binary, root.path(), &["daemon", "start"])
            .status
            .success()
    );
    mcp.kill().unwrap();
    mcp.wait().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let status = command(binary, root.path(), &["daemon", "status"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"],
        true
    );
    assert!(
        command(binary, root.path(), &["daemon", "stop"])
            .status
            .success()
    );
}

#[test]
fn bootstrap_pipe_cleans_transient_daemon_before_first_lease() {
    let root = tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    let mut daemon = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["daemon-serve", "--transient"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = command(binary, root.path(), &["daemon", "status"]);
        if serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"] == true {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
    let status = command(binary, root.path(), &["daemon", "status"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"],
        false
    );
}

#[test]
fn daemon_accepts_clients_while_waiting_for_watcher_leadership() {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/base.ts"), "export const base = 1;\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    assert!(command(binary, root.path(), &["index"]).status.success());

    let watch_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.path().join(".ravel/watch.lock"))
        .unwrap();
    assert!(fs4::fs_std::FileExt::try_lock_exclusive(&watch_lock).unwrap());
    let mut daemon = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["daemon-serve", "--transient"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = command(binary, root.path(), &["daemon", "status"]);
        if serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"] == true {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watch leadership blocked daemon RPC"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    fs4::fs_std::FileExt::unlock(&watch_lock).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !fs4::fs_std::FileExt::try_lock_exclusive(&watch_lock).unwrap() {
            break;
        }
        fs4::fs_std::FileExt::unlock(&watch_lock).unwrap();
        assert!(
            Instant::now() < deadline,
            "daemon watcher did not take over leadership"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn daemon_lease_limit_is_hard_and_does_not_consume_more_connections() {
    let root = tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    let mut daemon = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["daemon-serve", "--transient"])
        .env("RAVEL_DAEMON_MAX_CONNECTIONS", "40")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = ravel_core::daemon::DaemonClient::for_root(root.path()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !client.is_ready() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut leases = Vec::new();
    for _ in 0..32 {
        leases.push(client.acquire_lease().unwrap());
    }
    for _ in 0..32 {
        assert!(client.acquire_lease().is_err());
    }
    assert!(
        client.is_ready(),
        "rejected leases consumed request capacity"
    );
    drop(leases.pop());
    let replacement = client.acquire_lease().unwrap();
    drop(replacement);
    drop(leases);
    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
}
