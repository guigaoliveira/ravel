use blake3::Hash;
use notify::{
    Event, EventKind, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind},
};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CoalescedChange {
    pub paths: Vec<PathBuf>,
    pub needs_reconcile: bool,
    /// Those of `paths` that may stand for a whole directory: one created or removed, or anything
    /// renamed (a rename does not say what moved), unless another event in the batch showed it to
    /// be a file. The backend names such a directory alone, never what is inside it.
    #[serde(default)]
    pub structural: Vec<PathBuf>,
}
#[derive(Debug, Error)]
pub enum WatchError {
    #[error("watcher: {0}")]
    Notify(String),
    #[error("watch timeout")]
    Timeout,
    #[error("watch channel closed")]
    Closed,
}

pub struct PersistentWatcher {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<Event>>,
    overflowed: Arc<AtomicBool>,
    reconcile_pending: AtomicBool,
    gate: Option<Arc<WatchGate>>,
}

impl Drop for PersistentWatcher {
    fn drop(&mut self) {
        // A watcher that is gone reports nothing, which must never read as "nothing changed".
        if let Some(gate) = &self.gate {
            gate.degrade();
        }
    }
}

/// Elect exactly one watcher for a workspace across daemon and fallback MCP processes.
/// The returned file must remain alive for the entire watcher lifetime.
#[cfg(test)]
pub(crate) fn acquire_leadership(root: &Path, storage_home: &Path) -> std::io::Result<File> {
    use fs4::fs_std::FileExt;

    let storage = root.join(storage_home);
    std::fs::create_dir_all(&storage)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(storage.join("watch.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Attempt watcher leadership without waiting for another process to release it.
pub(crate) fn try_acquire_leadership(
    root: &Path,
    storage_home: &Path,
) -> std::io::Result<Option<File>> {
    use fs4::fs_std::FileExt;

    let storage = root.join(storage_home);
    std::fs::create_dir_all(&storage)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(storage.join("watch.lock"))?;
    match file.try_lock_exclusive() {
        Ok(true) => Ok(Some(file)),
        Ok(false) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

impl PersistentWatcher {
    pub fn new(root: &Path, queue_capacity: usize) -> Result<Self, WatchError> {
        Self::new_filtered(root, queue_capacity, |_| true)
    }

    /// Start a recursive watcher while discarding irrelevant paths before they consume bounded
    /// queue capacity. An event with several paths (notably a rename) is retained when at least
    /// one side is relevant; pathless `Other` events are retained so backend rescan signals are
    /// never hidden.
    pub fn new_filtered<F>(
        root: &Path,
        queue_capacity: usize,
        path_is_relevant: F,
    ) -> Result<Self, WatchError>
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        Self::build(root, queue_capacity, path_is_relevant, None)
    }

    /// [`Self::new_filtered`] that also keeps a [`WatchGate`] up to date, so a query can learn
    /// that nothing relevant changed without asking git. `cookie_dir` must lie inside `root`.
    /// `noise_dir` answers, for a directory relative to `root`, whether the relevance filter drops
    /// everything below it (see [`WatchGate::ignore_rules_below`]).
    pub(crate) fn new_gated<F>(
        root: &Path,
        queue_capacity: usize,
        cookie_dir: &Path,
        path_is_relevant: F,
        noise_dir: impl Fn(&Path) -> bool + Send + Sync + 'static,
    ) -> Result<Self, WatchError>
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        let gate = WatchGate::new(root, cookie_dir);
        gate.ignore_rules_below(noise_dir);
        Self::build(root, queue_capacity, path_is_relevant, Some(gate))
    }

    #[cfg(test)]
    pub(crate) fn new_with_gate<F>(
        root: &Path,
        queue_capacity: usize,
        gate: Arc<WatchGate>,
        path_is_relevant: F,
    ) -> Result<Self, WatchError>
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        Self::build(root, queue_capacity, path_is_relevant, Some(gate))
    }

    /// The gate this watcher feeds, if it was built with one.
    pub(crate) fn gate(&self) -> Option<Arc<WatchGate>> {
        self.gate.clone()
    }

    fn build<F>(
        root: &Path,
        queue_capacity: usize,
        path_is_relevant: F,
        gate: Option<Arc<WatchGate>>,
    ) -> Result<Self, WatchError>
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        // A bounded queue prevents an editor/event storm from growing the process without limit.
        // Overflow deliberately degrades to a full reconcile on the next batch.
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = overflowed.clone();
        // Subscribe to changes only. The library default (`EventKindMask::ALL`) also asks the
        // kernel for every open and read-only close under the tree, which `filter_event` then
        // throws away -- after this thread has woken up and read them. The daemon is the busiest
        // reader of its own workspace (each query's `git status`, every file the engine opens), so
        // on the 20k-file corpus that thread took 1.4-2.3 ms of CPU per query, half or more of the
        // daemon's total; a `grep -r` over the same tree cost it 131 ms.
        // A completed write stays: some backends report it only as a close. The gate's markers are
        // created files, which `CORE` keeps.
        let config = notify::Config::default()
            .with_event_kinds(notify::EventKindMask::CORE | notify::EventKindMask::ACCESS_CLOSE);
        let callback_gate = gate.clone();
        let mut watcher = notify::RecommendedWatcher::new(
            move |result| {
                if let Some(gate) = &callback_gate
                    && gate.absorb(&result)
                {
                    // One of the gate's own markers, not a change to the tree.
                    return;
                }
                let result = match result {
                    Ok(event) => {
                        let Some(event) = filter_event(event, &path_is_relevant) else {
                            return;
                        };
                        if let Some(gate) = &callback_gate {
                            gate.count(&event);
                        }
                        Ok(event)
                    }
                    Err(error) => {
                        if let Some(gate) = &callback_gate {
                            gate.degrade();
                        }
                        Err(error)
                    }
                };
                if sender.try_send(result).is_err() {
                    callback_overflowed.store(true, Ordering::Release);
                }
            },
            config,
        )
        .map_err(|error| WatchError::Notify(error.to_string()))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| WatchError::Notify(error.to_string()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            overflowed,
            reconcile_pending: AtomicBool::new(false),
            gate,
        })
    }

    pub fn next_batch(
        &self,
        debounce: Duration,
        timeout: Duration,
        max_paths: usize,
        max_batch: Duration,
    ) -> Result<CoalescedChange, WatchError> {
        // The quiet period has to fit inside a batch, with room to spare: one that does not is
        // never observed, so a pending reconcile is put off forever and every later event is
        // drained unread. Capping it at the batch itself is not enough, because the batch is
        // already shorter than that by the time the first wait starts.
        let debounce = if debounce >= max_batch {
            max_batch / 2
        } else {
            debounce
        };
        let mut paths = BTreeSet::new();
        let mut entries = EntryKinds::default();
        let mut needs_reconcile = self.reconcile_pending.swap(false, Ordering::AcqRel);
        if !needs_reconcile {
            let first = self
                .receiver
                .recv_timeout(timeout)
                .map_err(|error| match error {
                    mpsc::RecvTimeoutError::Timeout => WatchError::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => WatchError::Closed,
                })?
                .map_err(|error| WatchError::Notify(error.to_string()))?;
            entries.note(&first);
            accumulate_event(first, &mut paths, &mut needs_reconcile, max_paths);
        }
        let started = std::time::Instant::now();
        let mut became_quiet = false;
        // Debounce is a quiet-period policy, not a fixed window from the first event. A fixed
        // window splits a sustained editor storm into many batches and can consequently launch
        // many full reconciliations. Resetting the wait after every event produces one batch per
        // burst. Once the bounded producer queue overflows, paths are no longer authoritative;
        // drain/coalesce the whole burst and reconcile exactly once after it becomes quiet.
        loop {
            if self.overflowed.load(Ordering::Acquire) {
                needs_reconcile = true;
                paths.clear();
            }
            if !needs_reconcile && paths.len() >= max_paths {
                break;
            }
            let remaining = max_batch.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            let wait = debounce.min(remaining);
            match self.receiver.recv_timeout(wait) {
                Ok(Ok(event)) => {
                    if !needs_reconcile {
                        entries.note(&event);
                        accumulate_event(event, &mut paths, &mut needs_reconcile, max_paths);
                    }
                }
                Ok(Err(error)) => return Err(WatchError::Notify(error.to_string())),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    became_quiet = debounce <= remaining;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(WatchError::Closed),
            }
        }
        needs_reconcile |= self.overflowed.swap(false, Ordering::AcqRel);
        if needs_reconcile {
            paths.clear();
            if !became_quiet {
                // Preserve the lost-event signal across bounded polling slices. This lets the
                // owner observe shutdown while a storm continues, but delays the single full
                // reconciliation until the stream has actually become quiet.
                self.reconcile_pending.store(true, Ordering::Release);
                needs_reconcile = false;
            }
        }
        let structural = entries.structural(&paths);
        Ok(CoalescedChange {
            paths: paths.into_iter().collect(),
            needs_reconcile,
            structural,
        })
    }
}

/// What a batch's events said about the kind of entry behind each path.
#[derive(Default)]
struct EntryKinds {
    /// Named by an event that may concern a directory.
    maybe_directories: BTreeSet<PathBuf>,
    /// Named by an event only ever reported for a file.
    files: BTreeSet<PathBuf>,
}

impl EntryKinds {
    fn note(&mut self, event: &Event) {
        let kinds = match event.kind {
            // A directory or -- where the backend does not say (`Any`, `Other`) -- maybe one. A
            // rename cannot say what moved.
            EventKind::Create(CreateKind::Folder | CreateKind::Any | CreateKind::Other)
            | EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other)
            | EventKind::Modify(ModifyKind::Name(_)) => &mut self.maybe_directories,
            // An editor's save renames the file, or a backup of it, and then writes or removes the
            // file it renamed: those events settle that no directory moved.
            EventKind::Create(CreateKind::File)
            | EventKind::Remove(RemoveKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Access(AccessKind::Close(AccessMode::Write)) => &mut self.files,
            _ => return,
        };
        kinds.extend(event.paths.iter().cloned());
    }

    /// Those of the batch's paths that may still be directories.
    fn structural(self, paths: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
        let files = self.files;
        self.maybe_directories
            .into_iter()
            .filter(|path| paths.contains(path) && !files.contains(path))
            .collect()
    }
}

/// The source files a batch changed without naming any of them, which an exact sync of its paths
/// would never see: what is inside a directory created, moved or renamed into place -- the backend
/// reports the directory alone, and a file written into a new directory before its watch exists is
/// not reported at all -- and every indexed file under one that moved away.
///
/// `None` when they are more than `max_paths`, or the index cannot be read; the caller reconciles
/// instead. A file renamed or saved through a temporary costs nothing here: only what may be a
/// directory is looked at, and the index only for one that is gone.
pub fn sources_behind_directories(
    engine: &crate::engine::WorkspaceEngine,
    ignore: &crate::config::IgnoreChain,
    extensions: &[String],
    batch: &CoalescedChange,
    max_paths: usize,
) -> Option<Vec<PathBuf>> {
    let mut found = BTreeSet::new();
    let mut gone = Vec::new();
    for path in &batch.structural {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                let config = &engine.config;
                if !sources_under(path, config, ignore, extensions, max_paths, &mut found) {
                    return None;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Ok(relative) = path.strip_prefix(&engine.root) {
                    let relative = relative.to_string_lossy().replace('\\', "/");
                    gone.push(format!("{relative}/"));
                }
            }
            Err(_) => return None,
        }
    }
    if !gone.is_empty()
        && let Some(indexed) = engine.storage().open_file_list().ok()?
    {
        for relative in indexed.paths {
            if gone
                .iter()
                .any(|directory| relative.starts_with(directory.as_str()))
            {
                found.insert(engine.root.join(relative));
                if found.len() > max_paths {
                    return None;
                }
            }
        }
    }
    found.retain(|path| batch.paths.binary_search(path).is_err());
    Some(found.into_iter().collect())
}

/// Add the indexable files under `directory` to `found` as the full index walk would see them:
/// noise and ignored trees skipped, links not followed. False once there are more than `max_paths`.
fn sources_under(
    directory: &Path,
    config: &crate::config::Config,
    ignore: &crate::config::IgnoreChain,
    extensions: &[String],
    max_paths: usize,
    found: &mut BTreeSet<PathBuf>,
) -> bool {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if !config.is_noise(&path) && !ignore.is_ignored(&path) {
                    pending.push(path);
                }
            } else if kind.is_file()
                && crate::config::watched_path_is_indexable(config, ignore, extensions, &path)
            {
                found.insert(path);
                if found.len() > max_paths {
                    return false;
                }
            }
        }
    }
    true
}

/// Name prefix of the marker files the gate creates inside the storage directory.
const COOKIE_PREFIX: &str = ".watch-cookie-";
/// How long a marker may take to come back through the backend before this query stops waiting.
const BARRIER_TIMEOUT: Duration = Duration::from_millis(100);
/// A backend that fails to return its markers this many times in a row is not delivering.
const MAX_BARRIER_FAILURES: u32 = 3;
/// How long the gate then stands aside before it asks again, so that a stalled or merely starved
/// backend costs one short wait in a while instead of one per query.
const BARRIER_PAUSE: Duration = Duration::from_secs(30);
/// A tree the watcher has called quiet is still checked against git this often, so that whatever
/// the watcher cannot see has a bounded life instead of an unbounded one.
const QUIET_REVERIFY: Duration = Duration::from_secs(60);
/// Recursive backends install the watch for a new directory a moment *after* announcing it, and
/// anything created in it before then goes unreported. Nothing is called quiet until that moment
/// has certainly passed.
const STRUCTURE_SETTLE: Duration = Duration::from_secs(2);

/// What the watcher can tell a query about the tree without asking git.
///
/// Every query asks git what changed (~35 ms on 20k files), because the index is only as fresh as
/// the last thing that told it about an edit. A daemon already has a recursive watcher, which
/// hears about every edit; the question is whether it has heard about *all of them yet*. The
/// answer is a marker: before looking, a query creates a file inside the storage directory and
/// waits for the backend to report it. Events are queued in the order the operations completed
/// and delivered in that order, so once the marker has come back every edit that finished before
/// the query began has already been counted -- the watcher is demonstrably caught up, not merely
/// quiet. If the count has not moved since a full check last found the index consistent with the
/// tree, the tree is exactly as that check left it and the check can be skipped.
///
/// Anything that casts doubt on the count makes the gate stand aside, never guess: an error or a
/// lost-event signal from the backend, a marker that does not come back, a filesystem whose
/// changes this process may not hear about (network, FUSE, VM shares), a new directory whose
/// watch may not exist yet, a change to the ignore rules the event filter was built from, a
/// different index generation than the one that was checked, or simply time. In each case the
/// caller runs the full check it always ran.
pub struct WatchGate {
    root: PathBuf,
    cookie_dir: PathBuf,
    /// Whether the filesystem is one where this process hears about every change made to it.
    trusted: bool,
    timing: Timing,
    degraded: AtomicBool,
    /// Set while the backend is not answering its markers.
    paused_until: Mutex<Option<Instant>>,
    /// Events that could change what the tree contains, counted as the backend delivers them.
    events: AtomicU64,
    /// When the last event arrived that adds or moves entries (and so may need a new watch).
    structure: Mutex<Option<Instant>>,
    cookies: Mutex<HashMap<String, Arc<Cookie>>>,
    next_cookie: AtomicU32,
    barrier_failures: AtomicU32,
    mark: Mutex<Option<CleanMark>>,
    /// Which directories, relative to `root`, nothing below is ever relevant in. A rules file
    /// there cannot change what the relevance filter keeps. Unset, every rules file counts.
    noise_dir: std::sync::OnceLock<NoiseDir>,
}

type NoiseDir = Box<dyn Fn(&Path) -> bool + Send + Sync>;

impl std::fmt::Debug for WatchGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatchGate")
            .field("trusted", &self.trusted)
            .field("degraded", &self.degraded.load(Ordering::Relaxed))
            .field("events", &self.events.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Cookie {
    seen: Mutex<bool>,
    arrived: Condvar,
}

/// The clocks the gate runs on. The defaults are the production values; tests shorten or
/// lengthen them.
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    /// How long a marker may take to come back before the query stops waiting for it.
    pub(crate) barrier: Duration,
    /// How long the gate stands aside after [`MAX_BARRIER_FAILURES`] failures in a row.
    pub(crate) pause: Duration,
    /// How long a quiet verdict stands before a full check is due again.
    pub(crate) reverify: Duration,
    /// How long after a directory-creating event nothing is called quiet.
    pub(crate) settle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            barrier: BARRIER_TIMEOUT,
            pause: BARRIER_PAUSE,
            reverify: QUIET_REVERIFY,
            settle: STRUCTURE_SETTLE,
        }
    }
}

/// The state a full check found the tree and index in, and when.
#[derive(Clone)]
struct CleanMark {
    events: u64,
    generation: String,
    at: Instant,
}

/// What one query saw of the watcher before it looked at the tree.
pub(crate) struct Probe {
    events: u64,
    started: Instant,
    /// No directory-creating event is recent enough for its watch to be missing.
    settled: bool,
}

impl Probe {
    /// The moment the watcher was last known to be caught up. A listing of the tree is only as
    /// good as the probe if git was asked after it.
    pub(crate) fn started(&self) -> Instant {
        self.started
    }
}

impl WatchGate {
    pub fn new(root: &Path, cookie_dir: &Path) -> Arc<Self> {
        Self::with_timing(root, cookie_dir, Timing::default())
    }

    pub(crate) fn with_timing(root: &Path, cookie_dir: &Path, timing: Timing) -> Arc<Self> {
        // `RAVEL_WATCH_FASTPATH=0` turns the gate off: every query asks git, as it always did.
        let enabled = std::env::var_os("RAVEL_WATCH_FASTPATH").is_none_or(|value| value != "0");
        let trusted = enabled
            && cookie_dir.starts_with(root)
            && filesystem_reports_every_change(root)
            && filesystem_reports_every_change(cookie_dir);
        if trusted {
            // A process that died while a query was waiting left its marker behind.
            for entry in std::fs::read_dir(cookie_dir)
                .into_iter()
                .flatten()
                .flatten()
            {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(COOKIE_PREFIX)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Arc::new(Self {
            root: root.to_path_buf(),
            cookie_dir: cookie_dir.to_path_buf(),
            trusted,
            timing,
            degraded: AtomicBool::new(false),
            paused_until: Mutex::new(None),
            events: AtomicU64::new(0),
            structure: Mutex::new(None),
            cookies: Mutex::new(HashMap::new()),
            next_cookie: AtomicU32::new(0),
            barrier_failures: AtomicU32::new(0),
            mark: Mutex::new(None),
            noise_dir: std::sync::OnceLock::new(),
        })
    }

    /// Tell the gate which directories (relative to the root) the relevance filter drops whole.
    /// `npm install` writes packages that ship their own `.gitignore` under `node_modules`; each
    /// of those used to turn the gate off for the life of the process, though no rule below a
    /// directory the filter drops can make a file relevant again.
    pub(crate) fn ignore_rules_below(
        &self,
        noise_dir: impl Fn(&Path) -> bool + Send + Sync + 'static,
    ) {
        let _ = self.noise_dir.set(Box::new(noise_dir));
    }

    /// Whether a change to this rules file can alter what the relevance filter keeps. Only a
    /// `.gitignore`/`.ravelignore` provably inside a dropped directory cannot; `.git/info/exclude`
    /// lives under `.git` and always can, and a path the root does not prefix is not vouched for.
    fn rules_can_apply(&self, path: &Path, name: &str) -> bool {
        if name == "exclude" {
            return true;
        }
        let Some(noise_dir) = self.noise_dir.get() else {
            return true;
        };
        !path
            .parent()
            .and_then(|directory| directory.strip_prefix(&self.root).ok())
            .is_some_and(noise_dir)
    }

    /// Whether the gate can ever vouch for this tree (right filesystem, not switched off).
    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// Stop vouching for the tree, for the rest of this process.
    pub(crate) fn degrade(&self) {
        self.degraded.store(true, Ordering::Release);
        *lock(&self.mark) = None;
    }

    fn usable(&self) -> bool {
        self.trusted
            && !self.degraded.load(Ordering::Acquire)
            && lock(&self.paused_until).is_none_or(|until| Instant::now() >= until)
    }

    /// Called by the backend callback for every raw event, before the relevance filter. True when
    /// the event was one of this gate's markers (which are not changes and must not reach it).
    fn absorb(&self, result: &notify::Result<Event>) -> bool {
        let Ok(event) = result else { return false };
        let mut ours = false;
        for path in &event.paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // Only the markers in the gate's own directory: a workspace file that merely shares the
            // prefix is a change like any other, and swallowing it would let a quiet verdict hide it.
            if name.starts_with(COOKIE_PREFIX) && path.parent() == Some(self.cookie_dir.as_path()) {
                ours = true;
                if let Some(cookie) = lock(&self.cookies).get(name) {
                    *lock(&cookie.seen) = true;
                    cookie.arrived.notify_all();
                }
            } else if is_ignore_rules_file(path, name)
                && !is_read_only_access(&event.kind)
                && self.rules_can_apply(path, name)
            {
                // The relevance filter keeps the ignore rules it first read for as long as the
                // watcher lives, so after one of them changes it may be dropping events for files
                // that are now listed. Only a full check can speak for the tree from here on.
                self.degrade();
            }
        }
        ours
    }

    /// Called for every event the relevance filter kept.
    fn count(&self, event: &Event) {
        // Counted before the event can be queued anywhere that might drop it.
        self.events.fetch_add(1, Ordering::AcqRel);
        // A file made in a directory the backend already watches (that is how the event arrived)
        // has no watch to wait for. A directory, a move -- which cannot say what moved -- or a
        // rescan might.
        if matches!(
            event.kind,
            EventKind::Create(CreateKind::Folder | CreateKind::Any | CreateKind::Other)
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Other
        ) {
            *lock(&self.structure) = Some(Instant::now());
        }
    }

    /// Make the backend prove it is caught up: create a marker and wait for it to come back.
    fn barrier(&self) -> bool {
        let name = format!(
            "{COOKIE_PREFIX}{}-{}",
            std::process::id(),
            self.next_cookie.fetch_add(1, Ordering::Relaxed)
        );
        let cookie = Arc::new(Cookie::default());
        lock(&self.cookies).insert(name.clone(), Arc::clone(&cookie));
        let path = self.cookie_dir.join(&name);
        let arrived = File::create(&path).is_ok() && {
            let seen = lock(&cookie.seen);
            let (seen, _) = cookie
                .arrived
                .wait_timeout_while(seen, self.timing.barrier, |seen| !*seen)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *seen
        };
        let _ = std::fs::remove_file(&path);
        lock(&self.cookies).remove(&name);
        if arrived {
            self.barrier_failures.store(0, Ordering::Release);
        } else {
            crate::timing::note("watch.barrier_miss", String::new);
            // A backend that has just failed to deliver has not earned the trust a mark records:
            // the next verdict has to come from a full check.
            *lock(&self.mark) = None;
            if self.barrier_failures.fetch_add(1, Ordering::AcqRel) + 1 >= MAX_BARRIER_FAILURES {
                self.barrier_failures.store(0, Ordering::Release);
                *lock(&self.paused_until) = Some(Instant::now() + self.timing.pause);
            }
        }
        arrived
    }

    /// Catch the watcher up and read its count. `None` when the gate cannot vouch for anything
    /// right now, in which case the caller simply runs its full check.
    pub(crate) fn probe(&self) -> Option<Probe> {
        if !self.usable() || !self.barrier() {
            return None;
        }
        let settled = lock(&self.structure).is_none_or(|at| at.elapsed() >= self.timing.settle);
        Some(Probe {
            events: self.events.load(Ordering::Acquire),
            started: Instant::now(),
            settled,
        })
    }

    /// True when a full check already found the index consistent with the tree and nothing has
    /// been reported since.
    pub(crate) fn is_quiet(&self, probe: &Probe, generation: &str) -> bool {
        if !probe.settled || !self.usable() {
            return false;
        }
        lock(&self.mark).as_ref().is_some_and(|mark| {
            mark.events == probe.events
                && mark.generation == generation
                && probe.started.duration_since(mark.at) < self.timing.reverify
        })
    }

    /// Record that a full check begun at `probe` found the index consistent with the tree.
    /// Ignored when anything was reported since the probe, or when a new directory may still be
    /// waiting for its watch.
    pub(crate) fn mark_clean(&self, probe: &Probe, generation: String) {
        if !probe.settled || !self.usable() {
            return;
        }
        let mut mark = lock(&self.mark);
        if self.events.load(Ordering::Acquire) != probe.events {
            return;
        }
        *mark = Some(CleanMark {
            events: probe.events,
            generation,
            at: probe.started,
        });
    }
}

/// The files whose contents decide which paths are ignored. Asked about every event the backend
/// reports, so the name settles nearly all of them before the path is looked at.
/// Whether `batch` touched a file that decides what is ignored. A watcher reads those rules once
/// and caches them, so after such a batch it must [`IgnoreChain::forget_rules`] and index again:
/// files the new rules admit were never reported, and those they exclude are still indexed.
///
/// [`IgnoreChain::forget_rules`]: crate::config::IgnoreChain::forget_rules
pub fn changes_ignore_rules(batch: &CoalescedChange) -> bool {
    batch.paths.iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| is_ignore_rules_file(path, name))
    })
}

fn is_ignore_rules_file(path: &Path, name: &str) -> bool {
    match name {
        ".gitignore" | ".ravelignore" => true,
        "exclude" => path.ends_with(".git/info/exclude"),
        _ => false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether changes made to `path` -- by anyone, from anywhere -- reach this process's watcher.
///
/// True of a local disk. False where the kernel cannot know: network mounts and FUSE or VM file
/// shares carry edits made on another machine or by the host that no inotify watch ever hears
/// about, and a quiet watcher there proves nothing. Only Linux is answered; elsewhere nothing is
/// vouched for.
fn filesystem_reports_every_change(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        mount_type_of(&mounts, &canonical).is_some_and(|kind| LOCAL_FILESYSTEMS.contains(&kind))
            && mounts_below_are_local(&mounts, &canonical)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

#[cfg(any(target_os = "linux", test))]
const LOCAL_FILESYSTEMS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "tmpfs", "overlay", "zfs", "f2fs", "bcachefs",
];

/// Whether every mount inside `path` is a local filesystem too: a network share mounted in the
/// middle of the tree is as invisible to the watcher as one that holds the whole of it.
#[cfg(any(target_os = "linux", test))]
fn mounts_below_are_local(mountinfo: &str, path: &Path) -> bool {
    mountinfo.lines().all(|line| {
        let mut fields = line.split(' ');
        let Some(mount_point) = fields.nth(4).map(unescape_mount_field) else {
            return true;
        };
        let mount_point = Path::new(&mount_point);
        if mount_point == path || !mount_point.starts_with(path) {
            return true;
        }
        fields
            .skip_while(|field| *field != "-")
            .nth(1)
            .is_some_and(|kind| LOCAL_FILESYSTEMS.contains(&kind))
    })
}

/// The filesystem type of the mount holding `path`, from `/proc/self/mountinfo` text: the
/// deepest mount point that is a parent of the path.
#[cfg(any(target_os = "linux", test))]
fn mount_type_of<'a>(mountinfo: &'a str, path: &Path) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        let Some(mount_point) = fields.nth(4).map(unescape_mount_field) else {
            continue;
        };
        // Skip the optional fields up to the lone `-`, then the type follows.
        let Some(kind) = fields.skip_while(|field| *field != "-").nth(1) else {
            continue;
        };
        let mount_point = Path::new(&mount_point);
        if path.starts_with(mount_point) {
            let depth = mount_point.components().count();
            if best.is_none_or(|(deepest, _)| depth >= deepest) {
                best = Some((depth, kind));
            }
        }
    }
    best.map(|(_, kind)| kind)
}

/// `/proc/self/mountinfo` writes space, tab, newline and backslash as `\040` style octal escapes.
#[cfg(any(target_os = "linux", test))]
fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 4 <= bytes.len()
            && bytes[index + 1..index + 4].iter().all(u8::is_ascii_digit)
        {
            let value = u32::from(bytes[index + 1] - b'0') * 64
                + u32::from(bytes[index + 2] - b'0') * 8
                + u32::from(bytes[index + 3] - b'0');
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reading a file is not a change to it. inotify reports opens, reads and read-closes, and indexing
/// a path necessarily reads it -- so treating those as changes makes the watcher feed itself: sync
/// reads the file, the read raises an event, the event schedules another sync. One edit produced
/// dozens of identical republications per second until the process was killed.
///
/// `Close(Write)` stays: some backends report a completed write only that way, and dropping it would
/// lose real edits. Anything not obviously read-only is kept, since a missed change is worse than an
/// extra pass.
fn is_read_only_access(kind: &EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    matches!(
        kind,
        // An open is never itself a change, whatever the mode -- a write announces itself through
        // `Modify` and `Close(Write)`. Linux reports plain reads as `Open(Any)`, so matching only
        // the `Read` mode misses every one of them.
        EventKind::Access(
            AccessKind::Read
                | AccessKind::Open(_)
                | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
        )
    )
}

fn accumulate_event(
    event: Event,
    paths: &mut BTreeSet<PathBuf>,
    needs_reconcile: &mut bool,
    max_paths: usize,
) {
    if matches!(event.kind, EventKind::Other) {
        *needs_reconcile = true;
        paths.clear();
        return;
    }
    if is_read_only_access(&event.kind) {
        return;
    }
    if !*needs_reconcile {
        for path in event.paths {
            if paths.len() >= max_paths && !paths.contains(&path) {
                // A single backend event exceeded the configured exact-batch bound. Since part
                // of that event cannot be retained exactly, reconciliation is required.
                *needs_reconcile = true;
                paths.clear();
                return;
            }
            paths.insert(path);
        }
    }
}

fn filter_event<F>(mut event: Event, path_is_relevant: &F) -> Option<Event>
where
    F: Fn(&Path) -> bool + ?Sized,
{
    // Dropped in the producer so a read storm never even occupies the bounded queue -- filling it
    // would raise the overflow signal and turn self-inflicted reads into full reconciliations.
    if is_read_only_access(&event.kind) {
        return None;
    }
    event.paths.retain(|path| path_is_relevant(path));
    (!event.paths.is_empty() || matches!(event.kind, EventKind::Other)).then_some(event)
}

pub fn coalesce(events: impl IntoIterator<Item = Event>) -> CoalescedChange {
    let mut paths = BTreeSet::new();
    let mut entries = EntryKinds::default();
    let mut needs_reconcile = false;
    for event in events {
        if matches!(event.kind, EventKind::Other) {
            needs_reconcile = true;
        }
        // Same rule as the queue: a read is not a change. Three places decided this independently
        // before; they now share `is_read_only_access`.
        if is_read_only_access(&event.kind) {
            continue;
        }
        entries.note(&event);
        for path in event.paths {
            paths.insert(path);
        }
    }
    let structural = entries.structural(&paths);
    CoalescedChange {
        paths: paths.into_iter().collect(),
        needs_reconcile,
        structural,
    }
}

pub fn reconcile_hash(path: &Path) -> std::io::Result<Option<Hash>> {
    if !path.is_file() {
        return Ok(None);
    }
    // Stream the file through the hasher instead of reading it fully into memory (unbounded
    // for large files).
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(Some(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::CreateKind;

    /// What the kernel was asked for, not what `filter_event` later drops: a read must never wake
    /// the watcher thread, and a completed write must still reach it.
    #[cfg(target_os = "linux")]
    #[test]
    fn watcher_does_not_subscribe_to_reads() {
        const IN_CLOSE_WRITE: u32 = 0x08;
        const IN_CLOSE_NOWRITE: u32 = 0x10;
        const IN_OPEN: u32 = 0x20;
        let root = tempfile::tempdir().unwrap();
        let watcher = PersistentWatcher::new(root.path(), 16).unwrap();
        let mut masks = Vec::new();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
            let is_inotify = std::fs::read_link(entry.path())
                .is_ok_and(|target| target.to_string_lossy().contains("inotify"));
            if !is_inotify {
                continue;
            }
            let info = std::fs::read_to_string(format!(
                "/proc/self/fdinfo/{}",
                entry.file_name().to_string_lossy()
            ))
            .unwrap_or_default();
            masks.extend(
                info.lines()
                    .filter(|line| line.starts_with("inotify "))
                    .filter_map(|line| line.split("mask:").nth(1)?.split_whitespace().next())
                    .filter_map(|hex| u32::from_str_radix(hex, 16).ok()),
            );
        }
        drop(watcher);
        assert!(
            !masks.is_empty(),
            "no inotify watch found for the workspace"
        );
        for mask in masks {
            assert_eq!(
                mask & (IN_OPEN | IN_CLOSE_NOWRITE),
                0,
                "subscribed to reads: mask {mask:#x}"
            );
            assert_ne!(
                mask & IN_CLOSE_WRITE,
                0,
                "a completed write must still be reported: mask {mask:#x}"
            );
        }
    }

    #[test]
    fn reading_a_file_is_not_a_change_to_it() {
        use notify::event::{AccessKind, AccessMode, ModifyKind};
        let path = PathBuf::from("/ws/src/a.ts");
        // `Open(Any)` is what Linux actually reports for a read, and it is what the first version
        // of this filter missed: the loop survived because the test used modes the kernel never
        // sends. Measured on a two-file repo, one edit produced 42 of these.
        let read_only = [
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
        ];
        for kind in read_only {
            // Indexing a path reads it, so accepting these makes the watcher feed itself: the sync
            // reads the file, the read raises an event, the event schedules another sync.
            let mut paths = BTreeSet::new();
            let mut needs_reconcile = false;
            accumulate_event(
                Event {
                    kind,
                    paths: vec![path.clone()],
                    attrs: Default::default(),
                },
                &mut paths,
                &mut needs_reconcile,
                16,
            );
            assert!(paths.is_empty(), "{kind:?} must not schedule work");
            assert!(!needs_reconcile, "{kind:?} must not force a reconcile");
            assert!(
                filter_event(
                    Event {
                        kind,
                        paths: vec![path.clone()],
                        attrs: Default::default(),
                    },
                    &(|_: &Path| true) as &dyn Fn(&Path) -> bool,
                )
                .is_none(),
                "{kind:?} must be dropped before it occupies the queue"
            );
            assert!(
                coalesce([Event {
                    kind,
                    paths: vec![path.clone()],
                    attrs: Default::default(),
                }])
                .paths
                .is_empty(),
                "{kind:?} must not survive coalescing either"
            );
        }

        // A completed write is reported as a close on some backends; dropping it would lose edits.
        let mut paths = BTreeSet::new();
        let mut needs_reconcile = false;
        for kind in [
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Modify(ModifyKind::Any),
        ] {
            accumulate_event(
                Event {
                    kind,
                    paths: vec![path.clone()],
                    attrs: Default::default(),
                },
                &mut paths,
                &mut needs_reconcile,
                16,
            );
            assert!(paths.contains(&path), "{kind:?} is a real change");
            paths.clear();
        }
    }

    #[test]
    fn duplicate_events_are_coalesced() {
        let path = PathBuf::from("src/a.ts");
        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![path.clone()],
            attrs: Default::default(),
        };
        let result = coalesce([event.clone(), event]);
        assert_eq!(result.paths, vec![path]);
        assert!(!result.needs_reconcile);
    }

    #[test]
    fn a_batch_tells_which_paths_may_be_directories() {
        use notify::event::{DataChange, ModifyKind, RemoveKind, RenameMode};
        let event = |kind: EventKind, paths: &[&str]| Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        };
        let rename = || EventKind::Modify(ModifyKind::Name(RenameMode::Both));

        // A directory renamed, made, and removed: each is named alone, without what it holds.
        let batch = coalesce([
            event(rename(), &["src/feat", "src/feat2"]),
            event(EventKind::Create(CreateKind::Folder), &["src/new"]),
            event(EventKind::Remove(RemoveKind::Folder), &["src/old"]),
        ]);
        assert_eq!(
            batch.structural,
            ["src/feat", "src/feat2", "src/new", "src/old"].map(PathBuf::from)
        );

        // An editor's save: the file is renamed to a backup, written anew, and the backup removed.
        // Nothing here is a directory, and nothing should make the watcher look for one.
        let batch = coalesce([
            event(rename(), &["src/a.ts", "src/a.ts~"]),
            event(EventKind::Create(CreateKind::File), &["src/a.ts"]),
            event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                &["src/a.ts"],
            ),
            event(EventKind::Remove(RemoveKind::File), &["src/a.ts~"]),
        ]);
        assert!(batch.structural.is_empty(), "{:?}", batch.structural);
        assert_eq!(batch.paths, ["src/a.ts", "src/a.ts~"].map(PathBuf::from));
    }

    #[test]
    fn a_directory_moved_inside_the_tree_is_reported_as_possibly_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("feat/deep")).unwrap();
        std::fs::write(root.join("feat/deep/a.ts"), "export {}\n").unwrap();
        let watcher = PersistentWatcher::new(&root, 4_096).unwrap();
        std::fs::rename(root.join("feat"), root.join("feat2")).unwrap();
        let batch = watcher
            .next_batch(
                Duration::from_millis(50),
                Duration::from_secs(5),
                64,
                Duration::from_secs(2),
            )
            .unwrap();
        assert!(!batch.needs_reconcile);
        assert!(batch.structural.contains(&root.join("feat")), "{batch:?}");
        assert!(batch.structural.contains(&root.join("feat2")), "{batch:?}");
        assert!(
            !batch.paths.contains(&root.join("feat2/deep/a.ts")),
            "the backend names the directory alone: {batch:?}"
        );
    }

    #[test]
    fn filtering_drops_noise_before_queueing_but_keeps_relevant_rename_side() {
        use notify::event::{ModifyKind, RenameMode};

        let event = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![PathBuf::from(".ravel/old"), PathBuf::from("src/new.ts")],
            attrs: Default::default(),
        };
        let filtered = filter_event(event, &|path| !path.starts_with(".ravel")).unwrap();
        assert_eq!(filtered.paths, vec![PathBuf::from("src/new.ts")]);
    }

    #[test]
    fn filtering_keeps_pathless_backend_reconcile_signal() {
        let event = Event {
            kind: EventKind::Other,
            paths: Vec::new(),
            attrs: Default::default(),
        };
        assert!(filter_event(event, &|_| false).is_some());
    }

    #[test]
    fn reconcile_signal_discards_partial_paths_from_the_burst() {
        let mut paths = BTreeSet::new();
        let mut needs_reconcile = false;
        accumulate_event(
            Event {
                kind: EventKind::Create(CreateKind::File),
                paths: vec![PathBuf::from("src/a.ts")],
                attrs: Default::default(),
            },
            &mut paths,
            &mut needs_reconcile,
            16,
        );
        accumulate_event(
            Event {
                kind: EventKind::Other,
                paths: Vec::new(),
                attrs: Default::default(),
            },
            &mut paths,
            &mut needs_reconcile,
            16,
        );

        assert!(needs_reconcile);
        assert!(paths.is_empty());
    }

    #[test]
    fn continuous_distinct_paths_emit_bounded_exact_batch_without_overflow() {
        let root = tempfile::tempdir().unwrap();
        let watcher = PersistentWatcher::new_filtered(root.path(), 4_096, |path| {
            path.extension().and_then(|value| value.to_str()) == Some("ts")
        })
        .unwrap();
        let producer_root = root.path().to_path_buf();
        let producer = std::thread::spawn(move || {
            for index in 0..64 {
                std::fs::write(producer_root.join(format!("file-{index}.ts")), b"export {}")
                    .unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let debounce = Duration::from_millis(30);
        let max_batch = Duration::from_secs(1);
        let first_event_timeout = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let batch = watcher
            .next_batch(debounce, first_event_timeout, 8, max_batch)
            .unwrap();
        // Measure the watcher return, not producer teardown. Joining first made this assertion
        // depend on filesystem callback and scheduler latency after the batch had already met
        // its bound (notably on macOS ARM runners).
        let batch_elapsed = started.elapsed();
        producer.join().unwrap();

        assert!(!batch.needs_reconcile);
        assert!(!batch.paths.is_empty());
        assert!(batch.paths.len() <= 8);
        assert!(
            batch_elapsed
                <= first_event_timeout
                    .saturating_add(max_batch)
                    .saturating_add(debounce),
            "batch exceeded first-event timeout plus configured batch deadline: {batch_elapsed:?}"
        );
    }

    #[test]
    fn continuous_duplicate_stream_returns_at_batch_deadline() {
        let root = tempfile::tempdir().unwrap();
        let watched = root.path().join("same.ts");
        std::fs::write(&watched, b"0").unwrap();
        let watcher = PersistentWatcher::new_filtered(root.path(), 4_096, |path| {
            path.extension().and_then(|value| value.to_str()) == Some("ts")
        })
        .unwrap();
        let producer = std::thread::spawn(move || {
            for index in 0..100 {
                std::fs::write(&watched, index.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let started = std::time::Instant::now();
        let batch = watcher
            .next_batch(
                Duration::from_millis(30),
                Duration::from_secs(1),
                64,
                Duration::from_millis(50),
            )
            .unwrap();
        let elapsed = started.elapsed();
        producer.join().unwrap();

        assert!(!batch.needs_reconcile);
        assert_eq!(batch.paths.len(), 1);
        assert!(elapsed < Duration::from_millis(250));
    }

    #[test]
    fn pending_reconcile_is_deferred_until_stream_is_quiet() {
        let root = tempfile::tempdir().unwrap();
        let watched = root.path().join("same.ts");
        std::fs::write(&watched, b"0").unwrap();
        let watcher = PersistentWatcher::new_filtered(root.path(), 4_096, |path| {
            path.extension().and_then(|value| value.to_str()) == Some("ts")
        })
        .unwrap();
        watcher.reconcile_pending.store(true, Ordering::Release);
        let producer = std::thread::spawn(move || {
            for index in 0..60 {
                std::fs::write(&watched, index.to_string()).unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let during_storm = watcher
            .next_batch(
                Duration::from_millis(30),
                Duration::from_secs(1),
                64,
                Duration::from_millis(20),
            )
            .unwrap();
        assert!(!during_storm.needs_reconcile);
        assert!(during_storm.paths.is_empty());
        producer.join().unwrap();

        let after_quiet = watcher
            .next_batch(
                Duration::from_millis(30),
                Duration::from_secs(1),
                64,
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(after_quiet.needs_reconcile);
        assert!(after_quiet.paths.is_empty());
    }

    /// A quiet period longer than a whole batch could never be observed inside one: the pending
    /// reconcile was put off forever, and every event after it drained unread.
    #[test]
    fn a_debounce_longer_than_the_batch_does_not_make_the_watcher_deaf() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let watcher = PersistentWatcher::new(&root, 4_096).unwrap();
        watcher.reconcile_pending.store(true, Ordering::Release);
        let next = || {
            watcher.next_batch(
                Duration::from_millis(400),
                Duration::from_secs(2),
                64,
                Duration::from_millis(100),
            )
        };

        assert!(
            (0..5).any(|_| next().unwrap().needs_reconcile),
            "the pending reconcile never ran"
        );
        std::fs::write(root.join("after.ts"), "export {}\n").unwrap();
        let batch = next().unwrap();
        assert!(
            batch.paths.contains(&root.join("after.ts")),
            "an edit after it went unheard: {batch:?}"
        );
    }

    #[test]
    fn filtering_preserves_delete_and_both_relevant_rename_paths() {
        use notify::event::{ModifyKind, RemoveKind, RenameMode};

        let deleted = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![PathBuf::from("src/deleted.ts")],
            attrs: Default::default(),
        };
        assert_eq!(
            filter_event(deleted, &|_| true).unwrap().paths,
            vec![PathBuf::from("src/deleted.ts")]
        );

        let renamed = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![PathBuf::from("src/old.ts"), PathBuf::from("src/new.ts")],
            attrs: Default::default(),
        };
        assert_eq!(
            filter_event(renamed, &|_| true).unwrap().paths,
            vec![PathBuf::from("src/old.ts"), PathBuf::from("src/new.ts")]
        );
    }

    /// A watched temp tree whose storage directory holds the gate's markers. `None` when this
    /// machine's temp directory is on a filesystem the gate refuses to vouch for, in which case
    /// there is nothing for these tests to prove.
    fn gated_tree_with(
        reverify: Duration,
        settle: Duration,
    ) -> Option<(
        tempfile::TempDir,
        PathBuf,
        PersistentWatcher,
        Arc<WatchGate>,
    )> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        // A healthy backend returns a marker in microseconds; the long deadline is for a machine
        // so loaded that this test's own threads are starved, which is not what is under test.
        let timing = Timing {
            barrier: Duration::from_secs(3),
            reverify,
            settle,
            ..Timing::default()
        };
        let gate = WatchGate::with_timing(&root, &storage, timing);
        if !gate.is_trusted() {
            return None;
        }
        let ignored = storage.clone();
        let watcher = PersistentWatcher::new_with_gate(&root, 4_096, gate.clone(), move |path| {
            !path.starts_with(&ignored)
        })
        .unwrap();
        Some((dir, root, watcher, gate))
    }

    /// No settling delay: the tests that are not about it should not wait on it.
    fn gated_tree() -> Option<(
        tempfile::TempDir,
        PathBuf,
        PersistentWatcher,
        Arc<WatchGate>,
    )> {
        gated_tree_with(Duration::from_secs(60), Duration::ZERO)
    }

    /// The property the gate stands on: whatever finished before a probe is either counted, or the
    /// probe says it cannot vouch. Everything in a directory the backend already watches is
    /// counted. A file written into a directory created a moment earlier can be missed -- the
    /// backend installs that directory's watch only after announcing it -- and that is exactly why
    /// a directory-creating event keeps the gate from vouching for a while.
    #[test]
    fn a_probe_has_counted_everything_that_finished_before_it_or_refuses_to_vouch() {
        let Some((_dir, root, _watcher, gate)) =
            gated_tree_with(Duration::from_secs(60), Duration::from_secs(2))
        else {
            return;
        };
        let mut seen = gate
            .probe()
            .expect("a healthy watcher returns its marker")
            .events;
        let mut step = |label: String, watched: bool, operation: &dyn Fn()| {
            operation();
            let probe = gate.probe().expect("a healthy watcher returns its marker");
            assert!(
                probe.events > seen || (!watched && !probe.settled),
                "{label}: finished before the probe yet neither counted ({seen} -> {}) nor refused",
                probe.events
            );
            seen = probe.events;
        };
        for round in 0..200 {
            let file = root.join(format!("f{round}.ts"));
            step(format!("create {round}"), true, &|| {
                std::fs::write(&file, "export const a = 1;\n").unwrap();
            });
            step(format!("modify {round}"), true, &|| {
                std::fs::write(&file, "export const a = 2;\n").unwrap();
            });
            let renamed = root.join(format!("r{round}.ts"));
            step(format!("rename {round}"), true, &|| {
                std::fs::rename(&file, &renamed).unwrap();
            });
            step(format!("mkdir {round}"), true, &|| {
                std::fs::create_dir(root.join(format!("d{round}"))).unwrap();
            });
            // The racy one: the directory exists, its watch may not yet.
            step(format!("file in new dir {round}"), false, &|| {
                std::fs::write(root.join(format!("d{round}/x.ts")), "x").unwrap();
            });
            step(format!("delete {round}"), true, &|| {
                std::fs::remove_file(&renamed).unwrap();
            });
        }
    }

    #[test]
    fn a_probe_never_misses_what_a_concurrent_writer_finished_before_it() {
        let Some((_dir, root, _watcher, gate)) = gated_tree() else {
            return;
        };
        let completed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let writer = {
            let (completed, root) = (completed.clone(), root.clone());
            std::thread::spawn(move || {
                for index in 0..1_500u32 {
                    std::fs::write(root.join(format!("w{index}.ts")), "export {}\n").unwrap();
                    completed.fetch_add(1, Ordering::Release);
                }
            })
        };
        let mut probes = 0u32;
        while !writer.is_finished() || probes < 50 {
            // Every write counted here had returned before the marker was created, so each of
            // them -- one new file, one `Create` event at the least -- must already be counted.
            let finished = completed.load(Ordering::Acquire);
            let probe = gate.probe().expect("a healthy watcher returns its marker");
            assert!(
                probe.events >= finished,
                "{finished} writes had finished but only {} events were counted",
                probe.events
            );
            probes += 1;
        }
        writer.join().unwrap();
    }

    #[test]
    fn reading_files_and_writing_the_storage_directory_are_not_changes() {
        let Some((_dir, root, _watcher, gate)) = gated_tree() else {
            return;
        };
        std::fs::write(root.join("a.ts"), "export {}\n").unwrap();
        let before = gate.probe().unwrap().events;
        for _ in 0..50 {
            std::fs::read(root.join("a.ts")).unwrap();
        }
        std::fs::write(root.join(".ravel/CURRENT"), "manifest").unwrap();
        std::fs::write(root.join(".ravel/other"), "x").unwrap();
        assert_eq!(gate.probe().unwrap().events, before);
    }

    #[test]
    fn a_mark_holds_until_something_changes_or_the_generation_moves() {
        let Some((_dir, root, _watcher, gate)) = gated_tree() else {
            return;
        };
        let probe = gate.probe().unwrap();
        assert!(!gate.is_quiet(&probe, "g1"), "nothing has been checked yet");
        gate.mark_clean(&probe, "g1".into());
        let again = gate.probe().unwrap();
        assert!(gate.is_quiet(&again, "g1"));
        assert!(
            !gate.is_quiet(&again, "g2"),
            "another generation is another index"
        );

        std::fs::write(root.join("a.ts"), "export {}\n").unwrap();
        let after_edit = gate.probe().unwrap();
        assert!(!gate.is_quiet(&after_edit, "g1"), "a change ends the quiet");

        // A mark is only taken for a check that no event interrupted.
        let checking = gate.probe().unwrap();
        std::fs::write(root.join("a.ts"), "export const a = 1;\n").unwrap();
        gate.probe().unwrap();
        gate.mark_clean(&checking, "g1".into());
        assert!(
            !gate.is_quiet(&gate.probe().unwrap(), "g1"),
            "an edit during the check must not be vouched for"
        );

        gate.mark_clean(&gate.probe().unwrap(), "g1".into());
        assert!(gate.is_quiet(&gate.probe().unwrap(), "g1"));
        gate.degrade();
        assert!(
            gate.probe().is_none(),
            "a degraded gate vouches for nothing"
        );
    }

    #[test]
    fn nothing_is_called_quiet_while_a_new_directory_may_lack_its_watch() {
        let Some((_dir, root, _watcher, gate)) =
            gated_tree_with(Duration::from_secs(60), Duration::from_millis(400))
        else {
            return;
        };
        // A file made in a directory that is already watched has no watch to wait for.
        std::fs::write(root.join("plain.ts"), "export {}\n").unwrap();
        assert!(gate.probe().unwrap().settled);

        std::fs::create_dir(root.join("fresh")).unwrap();
        let probe = gate.probe().unwrap();
        assert!(!probe.settled);
        gate.mark_clean(&probe, "g".into());
        std::thread::sleep(Duration::from_millis(450));
        let later = gate.probe().unwrap();
        assert!(later.settled);
        assert!(
            !gate.is_quiet(&later, "g"),
            "a check that began before the directory settled must not have been recorded"
        );
        gate.mark_clean(&later, "g".into());
        assert!(gate.is_quiet(&gate.probe().unwrap(), "g"));

        // A move cannot say whether what moved was a directory.
        std::fs::rename(root.join("plain.ts"), root.join("moved.ts")).unwrap();
        assert!(!gate.probe().unwrap().settled);
    }

    /// A directory moved into the tree, or renamed inside it, is watched like any other once the
    /// backend has had a moment to install its watches: edits inside it are counted.
    #[test]
    fn edits_inside_a_moved_directory_are_counted() {
        let Some((_dir, root, _watcher, gate)) = gated_tree() else {
            return;
        };
        // Beside the watched tree, on the same filesystem: "somewhere else".
        let elsewhere = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        std::fs::create_dir_all(elsewhere.path().join("pkg/deep")).unwrap();
        std::fs::write(elsewhere.path().join("pkg/deep/a.ts"), "export {}\n").unwrap();
        std::fs::rename(elsewhere.path().join("pkg"), root.join("pkg")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let before = gate.probe().unwrap().events;
        std::fs::write(root.join("pkg/deep/a.ts"), "export const a = 1;\n").unwrap();
        assert!(
            gate.probe().unwrap().events > before,
            "an edit inside a directory moved in was not counted"
        );

        std::fs::rename(root.join("pkg"), root.join("renamed")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let before = gate.probe().unwrap().events;
        std::fs::write(root.join("renamed/deep/a.ts"), "export const a = 2;\n").unwrap();
        assert!(
            gate.probe().unwrap().events > before,
            "an edit inside a renamed directory was not counted"
        );
    }

    #[test]
    fn a_quiet_verdict_expires() {
        let Some((_dir, _root, _watcher, gate)) =
            gated_tree_with(Duration::from_millis(200), Duration::from_secs(1))
        else {
            return;
        };
        gate.mark_clean(&gate.probe().unwrap(), "g".into());
        assert!(gate.is_quiet(&gate.probe().unwrap(), "g"));
        std::thread::sleep(Duration::from_millis(250));
        assert!(!gate.is_quiet(&gate.probe().unwrap(), "g"));
    }

    #[test]
    fn a_watcher_that_goes_away_stops_vouching() {
        let Some((_dir, _root, watcher, gate)) = gated_tree() else {
            return;
        };
        assert!(gate.probe().is_some());
        drop(watcher);
        assert!(gate.probe().is_none());
    }

    #[test]
    fn markers_that_never_return_pause_the_gate_instead_of_stalling_every_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        // No watcher feeds this gate, so no marker can ever come back.
        let timing = Timing {
            pause: Duration::from_millis(600),
            ..Timing::default()
        };
        let gate = WatchGate::with_timing(&root, &storage, timing);
        if !gate.is_trusted() {
            return;
        }
        for _ in 0..MAX_BARRIER_FAILURES {
            assert!(gate.probe().is_none());
        }
        let started = Instant::now();
        assert!(gate.probe().is_none());
        assert!(
            started.elapsed() < BARRIER_TIMEOUT / 2,
            "a paused gate must not wait for a marker"
        );
        let leftovers = std::fs::read_dir(&storage).unwrap().count();
        assert_eq!(
            leftovers, 0,
            "markers are removed whether or not they return"
        );
        // The pause ends, and the gate asks again rather than giving up for good.
        std::thread::sleep(Duration::from_millis(650));
        let started = Instant::now();
        assert!(gate.probe().is_none());
        assert!(
            started.elapsed() >= BARRIER_TIMEOUT / 2,
            "after the pause the gate looks again"
        );
    }

    /// A backend that stops answering -- starved, wedged, anything -- loses the gate its mark and
    /// earns it a pause, and when the backend is back the first verdict comes from a full check.
    #[test]
    fn a_stalled_backend_costs_the_mark_and_a_pause_and_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        let timing = Timing {
            settle: Duration::ZERO,
            pause: Duration::from_millis(500),
            ..Timing::default()
        };
        let gate = WatchGate::with_timing(&root, &storage, timing);
        if !gate.is_trusted() {
            return;
        }
        let stall = Arc::new(AtomicBool::new(false));
        let ignored = storage.clone();
        let _watcher = PersistentWatcher::new_with_gate(&root, 4_096, gate.clone(), {
            let stall = stall.clone();
            move |path| {
                // The backend's one thread is busy with this event for a while.
                if path.ends_with("stall.ts") && stall.swap(false, Ordering::AcqRel) {
                    std::thread::sleep(Duration::from_millis(1_200));
                }
                !path.starts_with(&ignored)
            }
        })
        .unwrap();
        gate.mark_clean(&gate.probe().unwrap(), "g".into());
        assert!(gate.is_quiet(&gate.probe().unwrap(), "g"));

        stall.store(true, Ordering::Release);
        std::fs::write(root.join("stall.ts"), "export {}\n").unwrap();
        assert!(
            gate.probe().is_none(),
            "the marker cannot come back in time"
        );
        for _ in 1..MAX_BARRIER_FAILURES {
            assert!(gate.probe().is_none());
        }
        let started = Instant::now();
        assert!(gate.probe().is_none());
        assert!(
            started.elapsed() < BARRIER_TIMEOUT / 2,
            "the gate is paused"
        );

        std::thread::sleep(Duration::from_millis(1_300));
        let probe = gate.probe().expect("the backend answers again");
        assert!(
            !gate.is_quiet(&probe, "g"),
            "what was vouched for before the stall is not vouched for after it"
        );
        gate.mark_clean(&probe, "g".into());
        assert!(gate.is_quiet(&gate.probe().unwrap(), "g"));
    }

    #[test]
    fn markers_a_dead_process_left_behind_are_swept_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join(".watch-cookie-4242-7"), "").unwrap();
        std::fs::write(storage.join("CURRENT"), "manifest").unwrap();
        let gate = WatchGate::with_timing(&root, &storage, Timing::default());
        if !gate.is_trusted() {
            return;
        }
        assert!(!storage.join(".watch-cookie-4242-7").exists());
        assert!(storage.join("CURRENT").exists());
    }

    #[test]
    fn a_change_to_an_ignore_rules_file_ends_the_gate_for_good() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        let fresh = || WatchGate::with_timing(&root, &storage, Timing::default());
        if !fresh().is_trusted() {
            return;
        }
        use notify::event::{AccessKind, AccessMode, DataChange};
        let event = |kind: EventKind, path: &str| {
            Ok(Event {
                kind,
                paths: vec![root.join(path)],
                attrs: Default::default(),
            })
        };
        let write = || EventKind::Modify(ModifyKind::Data(DataChange::Any));
        let read = || EventKind::Access(AccessKind::Open(AccessMode::Any));
        for path in [
            ".gitignore",
            "apps/web/.gitignore",
            ".ravelignore",
            ".git/info/exclude",
        ] {
            let gate = fresh();
            assert!(!gate.absorb(&event(read(), path)));
            assert!(gate.usable(), "reading {path} changes nothing");
            assert!(!gate.absorb(&event(write(), path)));
            assert!(!gate.usable(), "writing {path} changes what is ignored");
        }
        let gate = fresh();
        for path in [
            "src/a.ts",
            "gitignore",
            "notes/.gitignored",
            ".git/info/refs",
        ] {
            assert!(!gate.absorb(&event(write(), path)));
        }
        assert!(gate.usable());
    }

    /// A package's own `.gitignore` under `node_modules` is below a directory the relevance filter
    /// drops whole, so `npm install` must not end the gate; the workspace's own rules still do.
    #[test]
    fn rules_files_below_a_dropped_directory_leave_the_gate_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join(".ravel");
        std::fs::create_dir_all(&storage).unwrap();
        let config = crate::config::Config::default();
        let fresh = || {
            let gate = WatchGate::with_timing(&root, &storage, Timing::default());
            let config = config.clone();
            gate.ignore_rules_below(move |relative| config.is_noise_relative(relative));
            gate
        };
        if !fresh().is_trusted() {
            return;
        }
        let write = |path: &str| {
            Ok(Event {
                kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                paths: vec![root.join(path)],
                attrs: Default::default(),
            })
        };
        let gate = fresh();
        for path in [
            "node_modules/left-pad/.gitignore",
            "packages/web/node_modules/esbuild/.gitignore",
            "dist/.ravelignore",
        ] {
            assert!(!gate.absorb(&write(path)));
            assert!(gate.usable(), "{path} cannot make anything relevant");
        }
        for path in [".gitignore", "packages/web/.gitignore", ".git/info/exclude"] {
            let gate = fresh();
            assert!(!gate.absorb(&write(path)));
            assert!(!gate.usable(), "writing {path} changes what is ignored");
        }
    }

    #[test]
    fn the_deepest_mount_decides_the_filesystem_type() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:35 / /home/user/share rw,relatime shared:5 - fuse.grpcfuse grpcfuse rw
41 22 0:36 / /mnt/with\\040space rw - xfs /dev/sdb rw
42 22 0:37 / /srv rw master:3 shared:9 - nfs4 server:/export rw
garbage line
43 22 0:38 / /srv/local rw - tmpfs tmpfs rw
";
        let kind = |path: &str| mount_type_of(mountinfo, Path::new(path));
        assert_eq!(kind("/home/user/project"), Some("ext4"));
        assert_eq!(kind("/home/user/share/repo"), Some("fuse.grpcfuse"));
        assert_eq!(kind("/home/user/sharepoint/repo"), Some("ext4"));
        assert_eq!(kind("/mnt/with space/repo"), Some("xfs"));
        assert_eq!(kind("/srv/x"), Some("nfs4"));
        assert_eq!(kind("/srv/local/x"), Some("tmpfs"));
        assert_eq!(kind("/"), Some("ext4"));
        assert_eq!(unescape_mount_field("a\\040b\\134c"), "a b\\c");

        // A share mounted inside a local tree is invisible to the tree's watcher.
        let below = |path: &str| mounts_below_are_local(mountinfo, Path::new(path));
        assert!(below("/home/user/project"));
        assert!(!below("/home/user"));
        assert!(!below("/"));
        assert!(below("/mnt/with space"));
        assert!(
            below("/srv"),
            "the mount holding the path is judged on its own"
        );
        assert!(below("/srv/local"));
    }
}
