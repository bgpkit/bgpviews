//! The one-writer claim on an archive root.
//!
//! Two `run` processes on one root would take the same epoch, open the same journal with
//! `truncate(true)` and race scan-based ordinal allocation, so the second writer is refused
//! instead of allowed to overwrite the first one's frames.
//!
//! The claim is an OS lock (`flock`) on `archive/.lock`, and the file's content says who
//! holds it. Design section 5 asks for a claim whose holder is gone to be reported rather
//! than silently broken: this does both. The kernel releases the lock when the process dies,
//! so a claim left by a crash is provably unheld and is reported on the next start; a claim
//! whose holder is alive cannot be taken, whatever the file says.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// The claim file inside the archive root, named by design section 5.
pub const LOCK_NAME: &str = ".lock";

/// An exclusive claim on one archive root, released when this value is dropped.
#[derive(Debug)]
pub struct ArchiveLock {
    /// Dropping this unlocks: the kernel owns the claim, not a file's existence.
    file: Flock<std::fs::File>,
    /// What the file said before this process took the claim, when it said anything.
    previous: Option<String>,
}

/// Why a claim could not be taken.
#[derive(Debug)]
pub enum AcquireError {
    /// Another process holds the root; the claim carries whatever it recorded.
    Conflict {
        /// The claim file.
        path: PathBuf,
        /// The holder's own description of itself.
        holder: String,
    },
    /// The root or the claim file could not be written.
    Io(String),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { path, holder } => write!(
                f,
                "writer_conflict: {} is held by {holder}; one archive root has one writer, so \
                 stop that process before starting another",
                path.display()
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AcquireError {}

impl ArchiveLock {
    /// Claim `root` for `command`, or report who holds it.
    ///
    /// # Errors
    /// [`AcquireError::Conflict`] when a live process holds the root, and [`AcquireError::Io`]
    /// when the root or the claim cannot be written.
    pub fn acquire(root: &Path, command: &str) -> Result<Self, AcquireError> {
        std::fs::create_dir_all(root)
            .map_err(|e| AcquireError::Io(format!("cannot create {}: {e}", root.display())))?;
        let path = root.join(LOCK_NAME);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| AcquireError::Io(format!("cannot open {}: {e}", path.display())))?;
        let file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(locked) => locked,
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                let holder = std::fs::read_to_string(&path)
                    .map(|text| text.trim().to_string())
                    .unwrap_or_else(|_| "an unreadable claim".to_string());
                let holder = if holder.is_empty() {
                    "a process that did not record itself".to_string()
                } else {
                    holder
                };
                return Err(AcquireError::Conflict { path, holder });
            }
            Err((_, e)) => {
                return Err(AcquireError::Io(format!(
                    "cannot lock {}: {e}",
                    path.display()
                )))
            }
        };

        let previous = std::fs::read_to_string(&path)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        let described = format!(
            "{command} pid {} since {}",
            std::process::id(),
            unix_seconds()
        );
        let mut file = file;
        let recorded = file
            .set_len(0)
            .and_then(|()| file.write_all(format!("{described}\n").as_bytes()))
            .and_then(|()| file.sync_all());
        if let Err(e) = recorded {
            return Err(AcquireError::Io(format!(
                "cannot record the claim {}: {e}",
                path.display()
            )));
        }
        Ok(Self { file, previous })
    }

    /// What the file said before this process took the claim, if anything.
    ///
    /// A stale entry means the previous holder died without releasing it: the kernel released
    /// the lock, so the archive is safe to extend, and the operator should be told.
    #[must_use]
    pub fn previous_holder(&self) -> Option<&str> {
        self.previous.as_deref()
    }
}

impl Drop for ArchiveLock {
    fn drop(&mut self) {
        // Record the release while the lock is still held, then let the kernel drop it. A
        // clean exit therefore leaves an empty file rather than a claim that looks live.
        let _ = self.file.set_len(0);
        let _ = self.file.sync_all();
        // The flock itself is released when the field is dropped, which is why a killed
        // process cannot leave a claim that lies about being held.
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bgpviews-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_second_writer_is_refused_and_named() -> Result<(), Box<dyn std::error::Error>> {
        let root = scratch("second");
        let first = ArchiveLock::acquire(&root, "run")?;
        assert!(root.join(LOCK_NAME).exists());
        let refused = ArchiveLock::acquire(&root, "rebuild-index");
        if let Err(AcquireError::Conflict { holder, .. }) = &refused {
            assert!(holder.contains("run"), "holder was {holder:?}");
            assert!(holder.contains(&std::process::id().to_string()));
        } else {
            return Err(format!("expected a conflict, got {refused:?}").into());
        }
        // The refused attempt must not have touched the holder's claim.
        let held = std::fs::read_to_string(root.join(LOCK_NAME))?;
        assert!(held.contains("run pid"));
        drop(first);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn a_released_claim_can_be_taken_again() -> Result<(), Box<dyn std::error::Error>> {
        let root = scratch("released");
        drop(ArchiveLock::acquire(&root, "run")?);
        let again = ArchiveLock::acquire(&root, "run")?;
        assert_eq!(
            again.previous_holder(),
            None,
            "a clean release says nothing"
        );
        drop(again);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn a_claim_left_by_a_dead_holder_is_reported_and_reused(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = scratch("stale");
        std::fs::create_dir_all(&root)?;
        // A file with a holder in it and no live lock: exactly what SIGKILL leaves behind.
        std::fs::write(root.join(LOCK_NAME), "run pid 999999 since 1\n")?;
        let taken = ArchiveLock::acquire(&root, "run")?;
        assert_eq!(taken.previous_holder(), Some("run pid 999999 since 1"));
        let now = std::fs::read_to_string(root.join(LOCK_NAME))?;
        assert!(now.contains(&std::process::id().to_string()));
        drop(taken);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
