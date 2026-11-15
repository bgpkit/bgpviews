//! Disk guards: what the collector gives up first when the archive's own disk fills.
//!
//! Design reference: `docs/design.md` section 13. The ordering is a judgement about what
//! can be rebuilt:
//!
//! - **warning**: the derived view goes first. A RIB snapshot is a projection of the
//!   update stream, so skipping one costs time, not evidence. Updates keep arriving.
//! - **critical**: the session closes. Nothing else can be given up: the record itself is
//!   what this program exists to write, and writing half of it into a full disk produces
//!   neither a record nor an honest gap.
//!
//! The percentages are read from the filesystem the archive lives on. A test can force a
//! percentage through `BGPVIEWS_DISK_USED_PERCENT`, because filling a real disk to 97 % in
//! CI is not a test, it is a hazard.

use std::path::Path;

/// How full the archive's filesystem is, in the terms the collector acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Below the warning threshold.
    Ok,
    /// Above the warning, below critical: write updates, skip RIBs.
    Warning,
    /// At or above critical: close the session.
    Critical,
}

impl Level {
    /// Classify a used percentage.
    #[must_use]
    pub fn from_percent(used: u8, warning: u8, critical: u8) -> Self {
        if used >= critical {
            Self::Critical
        } else if used >= warning {
            Self::Warning
        } else {
            Self::Ok
        }
    }

    /// Whether RIB snapshots should be skipped.
    #[must_use]
    pub fn skip_ribs(self) -> bool {
        self >= Self::Warning
    }

    /// Whether the session must close.
    #[must_use]
    pub fn close_session(self) -> bool {
        self == Self::Critical
    }
}

/// The used percentage of the filesystem holding `path`.
///
/// Returns `None` when it cannot be measured, which is treated as `Ok` by the caller: a
/// guard that closes a session because it cannot read a number would turn a missing
/// syscall into an outage.
#[must_use]
pub fn used_percent(path: &Path) -> Option<u8> {
    // A test seam, not a configuration knob: the name says so, and nothing reads it when
    // the guard is measured from the filesystem.
    if let Ok(forced) = std::env::var("BGPVIEWS_DISK_USED_PERCENT") {
        return forced.parse::<u8>().ok();
    }
    // `nix` is a safe wrapper around statvfs: this crate forbids `unsafe`, and measuring
    // a filesystem is not a good reason to make the first exception.
    let stats = nix::sys::statvfs::statvfs(path).ok()?;
    let total = stats.blocks();
    let free = stats.blocks_available();
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(free);
    // Percentages are computed in u128 because a large filesystem overflows u64 when
    // multiplied by 100.
    let percent = u128::from(used) * 100 / u128::from(total);
    u8::try_from(percent).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_thresholds_order_what_to_give_up() {
        assert_eq!(Level::from_percent(10, 90, 97), Level::Ok);
        assert_eq!(Level::from_percent(90, 90, 97), Level::Warning);
        assert_eq!(Level::from_percent(96, 90, 97), Level::Warning);
        assert_eq!(Level::from_percent(97, 90, 97), Level::Critical);
        assert!(!Level::Ok.skip_ribs());
        assert!(Level::Warning.skip_ribs());
        assert!(!Level::Warning.close_session());
        assert!(Level::Critical.close_session());
    }

    #[test]
    fn a_real_filesystem_reports_a_percentage() {
        let percent = used_percent(Path::new("/")).expect("the root filesystem is measurable");
        assert!(percent <= 100, "a used percentage cannot exceed 100");
    }
}
