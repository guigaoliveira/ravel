//! Versioned local-IPC contracts for the optional shared workspace daemon.
//!
//! This module deliberately does not start a daemon or change CLI/MCP routing.  It keeps the
//! transport contract, root identity and singleton primitive independent from either frontend so
//! the direct engine remains a safe fallback during rollout.

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use thiserror::Error;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
/// Defensive protocol ceiling. Callers may choose a lower bound when reading untrusted peers.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 100;
const WATCH_STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(3);
const DAEMON_READY_POLL: Duration = Duration::from_millis(20);
const DEFAULT_DAEMON_MIN_CONNECTIONS: usize = 8;
const DEFAULT_DAEMON_CONNECTIONS_PER_CPU: usize = 4;
const DEFAULT_DAEMON_MAX_LEASES: usize = 32;
const DEFAULT_DAEMON_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn max_connections() -> usize {
    std::env::var("RAVEL_DAEMON_MAX_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(DEFAULT_DAEMON_MIN_CONNECTIONS, usize::from)
                .saturating_mul(DEFAULT_DAEMON_CONNECTIONS_PER_CPU)
                .max(DEFAULT_DAEMON_MIN_CONNECTIONS)
        })
}

fn max_leases() -> usize {
    std::env::var("RAVEL_DAEMON_MAX_LEASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_DAEMON_MAX_LEASES)
}

fn request_timeout() -> Duration {
    std::env::var("RAVEL_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_REQUEST_TIMEOUT)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RootIdentity(String);

impl RootIdentity {
    pub fn discover(root: &Path) -> io::Result<Self> {
        let canonical = root.canonicalize()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ravel-root-v1\0");
        hash_platform_path(&mut hasher, &canonical);

        // A filesystem identity prevents two directory entries that resolve to different roots
        // from aliasing merely because their display spelling happens to normalize equally.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = canonical.metadata()?;
            hasher.update(&metadata.dev().to_le_bytes());
            hasher.update(&metadata.ino().to_le_bytes());
        }

        Ok(Self(hasher.finalize().to_hex().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn hash_platform_path(hasher: &mut blake3::Hasher, path: &Path) {
    #[cfg(windows)]
    let value = path.to_string_lossy().replace('/', "\\").to_lowercase();
    #[cfg(not(windows))]
    let value = path.to_string_lossy();
    hasher.update(value.as_bytes());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEndpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    #[cfg(windows)]
    WindowsPipe(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLayout {
    pub directory: PathBuf,
    pub endpoint: LocalEndpoint,
    pub singleton_lock: PathBuf,
}

impl RuntimeLayout {
    pub fn for_root(root: &RootIdentity) -> io::Result<Self> {
        Self::in_directory(runtime_base()?, root)
    }

    pub fn in_directory(base: PathBuf, root: &RootIdentity) -> io::Result<Self> {
        let short = root.as_str().get(..32).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid daemon root identity")
        })?;
        #[cfg(unix)]
        let directory = {
            use std::os::unix::ffi::OsStrExt;
            let directory = base.join("ravel");
            let projected = directory.join(format!("{short}.sock"));
            if projected.as_os_str().as_bytes().len() > MAX_UNIX_SOCKET_PATH_BYTES {
                short_unix_runtime_directory(&base)?
            } else {
                directory
            }
        };
        #[cfg(not(unix))]
        let directory = base.join("ravel");
        std::fs::create_dir_all(&directory)?;
        restrict_runtime_directory(&directory)?;
        let singleton_lock = directory.join(format!("{short}.lock"));
        #[cfg(unix)]
        let endpoint = {
            use std::os::unix::ffi::OsStrExt;
            let path = directory.join(format!("{short}.sock"));
            if path.as_os_str().as_bytes().len() > MAX_UNIX_SOCKET_PATH_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon socket path exceeds the portable Unix limit",
                ));
            }
            LocalEndpoint::Unix(path)
        };
        #[cfg(windows)]
        let endpoint =
            LocalEndpoint::WindowsPipe(format!(r"\\.\pipe\ravel-{}-v{}", short, PROTOCOL_MAJOR));
        Ok(Self {
            directory,
            endpoint,
            singleton_lock,
        })
    }
}

#[cfg(unix)]
fn short_unix_runtime_directory(base: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    // Unix socket limits apply to the pathname passed to bind, even when the user's private
    // TMPDIR is deeply nested (notably on macOS). Namespace the short fallback by the owner of
    // that private runtime base; the directory is subsequently verified and restricted to 0700.
    let uid = std::fs::metadata(base)?.uid();
    let directory = PathBuf::from(format!("/tmp/ravel-{uid}"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon runtime fallback directory",
        ));
    }
    Ok(directory)
}

fn runtime_base() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = std::env::var_os("TMPDIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".cache"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no private runtime directory"))
}

fn restrict_runtime_directory(_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Held for the lifetime of one daemon. The OS releases it after a crash.
#[derive(Debug)]
pub struct DaemonLease {
    _file: File,
}

impl DaemonLease {
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if file.try_lock_exclusive()? {
            Ok(Some(Self { _file: file }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub client_version: String,
    pub root: RootIdentity,
}

impl ClientHello {
    pub fn current(root: RootIdentity) -> Self {
        Self {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            client_version: crate::VERSION.to_owned(),
            root,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub server_version: String,
    pub root: RootIdentity,
}

pub fn validate_handshake(
    client: &ClientHello,
    server: &ServerHello,
) -> Result<(), HandshakeError> {
    if client.protocol_major != server.protocol_major {
        return Err(HandshakeError::Protocol {
            client: client.protocol_major,
            server: server.protocol_major,
        });
    }
    if client.root != server.root {
        return Err(HandshakeError::RootMismatch);
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("incompatible daemon protocol: client v{client}, server v{server}")]
    Protocol { client: u16, server: u16 },
    #[error("daemon belongs to a different workspace root")]
    RootMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonOperation {
    Status,
    Context { query: String, limit: usize },
    Sync { paths: Vec<PathBuf> },
    Lease,
    PromotePersistent,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum WireRequest {
    Hello(ClientHello),
    Operation(DaemonOperation),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum WireResponse {
    Hello(ServerHello),
    Value(Value),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    root: RootIdentity,
    layout: RuntimeLayout,
}

impl DaemonClient {
    pub fn for_root(root: &Path) -> io::Result<Self> {
        let root = RootIdentity::discover(root)?;
        let layout = RuntimeLayout::for_root(&root)?;
        Ok(Self { root, layout })
    }

    pub fn call(&self, operation: DaemonOperation) -> Result<Value, DaemonCallError> {
        let mut stream = self.connect_and_handshake()?;
        write_frame(&mut stream, &WireRequest::Operation(operation))
            .map_err(DaemonCallError::Transport)?;
        match read_frame::<WireResponse>(&mut stream).map_err(DaemonCallError::Transport)? {
            WireResponse::Value(value) => Ok(value),
            WireResponse::Error(error) => Err(DaemonCallError::Remote(error)),
            WireResponse::Hello(_) => Err(DaemonCallError::Transport(invalid_protocol(
                "unexpected daemon hello",
            ))),
        }
    }

    pub fn acquire_lease(&self) -> Result<DaemonClientLease, DaemonCallError> {
        let mut stream = self.connect_and_handshake()?;
        write_frame(&mut stream, &WireRequest::Operation(DaemonOperation::Lease))
            .map_err(DaemonCallError::Transport)?;
        match read_frame::<WireResponse>(&mut stream).map_err(DaemonCallError::Transport)? {
            WireResponse::Value(_) => Ok(DaemonClientLease { _stream: stream }),
            WireResponse::Error(error) => Err(DaemonCallError::Remote(error)),
            WireResponse::Hello(_) => Err(DaemonCallError::Transport(invalid_protocol(
                "unexpected daemon hello",
            ))),
        }
    }

    fn connect_and_handshake(&self) -> Result<interprocess::local_socket::Stream, DaemonCallError> {
        use interprocess::local_socket::prelude::*;
        let mut stream = match &self.layout.endpoint {
            #[cfg(unix)]
            LocalEndpoint::Unix(path) => {
                let name = path
                    .clone()
                    .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                    .map_err(DaemonCallError::Transport)?;
                LocalSocketStream::connect(name).map_err(DaemonCallError::Transport)?
            }
            #[cfg(windows)]
            LocalEndpoint::WindowsPipe(name) => {
                let name = name
                    .clone()
                    .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
                    .map_err(DaemonCallError::Transport)?;
                LocalSocketStream::connect(name).map_err(DaemonCallError::Transport)?
            }
        };
        write_frame(
            &mut stream,
            &WireRequest::Hello(ClientHello::current(self.root.clone())),
        )
        .map_err(DaemonCallError::Transport)?;
        let server =
            match read_frame::<WireResponse>(&mut stream).map_err(DaemonCallError::Transport)? {
                WireResponse::Hello(server) => server,
                WireResponse::Error(error) => return Err(DaemonCallError::Remote(error)),
                WireResponse::Value(_) => {
                    return Err(DaemonCallError::Transport(invalid_protocol(
                        "expected daemon hello",
                    )));
                }
            };
        validate_handshake(&ClientHello::current(self.root.clone()), &server)
            .map_err(|error| DaemonCallError::Transport(io::Error::other(error)))?;
        Ok(stream)
    }

    pub fn is_ready(&self) -> bool {
        self.call(DaemonOperation::Status).is_ok()
    }
}

/// Connect to the shared daemon for `root`, starting a transient one when absent. The returned
/// lease keeps that daemon alive for the caller and is released automatically after a crash or
/// normal drop.
pub fn ensure_transient(root: &Path) -> Result<(DaemonClient, DaemonClientLease), DaemonCallError> {
    let client = DaemonClient::for_root(root).map_err(DaemonCallError::Transport)?;
    if let Ok(lease) = client.acquire_lease() {
        return Ok((client, lease));
    }
    let executable = std::env::current_exe().map_err(DaemonCallError::Transport)?;
    let mut child = std::process::Command::new(executable)
        .arg("--root")
        .arg(root)
        .arg("daemon-serve")
        .arg("--transient")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(DaemonCallError::Transport)?;
    let deadline = std::time::Instant::now() + DAEMON_READY_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(lease) = client.acquire_lease() {
            drop(child.stdin.take());
            return Ok((client, lease));
        }
        if child
            .try_wait()
            .map_err(DaemonCallError::Transport)?
            .is_some()
        {
            // Another process may have won singleton startup; keep polling its endpoint.
        }
        std::thread::sleep(DAEMON_READY_POLL);
    }
    Err(DaemonCallError::Transport(io::Error::new(
        io::ErrorKind::TimedOut,
        "transient daemon did not become ready",
    )))
}

#[derive(Debug)]
pub struct DaemonClientLease {
    _stream: interprocess::local_socket::Stream,
}

#[derive(Debug, Error)]
pub enum DaemonCallError {
    #[error("daemon transport: {0}")]
    Transport(#[source] io::Error),
    #[error("daemon operation: {0}")]
    Remote(String),
}

fn invalid_protocol(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Serve the primary daemon protocol until an authenticated local client requests shutdown.
/// The workspace engine remains the source of truth; this transport owns no persistence.
pub fn serve(root: &Path, transient: bool) -> anyhow::Result<()> {
    use interprocess::local_socket::{ListenerOptions, prelude::*};

    let root = root.canonicalize()?;
    let identity = RootIdentity::discover(&root)?;
    let layout = RuntimeLayout::for_root(&identity)?;
    let Some(_lease) = DaemonLease::try_acquire(&layout.singleton_lock)? else {
        anyhow::bail!("a Ravel daemon already owns this workspace");
    };
    let name = match &layout.endpoint {
        #[cfg(unix)]
        LocalEndpoint::Unix(path) => path
            .clone()
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()?,
        #[cfg(windows)]
        LocalEndpoint::WindowsPipe(name) => {
            name.clone()
                .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?
        }
    };
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    let engine = Arc::new(crate::engine::WorkspaceEngine::load(
        &root,
        &Default::default(),
    )?);
    let state = Arc::new(DaemonState {
        persistent: AtomicBool::new(!transient),
        shutdown: AtomicBool::new(false),
        leases: AtomicUsize::new(0),
        inflight_requests: AtomicUsize::new(0),
        active_connections: AtomicUsize::new(0),
        max_connections: max_connections(),
        max_leases: max_leases(),
        request_timeout: request_timeout(),
        wake_endpoint: layout.endpoint.clone(),
    });
    if transient {
        spawn_bootstrap_monitor(state.clone());
    }
    spawn_daemon_watcher(root, engine.clone(), state.clone());
    loop {
        if state.shutdown.load(Ordering::Acquire)
            && state.leases.load(Ordering::Acquire) == 0
            && state.inflight_requests.load(Ordering::Acquire) == 0
            && state.active_connections.load(Ordering::Acquire) == 0
        {
            break;
        }
        let stream = listener.accept()?;
        if state.shutdown.load(Ordering::Acquire)
            && state.leases.load(Ordering::Acquire) == 0
            && state.inflight_requests.load(Ordering::Acquire) == 0
            && state.active_connections.load(Ordering::Acquire) == 0
        {
            break;
        }
        let identity = identity.clone();
        let engine = engine.clone();
        let state = state.clone();
        // This is a hard process-wide resource bound. Leases do not expand it.
        if !try_reserve(&state.active_connections, state.max_connections) {
            drop(stream);
            continue;
        }
        let reservation_state = state.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("ravel-daemon-client".into())
            .spawn(move || {
                let _connection = ConnectionGuard(&state);
                let mut stream = stream;
                set_request_read_timeout(&stream, Some(state.request_timeout));
                let _ = handle_connection(&mut stream, &identity, &engine, &state);
            })
        {
            // The closure never ran, so its connection guard cannot release the reservation.
            reservation_state
                .active_connections
                .fetch_sub(1, Ordering::AcqRel);
            return Err(error.into());
        }
    }
    Ok(())
}

fn spawn_bootstrap_monitor(state: Arc<DaemonState>) {
    let _ = std::thread::Builder::new()
        .name("ravel-daemon-bootstrap".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut byte = [0_u8; 1];
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            if state.leases.load(Ordering::Acquire) == 0
                && !state.persistent.load(Ordering::Acquire)
            {
                state.shutdown.store(true, Ordering::Release);
                wake_if_drained(&state);
            }
        });
}

struct DaemonState {
    persistent: AtomicBool,
    shutdown: AtomicBool,
    leases: AtomicUsize,
    inflight_requests: AtomicUsize,
    active_connections: AtomicUsize,
    max_connections: usize,
    max_leases: usize,
    request_timeout: Duration,
    wake_endpoint: LocalEndpoint,
}

struct ConnectionGuard<'a>(&'a DaemonState);

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::AcqRel);
        wake_if_drained(self.0);
    }
}

fn try_reserve(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .is_ok()
}

struct RequestGuard<'a>(&'a DaemonState);

impl<'a> RequestGuard<'a> {
    fn new(state: &'a DaemonState) -> Self {
        state.inflight_requests.fetch_add(1, Ordering::AcqRel);
        Self(state)
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight_requests.fetch_sub(1, Ordering::AcqRel);
        wake_if_drained(self.0);
    }
}

fn wake_listener(endpoint: &LocalEndpoint) {
    use interprocess::local_socket::prelude::*;
    match endpoint {
        #[cfg(unix)]
        LocalEndpoint::Unix(path) => {
            if let Ok(name) = path
                .clone()
                .to_fs_name::<interprocess::local_socket::GenericFilePath>()
            {
                let _ = LocalSocketStream::connect(name);
            }
        }
        #[cfg(windows)]
        LocalEndpoint::WindowsPipe(name) => {
            if let Ok(name) = name
                .clone()
                .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
            {
                let _ = LocalSocketStream::connect(name);
            }
        }
    }
}

fn wake_if_drained(state: &DaemonState) {
    if state.shutdown.load(Ordering::Acquire)
        && state.leases.load(Ordering::Acquire) == 0
        && state.inflight_requests.load(Ordering::Acquire) == 0
        && state.active_connections.load(Ordering::Acquire) == 0
    {
        wake_listener(&state.wake_endpoint);
    }
}

fn spawn_daemon_watcher(
    root: PathBuf,
    engine: Arc<crate::engine::WorkspaceEngine>,
    state: Arc<DaemonState>,
) {
    if !root.is_dir() || engine.config.sync.mode == "none" {
        return;
    }
    let debounce = Duration::from_millis(engine.config.watch.debounce_ms);
    let max_batch = Duration::from_millis(engine.config.watch.max_batch_ms);
    let max_batch_paths = engine.config.watch.max_batch_paths;
    let queue_capacity = engine.config.watch.queue_capacity;
    let watch_config = engine.config.clone();
    let storage_root = root.join(&engine.config.storage.home);
    let _ = std::thread::Builder::new()
        .name("ravel-daemon-watch".into())
        .spawn(move || {
            let _watch_leader = loop {
                if state.shutdown.load(Ordering::Acquire) {
                    return;
                }
                match crate::watch::try_acquire_leadership(&root, &engine.config.storage.home) {
                    Ok(Some(leader)) => break leader,
                    Ok(None) => std::thread::sleep(WATCH_STOP_POLL_INTERVAL),
                    Err(error) => {
                        engine.record_update_error("daemon watch leader", &error.to_string());
                        return;
                    }
                }
            };
            let watcher = match crate::watch::PersistentWatcher::new_filtered(
                &root,
                queue_capacity,
                move |path| !path.starts_with(&storage_root) && !watch_config.is_noise(path),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    engine.record_update_error("daemon watch", &error.to_string());
                    return;
                }
            };
            let extensions = crate::config::effective_extensions(&engine.config);
            while !state.shutdown.load(Ordering::Acquire) {
                let batch = match watcher.next_batch(
                    debounce,
                    WATCH_STOP_POLL_INTERVAL,
                    max_batch_paths,
                    max_batch,
                ) {
                    Ok(batch) => batch,
                    Err(crate::watch::WatchError::Timeout) => continue,
                    Err(crate::watch::WatchError::Closed) => return,
                    Err(error) => {
                        engine.record_update_error("daemon watch", &error.to_string());
                        return;
                    }
                };
                let paths: Vec<_> = batch
                    .paths
                    .into_iter()
                    .filter(|path| {
                        engine.config.is_source_with_extensions(path, &extensions)
                            && !engine.config.is_noise(path)
                    })
                    .collect();
                if !batch.needs_reconcile && paths.is_empty() {
                    continue;
                }
                let _request = RequestGuard::new(&state);
                let result = if batch.needs_reconcile {
                    engine.index()
                } else {
                    engine.sync(Some(&paths))
                };
                if let Err(error) = result {
                    engine.record_update_error("daemon watch update", &error.to_string());
                }
            }
        });
}

fn handle_connection(
    stream: &mut interprocess::local_socket::Stream,
    identity: &RootIdentity,
    engine: &crate::engine::WorkspaceEngine,
    state: &DaemonState,
) -> io::Result<bool> {
    let hello = match read_frame::<WireRequest>(stream)? {
        WireRequest::Hello(hello) => hello,
        WireRequest::Operation(_) => {
            write_frame(stream, &WireResponse::Error("handshake required".into()))?;
            return Ok(false);
        }
    };
    let server = ServerHello {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        server_version: crate::VERSION.to_owned(),
        root: identity.clone(),
    };
    if let Err(error) = validate_handshake(&hello, &server) {
        write_frame(stream, &WireResponse::Error(error.to_string()))?;
        return Ok(false);
    }
    write_frame(stream, &WireResponse::Hello(server))?;
    let operation = match read_frame::<WireRequest>(stream)? {
        WireRequest::Operation(operation) => operation,
        WireRequest::Hello(_) => {
            write_frame(stream, &WireResponse::Error("duplicate handshake".into()))?;
            return Ok(false);
        }
    };
    if matches!(operation, DaemonOperation::Lease) {
        let lease = match LeaseGuard::try_new(state) {
            Some(lease) => lease,
            None => {
                write_frame(
                    stream,
                    &WireResponse::Error("daemon lease limit reached".into()),
                )?;
                return Ok(false);
            }
        };
        write_frame(
            stream,
            &WireResponse::Value(serde_json::json!({ "leased": true })),
        )?;
        // An established lease intentionally lives until its peer disconnects.
        set_request_read_timeout(stream, None);
        let mut byte = [0_u8; 1];
        let read_result = loop {
            match stream.read(&mut byte) {
                Ok(0) => break Ok(()),
                Ok(_) => continue,
                Err(error) => break Err(error),
            }
        };
        drop(lease);
        read_result?;
        return Ok(false);
    }
    if state.shutdown.load(Ordering::Acquire) && !matches!(operation, DaemonOperation::Shutdown) {
        write_frame(
            stream,
            &WireResponse::Error("daemon is shutting down".into()),
        )?;
        return Ok(false);
    }
    let shutdown = matches!(operation, DaemonOperation::Shutdown);
    let request_guard = RequestGuard::new(state);
    let response: Result<Value, String> = match operation {
        DaemonOperation::Status => engine.status().map_err(|error| error.to_string()),
        DaemonOperation::Context { query, limit } => engine
            .context(&query, limit)
            .map_err(|error| error.to_string()),
        DaemonOperation::Sync { paths } => engine
            .sync((!paths.is_empty()).then_some(paths.as_slice()))
            .map_err(|error| error.to_string())
            .and_then(|stats| serde_json::to_value(stats).map_err(|error| error.to_string())),
        DaemonOperation::Lease => unreachable!(),
        DaemonOperation::PromotePersistent => {
            state.persistent.store(true, Ordering::Release);
            Ok(serde_json::json!({ "persistent": true }))
        }
        DaemonOperation::Shutdown => Ok(serde_json::json!({ "shutdown": true })),
    };
    match response {
        Ok(value) => write_frame(stream, &WireResponse::Value(value))?,
        Err(error) => write_frame(stream, &WireResponse::Error(error.to_string()))?,
    }
    if shutdown {
        state.shutdown.store(true, Ordering::Release);
        drop(request_guard);
        wake_if_drained(state);
    }
    Ok(shutdown)
}

struct LeaseGuard<'a>(&'a DaemonState);

impl<'a> LeaseGuard<'a> {
    fn try_new(state: &'a DaemonState) -> Option<Self> {
        try_reserve(&state.leases, state.max_leases).then(|| Self(state))
    }
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        if self.0.leases.fetch_sub(1, Ordering::AcqRel) == 1
            && !self.0.persistent.load(Ordering::Acquire)
        {
            self.0.shutdown.store(true, Ordering::Release);
        }
        wake_if_drained(self.0);
    }
}

#[cfg(unix)]
fn set_request_read_timeout(
    stream: &interprocess::local_socket::Stream,
    timeout: Option<Duration>,
) {
    use interprocess::local_socket::traits::Stream;
    let _ = stream.set_recv_timeout(timeout);
}

#[cfg(not(unix))]
fn set_request_read_timeout(
    _stream: &interprocess::local_socket::Stream,
    _timeout: Option<Duration>,
) {
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC frame too large",
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "IPC frame too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&payload)
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    read_frame_with_limit(reader, MAX_FRAME_BYTES)
}

pub fn read_frame_with_limit<T: DeserializeOwned>(
    reader: &mut impl Read,
    max_bytes: usize,
) -> io::Result<T> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC frame too large",
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn root_identity_is_stable_and_distinguishes_worktrees() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        assert_eq!(
            RootIdentity::discover(first.path()).unwrap(),
            RootIdentity::discover(first.path()).unwrap()
        );
        assert_ne!(
            RootIdentity::discover(first.path()).unwrap(),
            RootIdentity::discover(second.path()).unwrap()
        );
    }

    #[test]
    fn singleton_lease_is_recoverable_after_drop() {
        let dir = tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        let first = DaemonLease::try_acquire(&lock).unwrap().unwrap();
        assert!(DaemonLease::try_acquire(&lock).unwrap().is_none());
        drop(first);
        assert!(DaemonLease::try_acquire(&lock).unwrap().is_some());
    }

    #[test]
    fn runtime_layout_is_stable_per_root() {
        let runtime = tempdir().unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let first = RootIdentity::discover(first.path()).unwrap();
        let second = RootIdentity::discover(second.path()).unwrap();
        let layout = RuntimeLayout::in_directory(runtime.path().into(), &first).unwrap();
        assert_eq!(
            layout,
            RuntimeLayout::in_directory(runtime.path().into(), &first).unwrap()
        );
        assert_ne!(
            layout.endpoint,
            RuntimeLayout::in_directory(runtime.path().into(), &second)
                .unwrap()
                .endpoint
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_layout_uses_a_safe_short_socket_for_long_runtime_base() {
        use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

        let runtime = tempdir().unwrap();
        let long_base = runtime.path().join("x".repeat(180));
        std::fs::create_dir_all(&long_base).unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let first = RootIdentity::discover(first.path()).unwrap();
        let second = RootIdentity::discover(second.path()).unwrap();

        let layout = RuntimeLayout::in_directory(long_base.clone(), &first).unwrap();
        let same = RuntimeLayout::in_directory(long_base.clone(), &first).unwrap();
        let other = RuntimeLayout::in_directory(long_base, &second).unwrap();
        let LocalEndpoint::Unix(endpoint) = &layout.endpoint;
        assert!(endpoint.as_os_str().as_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES);
        assert_eq!(layout, same);
        assert_ne!(layout.endpoint, other.endpoint);
        assert_eq!(
            std::fs::metadata(&layout.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn framed_handshake_round_trips_and_checks_root() {
        let root_dir = tempdir().unwrap();
        let other_dir = tempdir().unwrap();
        let root = RootIdentity::discover(root_dir.path()).unwrap();
        let hello = ClientHello::current(root.clone());
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &hello).unwrap();
        assert_eq!(
            read_frame::<ClientHello>(&mut bytes.as_slice()).unwrap(),
            hello
        );

        let server = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_version: crate::VERSION.to_owned(),
            root,
        };
        validate_handshake(&hello, &server).unwrap();
        let wrong = ServerHello {
            root: RootIdentity::discover(other_dir.path()).unwrap(),
            ..server
        };
        assert_eq!(
            validate_handshake(&hello, &wrong),
            Err(HandshakeError::RootMismatch)
        );
    }

    #[test]
    fn frame_limit_is_enforced_before_allocation() {
        let mut bytes = Vec::from((1024_u32).to_le_bytes());
        bytes.extend_from_slice(b"{}");
        let error =
            read_frame_with_limit::<serde_json::Value>(&mut bytes.as_slice(), 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reservations_never_exceed_the_configured_bound() {
        let counter = AtomicUsize::new(0);
        assert!(try_reserve(&counter, 2));
        assert!(try_reserve(&counter, 2));
        assert!(!try_reserve(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }
}
