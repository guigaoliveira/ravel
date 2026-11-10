//! Opt-in stage timing for performance work. Enabled via `RAVEL_TIMING=1`;
//! zero overhead otherwise beyond one branch per stage.

use std::ffi::OsStr;
use std::sync::OnceLock;
use std::time::Instant;

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| switched_on(std::env::var_os("RAVEL_TIMING").as_deref()))
}

/// `RAVEL_TIMING=0`, `false`, `off` or an empty value leave timing off, as they read.
fn switched_on(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        let value = value.to_string_lossy();
        let value = value.trim();
        !(value.is_empty()
            || value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("off"))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_is_on_only_for_values_that_say_so() {
        for on in ["1", "true", "yes", "stages"] {
            assert!(switched_on(Some(OsStr::new(on))), "{on}");
        }
        for off in ["0", "false", "FALSE", "off", "", " "] {
            assert!(!switched_on(Some(OsStr::new(off))), "{off:?}");
        }
        assert!(!switched_on(None));
    }
}
