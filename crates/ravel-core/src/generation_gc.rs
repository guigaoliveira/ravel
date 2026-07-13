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
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if exclusive {
            FileExt::lock_exclusive(&file)?;
        } else {
            FileExt::lock_shared(&file)?;
        }
        Ok(Self { _file: file })
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
