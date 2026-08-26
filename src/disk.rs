//! Disk sizing, measured lazily.
//!
//! Walking forty worktrees before drawing the first row feels broken, so a scan
//! leaves every [`Size`] as `Pending` and this module fills them in behind the
//! rendering.
//!
//! What is measured is space that would actually be **reclaimed**: bytes
//! occupied on disk (`st_blocks * 512` on unix, which is what `du` reports),
//! with hardlinked files counted once per `(dev, ino)`. Apparent size would
//! overstate a sparse file and understate the block padding on thousands of tiny
//! source files, and this number's whole job is to be the one the user checks
//! against `df`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::model::{Inventory, Size};

/// Measures one checkout. Symlinks are never followed — a worktree containing a
/// symlink to `/` must not report the size of the machine.
///
/// `cancel` is polled during the walk so a review pane that is being torn down
/// does not have to wait for a slow filesystem.
pub fn measure(path: &Path, cancel: &AtomicBool) -> Size {
    // `symlink_metadata` rather than `metadata`: the top of the walk is not
    // followed either, so a worktree path that is itself a symlink reports the
    // link, not its target.
    let top = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // A prunable worktree's directory is gone. That is a different claim
        // from "0 bytes" and the table says so.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Size::Gone,
        Err(_) => return Size::Failed,
    };

    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total: u64 = 0;
    if !count(&top, &mut seen, &mut total) {
        return Size::Failed;
    }
    if !top.is_dir() {
        return Size::Bytes(total);
    }

    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            // Not measured, and honestly so. `Failed` would say the walk was
            // tried and broke; it was not tried to the end.
            return Size::Pending;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished under us is a race, not a failure; a
            // directory we may not read means the total would be wrong, and a
            // wrong total is worse than no total.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Size::Failed,
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return Size::Failed,
            };
            // `DirEntry::metadata` does not traverse symlinks, which is exactly
            // what is wanted: a link costs its own inode, never its target.
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Size::Failed,
            };
            if !count(&meta, &mut seen, &mut total) {
                return Size::Failed;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Size::Bytes(total)
}

/// Adds one entry's occupied bytes, skipping any hardlink already counted.
/// Returns false when the total would overflow, which is a failure rather than
/// a wrapped number.
#[cfg(unix)]
fn count(meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>, total: &mut u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
        return true;
    }
    match total.checked_add(meta.blocks().saturating_mul(512)) {
        Some(sum) => {
            *total = sum;
            true
        }
        None => false,
    }
}

/// Apparent size, on platforms with no `st_blocks`. Documented as an
/// approximation rather than presented as the same number.
#[cfg(not(unix))]
fn count(meta: &std::fs::Metadata, _seen: &mut HashSet<(u64, u64)>, total: &mut u64) -> bool {
    match total.checked_add(meta.len()) {
        Some(sum) => {
            *total = sum;
            true
        }
        None => false,
    }
}

/// Measures every candidate in the inventory, in parallel, and writes the
/// results back. Used by `--list` and `--json`, which have no incremental
/// rendering to keep responsive.
pub fn measure_all(inventory: &mut Inventory, cancel: &AtomicBool) {
    let paths: Vec<PathBuf> = inventory
        .candidates
        .iter()
        .map(|candidate| candidate.worktree.path.clone())
        .collect();
    if paths.is_empty() {
        return;
    }

    let sizes: Vec<Mutex<Size>> = paths.iter().map(|_| Mutex::new(Size::Pending)).collect();
    let next = AtomicUsize::new(0);
    // The work is filesystem-bound and wildly uneven — one worktree can hold a
    // `node_modules` and the next a single file — so threads pull from a shared
    // cursor rather than taking a fixed slice each.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
        .min(paths.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= paths.len() || cancel.load(Ordering::Relaxed) {
                    return;
                }
                let size = measure(&paths[index], cancel);
                *sizes[index]
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = size;
            });
        }
    });

    for (candidate, size) in inventory.candidates.iter_mut().zip(sizes) {
        candidate.size = size.into_inner().unwrap_or_else(|p| p.into_inner());
    }
}

// ---------------------------------------------------------------------------
// The size cache: last run's figures, for the provisional first frame
// ---------------------------------------------------------------------------

/// One remembered measurement. `at` is unix seconds, recorded so a future
/// reader can weigh how old a figure is; nothing renders it today.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Remembered {
    path: PathBuf,
    bytes: u64,
    at: u64,
}

/// Marks every pending row whose path has a remembered figure as
/// [`Size::Provisional`]. The walk still runs for those rows — a provisional
/// figure is drawn, never trusted — and rows the cache does not know stay
/// `Pending`. A cache that cannot be read is an absent cache, not an error:
/// the pane simply opens the way it always has.
pub fn recall(inventory: &mut Inventory, cache: &Path) {
    let Ok(raw) = std::fs::read_to_string(cache) else {
        return;
    };
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Remembered>(line) else {
            // A corrupt line loses one figure, never the file.
            continue;
        };
        for candidate in inventory.candidates.iter_mut() {
            if candidate.size == Size::Pending && candidate.worktree.path == entry.path {
                candidate.size = Size::Provisional(entry.bytes);
            }
        }
    }
}

/// Writes what this run measured, merged with remembered figures for paths it
/// did not measure — checkouts another `--repo` scans, and rows whose walk
/// never finished, which must not lose their figure to a quick open-and-quit.
/// An entry whose path no longer exists is dropped, which is what keeps the
/// file from growing forever. Written whole and atomically; a failure to write
/// costs the next run its provisional figures and nothing else, so it is not
/// an error.
pub fn remember(inventory: &Inventory, cache: &Path) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut entries: Vec<Remembered> = Vec::new();
    for candidate in &inventory.candidates {
        if let Size::Bytes(bytes) = candidate.size {
            entries.push(Remembered {
                path: candidate.worktree.path.clone(),
                bytes,
                at: now,
            });
        }
    }

    if let Ok(raw) = std::fs::read_to_string(cache) {
        for line in raw.lines() {
            let Ok(entry) = serde_json::from_str::<Remembered>(line) else {
                continue;
            };
            let measured_this_run = inventory.candidates.iter().any(|candidate| {
                matches!(candidate.size, Size::Bytes(_)) && candidate.worktree.path == entry.path
            });
            // `symlink_metadata`, not `exists`: a checkout behind a dead
            // symlink still occupies whatever it occupies.
            if !measured_this_run && std::fs::symlink_metadata(&entry.path).is_ok() {
                entries.push(entry);
            }
        }
    }

    let mut body = String::new();
    for entry in &entries {
        let Ok(line) = serde_json::to_string(entry) else {
            continue;
        };
        body.push_str(&line);
        body.push('\n');
    }

    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic: a pane torn down mid-write must not leave half a file for the
    // next open to trip over.
    let tmp = cache.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, cache);
    }
}

/// Human-readable size for the table: `1.2 GB`, `340 MB`, `12 kB`, `~1.2 GB`
/// for a previous run's figure while the walk re-measures, `-` for a path that
/// is gone, `…` while pending, `?` when the walk failed.
///
/// Units are powers of 1024 with SI-style suffixes, matching what `du -h`
/// prints, because that is the command a user will check this against.
pub fn human(size: Size) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];

    let bytes = match size {
        Size::Bytes(bytes) => bytes,
        Size::Gone => return "-".to_string(),
        // Marked, because it is a claim about last time: the walk replaces it.
        Size::Provisional(bytes) => return format!("~{}", human(Size::Bytes(bytes))),
        Size::Pending => return "…".to_string(),
        Size::Failed => return "?".to_string(),
    };

    let mut unit = 0usize;
    let mut divisor = 1u64;
    while unit + 1 < UNITS.len() && bytes / divisor >= 1024 {
        divisor *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }

    // `du -h` rounds *up*, and shows one decimal only while the scaled value is
    // below ten: 1536 is `1.5K`, 12000 is `12K`, 10240 is `10K`. Done in
    // integers so the boundaries land where du's do.
    loop {
        let tenths = bytes.saturating_mul(10).div_ceil(divisor);
        if tenths < 100 {
            return format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit]);
        }
        let whole = bytes.div_ceil(divisor);
        // Rounding up can push a value onto the next unit (1048575 B is not
        // "1024 kB"), which is where du rescales too.
        if whole >= 1024 && unit + 1 < UNITS.len() {
            divisor *= 1024;
            unit += 1;
            continue;
        }
        return format!("{whole} {}", UNITS[unit]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundaries are `du -h`'s, checked against real `du` output on the
    /// same byte counts: 1536 is `1.5K`, 12000 is `12K`, 10240 is `10K`. It
    /// rounds *up*, and shows a decimal only while the scaled value is under
    /// ten. A user who checks the number against `du` has to see the same one.
    #[test]
    fn human_rounds_the_way_du_does() {
        assert_eq!(human(Size::Bytes(0)), "0 B");
        assert_eq!(human(Size::Bytes(1)), "1 B");
        assert_eq!(human(Size::Bytes(1023)), "1023 B");
        assert_eq!(human(Size::Bytes(1024)), "1.0 kB");
        assert_eq!(human(Size::Bytes(1536)), "1.5 kB");
        // Rounds up, not to nearest: 1025/1024 is 1.0009, and du prints 1.1K.
        assert_eq!(human(Size::Bytes(1025)), "1.1 kB");
        assert_eq!(human(Size::Bytes(10 * 1024)), "10 kB");
        assert_eq!(human(Size::Bytes(12_000)), "12 kB");
        assert_eq!(human(Size::Bytes(1024 * 1024)), "1.0 MB");
        assert_eq!(human(Size::Bytes(340 * 1024 * 1024)), "340 MB");
        assert_eq!(human(Size::Bytes(1024 * 1024 * 1024)), "1.0 GB");
        assert_eq!(human(Size::Bytes(1024 * 1024 * 1024 + 1)), "1.1 GB");
        // The scale runs out at PB rather than wrapping or panicking; a number
        // that large is wrong for other reasons, but it still has to render.
        assert!(human(Size::Bytes(u64::MAX)).ends_with(" PB"));
    }

    /// Rounding up must not print a value that belongs in the next unit — up to
    /// the largest unit there is, where there is nowhere left to carry to.
    #[test]
    fn rounding_up_never_prints_1024_of_a_unit() {
        for bytes in [
            1024u64 - 1,
            1024 * 1024 - 1,
            1024 * 1024 * 1024 - 1,
            1024u64.pow(4) - 1,
            1024u64.pow(5) - 1,
        ] {
            let rendered = human(Size::Bytes(bytes));
            let number = rendered.split(' ').next().expect("a number");
            assert!(
                number.parse::<f64>().expect("a number") < 1024.0,
                "{bytes} rendered as {rendered}"
            );
        }
    }

    /// The three non-measurements are three different claims, and none of them
    /// is a zero. A prunable worktree reclaiming nothing and a walk that failed
    /// must not both read as `0 B`.
    #[test]
    fn the_non_measurements_are_not_zeroes() {
        assert_eq!(human(Size::Gone), "-");
        assert_eq!(human(Size::Pending), "…");
        assert_eq!(human(Size::Failed), "?");
        assert_ne!(human(Size::Gone), human(Size::Bytes(0)));
        assert_ne!(human(Size::Failed), human(Size::Bytes(0)));
    }

    /// A provisional figure is marked, and marked the same at every scale: it
    /// is last run's claim, and the tilde is what keeps it from being read as
    /// a measurement.
    #[test]
    fn a_provisional_figure_is_marked() {
        assert_eq!(human(Size::Provisional(340 * 1024 * 1024)), "~340 MB");
        assert_eq!(human(Size::Provisional(0)), "~0 B");
        assert_ne!(
            human(Size::Provisional(1024)),
            human(Size::Bytes(1024)),
            "a claim and a measurement must never render alike"
        );
    }
}
