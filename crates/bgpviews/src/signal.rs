//! Signal handling: SIGHUP means "snapshot the RIB and re-read the configuration".
//!
//! The handler does nothing but set a flag. Everything that has to happen - writing a
//! file, parsing a config, closing a session - happens on the collector's own thread,
//! because a signal handler that calls into the allocator or the filesystem is a data
//! race with whatever the main thread was doing.
//!
//! The flag is checked where the collector already has a reason to pause: on a read
//! timeout between frames, and between sessions in the accept loop. That is why
//! `pump_with_tick` exists.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A flag that records that SIGHUP arrived.
#[derive(Clone, Debug, Default)]
pub struct HupFlag {
    raised: Arc<AtomicBool>,
}

impl HupFlag {
    /// A flag that has not been raised.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the signal handler to set this flag on SIGHUP.
    ///
    /// # Errors
    /// Returns the OS error when the handler cannot be installed, which happens on a
    /// platform without `signal-hook` support or when too many handlers are registered.
    pub fn install(&self) -> std::io::Result<()> {
        signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&self.raised))
            .map(|_| ())
    }

    /// Take the flag: true once per SIGHUP received.
    pub fn take(&self) -> bool {
        self.raised.swap(false, Ordering::SeqCst)
    }
}

/// The process-wide flag, installed once.
///
/// One flag for the process, not one per session: SIGHUP can arrive between sessions,
/// when there is no collector to snapshot, and the accept loop still has to survive it.
pub fn global() -> &'static HupFlag {
    static FLAG: std::sync::OnceLock<HupFlag> = std::sync::OnceLock::new();
    FLAG.get_or_init(|| {
        let flag = HupFlag::new();
        if let Err(e) = flag.install() {
            eprintln!("bgpviews: cannot install SIGHUP handler: {e}");
        }
        flag
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_raised_signal_sets_the_flag_exactly_once() {
        let flag = HupFlag::new();
        flag.install().unwrap();
        assert!(!flag.take());
        signal_hook::low_level::raise(signal_hook::consts::SIGHUP).unwrap();
        // The handler runs asynchronously; wait for it rather than assuming order.
        let mut seen = false;
        for _ in 0..100 {
            if flag.take() {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(seen, "SIGHUP must set the flag");
        assert!(!flag.take(), "the signal is consumed once");
    }

    #[test]
    fn repeated_signals_collapse_into_one_request() {
        let flag = HupFlag::new();
        flag.install().unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGHUP).unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGHUP).unwrap();
        let mut seen = false;
        for _ in 0..100 {
            if flag.take() {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Two HUPs that arrive before the collector looks produce one snapshot. That is
        // the documented trade: a snapshot is a function of the RIB, not of the signal,
        // so a second one would write the same bytes.
        assert!(seen, "a signal must reach the flag");
        assert!(!flag.take(), "the request is consumed once");
    }
}
