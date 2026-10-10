use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::tempdir;

const TEST_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

fn acquire_lease_until(
    client: &ravel_core::daemon::DaemonClient,
    deadline: Instant,
) -> ravel_core::daemon::DaemonClientLease {
    loop {
        match client.acquire_lease() {
            Ok(lease) => return lease,
            Err(_) if Instant::now() < deadline => std::thread::yield_now(),
            Err(error) => panic!("daemon lease capacity was not replenished: {error}"),
        }
    }
}

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
        .env(
            "RAVEL_DAEMON_REQUEST_TIMEOUT_MS",
            TEST_DAEMON_REQUEST_TIMEOUT.as_millis().to_string(),
        )
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
    // Closing the client socket and observing EOF are asynchronous across the IPC boundary.
    // Retry until the daemon's configured request deadline rather than assuming immediate EOF.
    let replacement = acquire_lease_until(&client, Instant::now() + TEST_DAEMON_REQUEST_TIMEOUT);
    drop(replacement);
    drop(leases);
    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
}

fn indexed_workspace(symbol: &str) -> tempfile::TempDir {
    let root = tempdir().unwrap();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("src/base.ts"),
        format!("export function {symbol}() {{ return 1; }}\n"),
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_ravel");
    assert!(command(binary, root.path(), &["index"]).status.success());
    root
}

fn transient_daemon(root: &Path, max_connections: &str) -> std::process::Child {
    let binary = env!("CARGO_BIN_EXE_ravel");
    let daemon = Command::new(binary)
        .arg("--root")
        .arg(root)
        .args(["daemon-serve", "--transient"])
        .env("RAVEL_DAEMON_MAX_CONNECTIONS", max_connections)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = ravel_core::daemon::DaemonClient::for_root(root).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.is_ready() {
        assert!(Instant::now() < deadline, "daemon never became ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

#[test]
fn a_lease_answers_queries_without_taking_another_connection() {
    use ravel_core::daemon::DaemonOperation;

    let root = indexed_workspace("leaseBase");
    // One connection in all, and the lease holds it: anything that needed a connection of its own
    // would be turned away.
    let mut daemon = transient_daemon(root.path(), "1");
    let client = ravel_core::daemon::DaemonClient::for_root(root.path()).unwrap();
    let lease = acquire_lease_until(&client, Instant::now() + Duration::from_secs(5));

    for _ in 0..5 {
        let status = lease.call_text(DaemonOperation::Status).unwrap();
        assert!(status.contains(r#""indexed":true"#), "{status}");
    }
    let context = lease
        .call_text(DaemonOperation::Context {
            query: "leaseBase".into(),
            limit: 5,
            detail: false,
        })
        .unwrap();
    assert!(context.contains("leaseBase"), "{context}");
    assert!(
        client.call(DaemonOperation::Status).is_err(),
        "the lease should have used the daemon's only connection"
    );

    drop(lease);
    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
}

#[test]
fn a_lease_refuses_lifecycle_operations_and_keeps_serving() {
    use ravel_core::daemon::{DaemonCallError, DaemonOperation};

    let root = indexed_workspace("leaseKeeps");
    let mut daemon = transient_daemon(root.path(), "8");
    let client = ravel_core::daemon::DaemonClient::for_root(root.path()).unwrap();
    let lease = acquire_lease_until(&client, Instant::now() + Duration::from_secs(5));

    match lease.call_text(DaemonOperation::Shutdown) {
        Err(DaemonCallError::Remote(message)) => {
            assert!(message.contains("not served"), "{message}")
        }
        other => panic!("a lease must not stop its daemon: {other:?}"),
    }
    assert!(
        lease.call_text(DaemonOperation::Status).is_ok(),
        "a refused operation must not end the lease"
    );
    assert!(client.is_ready(), "the daemon must still be up");

    drop(lease);
    drop(daemon.stdin.take());
    assert!(daemon.wait().unwrap().success());
}

/// A daemon started the way `daemon start` starts one (not transient), killed if the test ends
/// before it exits on its own.
struct PersistentDaemon(std::process::Child);

impl PersistentDaemon {
    fn start(root: &Path) -> Self {
        let daemon = Self(
            Command::new(env!("CARGO_BIN_EXE_ravel"))
                .arg("--root")
                .arg(root)
                .arg("daemon-serve")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let client = ravel_core::daemon::DaemonClient::for_root(root).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.is_ready() {
            assert!(Instant::now() < deadline, "daemon never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn exits_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.0.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for PersistentDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn stop_leaves_the_daemon_to_its_sessions_until_the_last_one_ends() {
    use ravel_core::daemon::DaemonOperation;

    let root = indexed_workspace("stopShared");
    let binary = env!("CARGO_BIN_EXE_ravel");
    let mut daemon = PersistentDaemon::start(root.path());
    let client = ravel_core::daemon::DaemonClient::for_root(root.path()).unwrap();
    // An MCP session holds a lease on the daemon `daemon start` began.
    let session = acquire_lease_until(&client, Instant::now() + Duration::from_secs(5));

    let stop = command(binary, root.path(), &["daemon", "stop"]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    // Still serving everyone: the session, the CLI, and a session that joins now. A stop that
    // shut down under the session left a daemon that refused every call and could not exit.
    assert!(
        session.call_text(DaemonOperation::Status).is_ok(),
        "the session lost its daemon to `stop`"
    );
    assert!(
        client.call(DaemonOperation::Status).is_ok(),
        "the daemon refused a call after `stop`"
    );
    let context = command(binary, root.path(), &["context", "stopShared"]);
    assert!(
        context.status.success() && String::from_utf8_lossy(&context.stdout).contains("stopShared"),
        "{}",
        String::from_utf8_lossy(&context.stderr)
    );
    let status = command(binary, root.path(), &["daemon", "status"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["running"],
        true
    );
    let joined = acquire_lease_until(&client, Instant::now() + Duration::from_secs(2));
    drop(joined);

    // `start` takes it back, and then the daemon outlives the session.
    assert!(
        command(binary, root.path(), &["daemon", "start"])
            .status
            .success()
    );
    drop(session);
    assert!(
        !daemon.exits_within(Duration::from_millis(300)),
        "a daemon started again exited with its session"
    );

    // Stopped again with a session attached, it exits once that session ends.
    let session = acquire_lease_until(&client, Instant::now() + Duration::from_secs(5));
    assert!(
        command(binary, root.path(), &["daemon", "stop"])
            .status
            .success()
    );
    assert!(
        !daemon.exits_within(Duration::from_millis(200)),
        "`stop` ended the daemon under its session"
    );
    drop(session);
    assert!(
        daemon.exits_within(Duration::from_secs(5)),
        "a stopped daemon outlived its last session"
    );
}

/// Process ids of the daemons serving `root`, found the way an operator would: by command line.
#[cfg(target_os = "linux")]
fn daemon_pids(root: &Path) -> Vec<u32> {
    let root = root.canonicalize().unwrap();
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        let serves = args.iter().any(|arg| *arg == b"daemon-serve");
        let for_root = args
            .iter()
            .any(|arg| Path::new(std::str::from_utf8(arg).unwrap_or("")) == root);
        if serves && for_root {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(target_os = "linux")]
#[test]
fn an_mcp_session_recovers_when_its_daemon_is_killed() {
    use std::io::{BufRead, BufReader, Write};

    struct Session {
        stdin: std::process::ChildStdin,
        lines: std::sync::mpsc::Receiver<String>,
        next_id: u64,
    }
    impl Session {
        fn request(&mut self, method: &str, params: &str) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            writeln!(
                self.stdin,
                r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
            )
            .unwrap();
            self.stdin.flush().unwrap();
            loop {
                let line = self
                    .lines
                    .recv_timeout(Duration::from_secs(30))
                    .expect("`ravel mcp` did not answer");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        }

        fn status(&mut self) -> Value {
            let reply = self.request("tools/call", r#"{"name":"status","arguments":{}}"#);
            assert_ne!(reply["result"]["isError"], true, "{reply}");
            let text = reply["result"]["content"][0]["text"].as_str().unwrap();
            serde_json::from_str(text).unwrap()
        }
    }

    let root = indexed_workspace("survivesKill");
    let binary = env!("CARGO_BIN_EXE_ravel");
    let mut mcp = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = mcp.stdout.take().unwrap();
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut session = Session {
        stdin: mcp.stdin.take().unwrap(),
        lines,
        next_id: 0,
    };
    session.request(
        "initialize",
        r#"{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}"#,
    );
    writeln!(
        session.stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();

    assert_eq!(session.status()["indexed"], true);
    let before = daemon_pids(root.path());
    assert_eq!(before.len(), 1, "expected one daemon, found {before:?}");

    // The daemon dies under a session that holds a lease on it and sends its calls down it.
    assert!(
        Command::new("sh")
            .args(["-c", &format!("kill -9 {}", before[0])])
            .status()
            .unwrap()
            .success()
    );
    // Its parent never reaps it, so "dead" is a missing entry or a zombie.
    let alive = |pid: u32| {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit(") ")
                .next()
                .is_some_and(|rest| !rest.starts_with('Z'))
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(before[0]) {
        assert!(Instant::now() < deadline, "the daemon did not die");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The next call starts a new daemon and a new lease instead of failing the agent's request.
    assert_eq!(session.status()["indexed"], true);
    let after = daemon_pids(root.path());
    assert_eq!(after.len(), 1, "expected a new daemon, found {after:?}");
    assert_ne!(after, before);
    assert_eq!(session.status()["indexed"], true);

    drop(session);
    mcp.wait().unwrap();
}
