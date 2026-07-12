use blake3::Hash;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::mpsc,
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
    #[error("watch channel closed")]
    Closed,
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

pub fn watch_batch(
    root: &Path,
    debounce: Duration,
    timeout: Duration,
) -> Result<CoalescedChange, WatchError> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = sender.send(result);
    })
    .map_err(|error| WatchError::Notify(error.to_string()))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|error| WatchError::Notify(error.to_string()))?;
    let first = receiver
        .recv_timeout(timeout)
        .map_err(|_| WatchError::Closed)?
        .map_err(|error| WatchError::Notify(error.to_string()))?;
    let mut events = vec![first];
    let start = std::time::Instant::now();
    loop {
        // Read the clock once per iteration (was queried twice: guard + timeout).
        let elapsed = start.elapsed();
        if elapsed >= debounce {
            break;
        }
        match receiver.recv_timeout(debounce.saturating_sub(elapsed)) {
            Ok(Ok(event)) => events.push(event),
            Ok(Err(error)) => return Err(WatchError::Notify(error.to_string())),
            Err(_) => break,
        }
    }
    Ok(coalesce(events))
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
}
