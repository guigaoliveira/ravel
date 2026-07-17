//! Opt-in stage timing for performance work. Enabled via `RAVEL_TIMING=1`;
//! zero overhead otherwise beyond one branch per stage.

use std::sync::OnceLock;
use std::time::Instant;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("RAVEL_TIMING").is_some())
}

/// Log elapsed time for a stage with an optional detail (counts, byte sizes).
pub fn stage(label: &str, start: Instant, detail: impl FnOnce() -> String) {
    if enabled() {
        eprintln!(
            "[ravel-timing] {label} {:.1}ms {}",
            start.elapsed().as_secs_f64() * 1000.0,
            detail()
        );
    }
}

/// Log a bare note (no duration), e.g. sizes or chosen path.
pub fn note(label: &str, detail: impl FnOnce() -> String) {
    if enabled() {
        eprintln!("[ravel-timing] {label} {}", detail());
    }
}
