use blake3::Hash;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use thiserror::Error;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CoalescedChange {
    pub paths: Vec<PathBuf>,
    pub needs_reconcile: bool,
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
        // A bounded queue prevents an editor/event storm from growing the process without limit.
        // Overflow deliberately degrades to a full reconcile on the next batch.
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = overflowed.clone();
        let mut watcher = notify::recommended_watcher(move |result| {
            let result = match result {
                Ok(event) => {
                    let Some(event) = filter_event(event, &path_is_relevant) else {
                        return;
                    };
                    Ok(event)
                }
                Err(error) => Err(error),
            };
            if sender.try_send(result).is_err() {
                callback_overflowed.store(true, Ordering::Release);
            }
        })
        .map_err(|error| WatchError::Notify(error.to_string()))?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|error| WatchError::Notify(error.to_string()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            overflowed,
            reconcile_pending: AtomicBool::new(false),
        })
    }

    pub fn next_batch(
        &self,
        debounce: Duration,
        timeout: Duration,
        max_paths: usize,
        max_batch: Duration,
    ) -> Result<CoalescedChange, WatchError> {
        let mut paths = BTreeSet::new();
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
        Ok(CoalescedChange {
            paths: paths.into_iter().collect(),
            needs_reconcile,
        })
    }
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
    event.paths.retain(|path| path_is_relevant(path));
    (!event.paths.is_empty() || matches!(event.kind, EventKind::Other)).then_some(event)
}

pub fn coalesce(events: impl IntoIterator<Item = Event>) -> CoalescedChange {
    let mut paths = BTreeSet::new();
    let mut needs_reconcile = false;
    for event in events {
        if matches!(event.kind, EventKind::Other) {
            needs_reconcile = true;
        }
        for path in event.paths {
            paths.insert(path);
        }
    }
    CoalescedChange {
        paths: paths.into_iter().collect(),
        needs_reconcile,
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
}
