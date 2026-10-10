//! Cross-process generation lifetime barrier.
//!
//! Lock order for code that also needs another storage lock is:
//! `update.lock` -> `generation-gc.lock` -> component-specific lock.

use fs4::fs_std::FileExt;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct GenerationGuard {
    _file: fs::File,
}

impl GenerationGuard {
    pub fn shared(root: &Path) -> io::Result<Self> {
        Self::acquire(root, false)
    }

    pub fn exclusive(root: &Path) -> io::Result<Self> {
        Self::acquire(root, true)
    }

    pub fn try_exclusive(root: &Path) -> io::Result<Option<Self>> {
        fs::create_dir_all(root)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(root))?;
        if FileExt::try_lock_exclusive(&file)? {
            Ok(Some(Self { _file: file }))
        } else {
            Ok(None)
        }
    }

    fn acquire(root: &Path, exclusive: bool) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let path = lock_path(root);
        let file = if exclusive {
            open_for_locking(&path)?
        } else {
            open_read_only_if_present(&path)?
        };
        if exclusive {
            FileExt::lock_exclusive(&file)?;
        } else {
            lock_shared_without_queueing(&file)?;
        }
        Ok(Self { _file: file })
    }
}

/// Take a shared lock by retrying a non-blocking attempt rather than sleeping in the kernel's
/// queue for it.
///
/// macOS does not wake every waiter when an exclusive `flock` is released: it wakes one and queues
/// the others behind the lock that one is granted. Readers that arrived together behind a GC pass
/// -- a cold `context` starts three -- then sleep for as long as the first keeps its shared lock,
/// and a cached search index or symbol table keeps it for the life of the process. The daemon hung
/// that way, every reader thread in `flock(LOCK_SH)` with no exclusive holder left. The exclusive
/// holders (generation GC, artifact compaction) only ever try their lock and keep it for
/// milliseconds, so retrying costs nothing a query can measure.
pub(crate) fn lock_shared_without_queueing(file: &fs::File) -> io::Result<()> {
    let mut pause = std::time::Duration::from_micros(200);
    loop {
        match FileExt::try_lock_shared(file) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(std::time::Duration::from_millis(10));
    }
}

fn open_for_locking(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

/// A shared lock needs the file to exist, not to be writable. Every read of the index takes one,
/// and closing a file that was opened for writing is reported to a watcher as a finished write:
/// the daemon's own watcher was woken several times per query by a lock file nobody had changed.
fn open_read_only_if_present(path: &Path) -> io::Result<fs::File> {
    match fs::File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => open_for_locking(path),
        opened => opened,
    }
}

pub fn lock_path(root: &Path) -> PathBuf {
    root.join("generation-gc.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    fn access_mode(guard: &GenerationGuard) -> u32 {
        use std::os::fd::AsRawFd;
        let info =
            fs::read_to_string(format!("/proc/self/fdinfo/{}", guard._file.as_raw_fd())).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .unwrap();
        u32::from_str_radix(flags.trim(), 8).unwrap() & 0o3
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shared_lock_is_taken_through_a_read_only_handle() {
        // Closing a handle opened for writing reaches a file watcher as a completed write.
        const O_RDONLY: u32 = 0;
        const O_RDWR: u32 = 2;
        let dir = tempfile::tempdir().unwrap();
        // First use creates the file, which needs the write-capable open ...
        let first = GenerationGuard::shared(dir.path()).unwrap();
        assert!(lock_path(dir.path()).is_file());
        drop(first);
        // ... and every later reader finds it and does not.
        let shared = GenerationGuard::shared(dir.path()).unwrap();
        let other = GenerationGuard::shared(dir.path()).unwrap();
        assert_eq!(access_mode(&shared), O_RDONLY);
        assert_eq!(access_mode(&other), O_RDONLY);
        drop((shared, other));
        // A writer keeps its handle, and still excludes readers.
        let exclusive = GenerationGuard::exclusive(dir.path()).unwrap();
        assert_eq!(access_mode(&exclusive), O_RDWR);
        assert!(
            GenerationGuard::try_exclusive(dir.path())
                .unwrap()
                .is_none()
        );
    }

    /// Readers that queued behind an exclusive holder all get in once it lets go, even though each
    /// keeps its lock afterwards. Sleeping in `flock` for it, macOS let one through and left the
    /// others queued behind that one's lock for as long as it was held.
    #[test]
    fn readers_queued_behind_an_exclusive_lock_all_enter_while_each_stays_held() {
        let dir = tempfile::tempdir().unwrap();
        let exclusive = GenerationGuard::exclusive(dir.path()).unwrap();
        let (entered, arrivals) = mpsc::channel();
        let release = Arc::new(Barrier::new(4));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let root = dir.path().to_path_buf();
                let entered = entered.clone();
                let release = release.clone();
                std::thread::spawn(move || {
                    let _shared = GenerationGuard::shared(&root).unwrap();
                    entered.send(()).unwrap();
                    release.wait();
                })
            })
            .collect();
        // Long enough for every reader to be waiting on the exclusive holder.
        std::thread::sleep(Duration::from_millis(200));
        assert!(arrivals.try_recv().is_err(), "a reader got past a writer");
        drop(exclusive);
        for reader in 0..3 {
            arrivals
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("reader {reader} of 3 never got its shared lock"));
        }
        release.wait();
        for reader in readers {
            reader.join().unwrap();
        }
    }

    #[test]
    fn exclusive_waits_for_reader_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let shared = GenerationGuard::shared(dir.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            let _exclusive = GenerationGuard::exclusive(&root).unwrap();
            tx.send(()).unwrap();
        });
        barrier.wait();
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(shared);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
    }
}
