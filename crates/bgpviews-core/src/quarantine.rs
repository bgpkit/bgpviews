//! Quarantine: withholding an artifact without pretending it never existed.
//!
//! Design reference: `docs/design.md` section 7. When a segment fails its own hash, the
//! archive has two bad options and one good one.
//!
//! Bad option one: delete it. The coverage log still says a segment covers that interval,
//! so the archive would claim records it no longer holds.
//!
//! Bad option two: leave it in place. Every reader that trusts the sidecar now reads bytes
//! that are not the bytes it names.
//!
//! The good option is to say so: the segment becomes *withheld*, which is a terminal state
//! the archive can represent. The order of the three steps is what makes it crash-safe.
//!
//! 1. **Write the tombstone first.** It is the statement that this artifact is withheld, and
//!    it is durable before anything moves. If the process dies now, the archive has a
//!    segment that is still in place but already declared withheld, which is recoverable.
//! 2. **Move the artifact and its sidecar** into `quarantine/`, so no reader stumbles on
//!    them, and so a human can still inspect the bytes.
//! 3. **Nothing else.** The coverage log is not rewritten: it records what was published,
//!    and a tombstone is where the later judgement belongs.
//!
//! `resume` finishes step 2 for tombstones whose artifact is still where it was, which is
//! the only state an interrupted quarantine can leave.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::segment::Sidecar;

/// The durable record that an artifact was withheld.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    /// Always 1 for this layout.
    pub schema_version: u32,
    /// The collector the artifact belonged to.
    pub collector: String,
    /// Path the artifact had, relative to the archive root.
    pub path: String,
    /// The SHA-256 the sidecar claimed, so a reader can see what was expected.
    pub expected_sha256: String,
    /// Why it was withheld.
    pub reason: String,
    /// `withheld`; the terminal state, also recorded in the withheld sidecar.
    pub state: String,
    /// When the decision was made, seconds since the epoch.
    pub quarantined_at: i64,
}

/// What a quarantine did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// The tombstone written.
    pub tombstone: Option<PathBuf>,
    /// Files moved into `quarantine/`.
    pub moved: Vec<PathBuf>,
}

/// Withhold one published artifact.
///
/// # Errors
/// Returns a message when the sidecar cannot be read or a file cannot be moved. A failure
/// before the tombstone is written changes nothing; a failure after it leaves a state
/// `resume` can finish.
pub fn quarantine(root: &Path, artifact: &Path, reason: &str, at: i64) -> Result<Report, String> {
    // The path has to land inside the archive, and a relative one may be written either from
    // the archive root or from the working directory. A tombstone naming a path outside the
    // root is one `resume` and `withheld_reason` never resolve, so a foreign path is refused
    // rather than stored.
    let relative = |candidate: &Path| candidate.strip_prefix(root).ok().map(Path::to_path_buf);
    let artifact = if artifact.is_absolute() || relative(artifact).is_some() {
        artifact.to_path_buf()
    } else {
        root.join(artifact)
    };
    let relative = relative(&artifact).ok_or_else(|| {
        format!(
            "{} is not inside the archive root {}",
            artifact.display(),
            root.display()
        )
    })?;
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "{} escapes the archive root {}",
            artifact.display(),
            root.display()
        ));
    }
    let sidecar_path = PathBuf::from(format!("{}.json", artifact.display()));
    let text = std::fs::read_to_string(&sidecar_path)
        .map_err(|e| format!("{}: {e}", sidecar_path.display()))?;
    let mut sidecar: Sidecar =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", sidecar_path.display()))?;
    let relative = relative.display().to_string();

    let tombstone = write_tombstone(
        root,
        &Tombstone {
            schema_version: 1,
            collector: sidecar.collector.clone(),
            path: relative.clone(),
            expected_sha256: sidecar.sha256.clone(),
            reason: reason.to_string(),
            state: "withheld".to_string(),
            quarantined_at: at,
        },
    )?;
    let mut report = Report {
        tombstone: Some(tombstone),
        moved: Vec::new(),
    };

    // The sidecar that travels with the artifact says withheld too, so the pair is
    // consistent wherever a human finds it.
    sidecar.state = "withheld".to_string();
    if !sidecar.partial_reasons.iter().any(|r| r == reason) {
        sidecar.partial_reasons.push(reason.to_string());
    }
    let quarantine_dir = root.join("quarantine").join(&sidecar.collector);
    std::fs::create_dir_all(&quarantine_dir)
        .map_err(|e| format!("{}: {e}", quarantine_dir.display()))?;

    for source in [artifact.to_path_buf(), sidecar_path] {
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let target = quarantine_dir.join(&name);
        std::fs::rename(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
        report.moved.push(target);
    }
    // Only now is the withheld sidecar's content written, in the quarantine directory:
    // rewriting the original before moving it would leave the archive describing bytes it
    // is about to hide.
    let moved_sidecar = quarantine_dir.join(
        PathBuf::from(&relative)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            + ".json",
    );
    if moved_sidecar.exists() {
        let json = serde_json::to_string_pretty(&sidecar).map_err(|e| e.to_string())?;
        std::fs::write(&moved_sidecar, json + "\n")
            .map_err(|e| format!("{}: {e}", moved_sidecar.display()))?;
    }
    Ok(report)
}

/// Finish a quarantine that was interrupted after its tombstone was written.
///
/// Returns the artifacts that were moved.
///
/// # Errors
/// Returns a message when a tombstone cannot be read or a file cannot be moved.
pub fn resume(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut moved = Vec::new();
    for path in tombstones(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(tombstone) = serde_json::from_str::<Tombstone>(&text) else {
            continue;
        };
        let artifact = root.join(&tombstone.path);
        if !artifact.exists() {
            // Already moved: the tombstone is the whole record now.
            continue;
        }
        let sidecar_path = PathBuf::from(format!("{}.json", artifact.display()));
        let quarantine_dir = root.join("quarantine").join(&tombstone.collector);
        std::fs::create_dir_all(&quarantine_dir)
            .map_err(|e| format!("{}: {e}", quarantine_dir.display()))?;
        for source in [artifact, sidecar_path] {
            if !source.exists() {
                continue;
            }
            // A sidecar in flight still says `complete` or `partial`. `verify` reads it inside
            // `quarantine/` and hashes the artifact against it, and a quarantined artifact is by
            // definition the one whose bytes did not match: leaving the old state makes every
            // later `verify` report `corrupt` with no way to clear it.
            if source.extension().is_some_and(|ext| ext == "json") {
                if let Ok(text) = std::fs::read_to_string(&source) {
                    if let Ok(mut sidecar) = serde_json::from_str::<crate::segment::Sidecar>(&text)
                    {
                        sidecar.state = "withheld".to_string();
                        if let Ok(json) = serde_json::to_string_pretty(&sidecar) {
                            let _ = std::fs::write(&source, json + "\n");
                        }
                    }
                }
            }
            let name = source
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let target = quarantine_dir.join(&name);
            std::fs::rename(&source, &target).map_err(|e| format!("{}: {e}", source.display()))?;
            moved.push(target);
        }
    }
    Ok(moved)
}

/// Whether `relative_path` has been withheld, and why.
#[must_use]
pub fn withheld_reason(root: &Path, relative_path: &str) -> Option<String> {
    for path in tombstones(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(tombstone) = serde_json::from_str::<Tombstone>(&text) else {
            continue;
        };
        if tombstone.path == relative_path {
            return Some(tombstone.reason);
        }
    }
    None
}

fn write_tombstone(root: &Path, tombstone: &Tombstone) -> Result<PathBuf, String> {
    let dir = root.join("tombstones").join(&tombstone.collector);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = tombstone
        .path
        .replace('/', "_")
        .trim_end_matches(".gz")
        .to_string()
        + ".json";
    let path = dir.join(name);
    let json = serde_json::to_string_pretty(tombstone).map_err(|e| e.to_string())?;
    let mut file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    use std::io::Write;
    file.write_all(json.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

fn tombstones(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("tombstones")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn archive(dir: &Path) -> (PathBuf, PathBuf) {
        let collector = dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES");
        std::fs::create_dir_all(&collector).unwrap();
        let artifact = collector.join("updates.20261115.1800.gz");
        std::fs::write(&artifact, b"published bytes").unwrap();
        let sidecar = Sidecar {
            schema_version: 1,
            collector: "r2-adj-in-pre".to_string(),
            client: "r2".to_string(),
            view: "adj-in-pre".to_string(),
            feed: "bmp".to_string(),
            stream: crate::segment::Stream::Updates,
            range_start: "1763166000.000000".to_string(),
            range_end: "1763166899.000000".to_string(),
            session_epoch: 1,
            ordinal: 1,
            path: "r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz".to_string(),
            bytes: 15,
            records: 1,
            end_of_rib_records: 0,
            compression: "gzip".to_string(),
            sha256: "expected-digest".to_string(),
            state: "complete".to_string(),
            partial_reasons: Vec::new(),
            late_arrival: 0,
            exporter_sys_name: None,
            exporter_sys_descr: None,
            first_router_time: None,
            last_router_time: None,
            baseline: Vec::new(),
        };
        std::fs::write(
            format!("{}.json", artifact.display()),
            serde_json::to_string(&sidecar).unwrap(),
        )
        .unwrap();
        (artifact, sidecar.path.into())
    }

    #[test]
    fn a_quarantined_segment_leaves_a_tombstone_and_moves_its_bytes() {
        let dir = std::env::temp_dir().join(format!("bgpviews-quarantine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (artifact, _) = archive(&dir);

        let report = quarantine(&dir, &artifact, "sha256_mismatch", 1_763_166_900).unwrap();
        assert!(report.tombstone.is_some());
        assert_eq!(report.moved.len(), 2, "artifact and sidecar both move");
        assert!(!artifact.exists(), "the published path is now empty");
        assert!(dir
            .join("quarantine/r2-adj-in-pre/updates.20261115.1800.gz")
            .exists());

        let reason = withheld_reason(
            &dir,
            "r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz",
        );
        assert_eq!(reason.as_deref(), Some("sha256_mismatch"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relative_path_is_taken_against_the_root_and_recorded_relative() {
        let dir =
            std::env::temp_dir().join(format!("bgpviews-quarantine-rel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (artifact, _) = archive(&dir);
        let relative = artifact.strip_prefix(&dir).unwrap().to_path_buf();

        let report = quarantine(&dir, &relative, "parse_error", 1_763_166_900).unwrap();
        assert_eq!(report.moved.len(), 2);
        // The tombstone has to name the artifact the way the rest of the archive does, or
        // `withheld_reason` looks for a path that never existed.
        let reason = withheld_reason(
            &dir,
            "r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz",
        );
        assert_eq!(reason.as_deref(), Some("parse_error"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_outside_the_root_is_refused_and_nothing_moves() {
        let dir =
            std::env::temp_dir().join(format!("bgpviews-quarantine-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (artifact, _) = archive(&dir);

        let outside = std::env::temp_dir().join(format!(
            "bgpviews-quarantine-elsewhere-{}.gz",
            std::process::id()
        ));
        std::fs::write(&outside, b"x").unwrap();
        let refused = quarantine(&dir, &outside, "manual", 1);
        assert!(refused.is_err(), "a foreign path must be refused");
        assert!(artifact.exists(), "the published artifact is untouched");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resumed_quarantine_moves_what_is_still_in_place() {
        let dir =
            std::env::temp_dir().join(format!("bgpviews-quarantine-r-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (artifact, _) = archive(&dir);

        // Simulate a crash after the tombstone: write it, move nothing.
        write_tombstone(
            &dir,
            &Tombstone {
                schema_version: 1,
                collector: "r2-adj-in-pre".to_string(),
                path: "r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz".to_string(),
                expected_sha256: "expected-digest".to_string(),
                reason: "sha256_mismatch".to_string(),
                state: "withheld".to_string(),
                quarantined_at: 1_763_166_900,
            },
        )
        .unwrap();
        assert!(artifact.exists());

        let moved = resume(&dir).unwrap();
        assert_eq!(moved.len(), 2);
        assert!(!artifact.exists());
        // The sidecar arrives in quarantine/ marked withheld, or `verify` hashes the artifact
        // against a `complete` claim it cannot meet and reports `corrupt` forever.
        let moved_sidecar = moved
            .iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .expect("the sidecar moved");
        let sidecar: crate::segment::Sidecar =
            serde_json::from_str(&std::fs::read_to_string(moved_sidecar).unwrap()).unwrap();
        assert_eq!(sidecar.state, "withheld");
        // A second resume is a no-op, which is what makes it safe to run at every start.
        assert!(resume(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
