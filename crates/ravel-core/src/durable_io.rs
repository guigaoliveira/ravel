use std::{io, path::Path};

#[cfg(unix)]
use std::fs;

#[cfg(not(windows))]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Windows replace failures that mean "someone is holding the destination right now" rather than
/// "this will never work". A reader that merely has the file open is enough: `MoveFileExW` reports
/// it as `ERROR_ACCESS_DENIED`, which is indistinguishable by name from a permissions problem but
/// clears on its own within milliseconds.
#[cfg(windows)]
fn replace_is_retryable(error: &io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    matches!(
        error.raw_os_error(),
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
    )
}

/// How long a replace keeps retrying before giving up. Long enough to outlast a reader opening and
/// closing the file, short enough that a genuine permissions failure is still reported promptly.
#[cfg(windows)]
const REPLACE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(windows)]
const REPLACE_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

#[cfg(windows)]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // Each attempt is still atomic; retrying only widens the window in which one can start. Unlike
    // POSIX rename, `MoveFileExW` refuses to replace a destination another handle has open, so a
    // second Ravel process merely *reading* CURRENT while this one published made the publish fail
    // outright with "Access is denied."
    let deadline = std::time::Instant::now() + REPLACE_RETRY_BUDGET;
    loop {
        let result = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !replace_is_retryable(&error) || std::time::Instant::now() >= deadline {
            return Err(error);
        }
        std::thread::sleep(REPLACE_RETRY_PAUSE);
    }
}

#[cfg(windows)]
#[cfg(test)]
mod windows_tests {
    use super::*;

    #[test]
    fn only_contention_is_retried() {
        assert!(replace_is_retryable(&io::Error::from_raw_os_error(5)));
        assert!(replace_is_retryable(&io::Error::from_raw_os_error(32)));
        // ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND never clear by waiting.
        assert!(!replace_is_retryable(&io::Error::from_raw_os_error(2)));
        assert!(!replace_is_retryable(&io::Error::from_raw_os_error(3)));
    }
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    // atomic_replace uses MOVEFILE_WRITE_THROUGH. Windows does not permit opening a directory
    // through std::fs::File, and attempting it fails with ERROR_ACCESS_DENIED.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
