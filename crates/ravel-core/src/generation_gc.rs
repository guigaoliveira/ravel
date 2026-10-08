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
            FileExt::lock_shared(&file)?;
        }
        Ok(Self { _file: file })
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
