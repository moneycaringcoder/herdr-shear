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

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::model::{Inventory, Size};

/// Measures one checkout. Symlinks are never followed — a worktree containing a
/// symlink to `/` must not report the size of the machine.
///
/// `cancel` is polled during the walk so a review pane that is being torn down
/// does not have to wait for a slow filesystem.
pub fn measure(path: &Path, cancel: &AtomicBool) -> Size {
    let _ = (path, cancel);
    unimplemented!("classifier: measure")
}

/// Measures every candidate in the inventory, in parallel, and writes the
/// results back. Used by `--list` and `--json`, which have no incremental
/// rendering to keep responsive.
pub fn measure_all(inventory: &mut Inventory, cancel: &AtomicBool) {
    let _ = (inventory, cancel);
    unimplemented!("classifier: measure_all")
}

/// Human-readable size for the table: `1.2 GB`, `340 MB`, `12 kB`, `-` for a
/// path that is gone, `…` while pending, `?` when the walk failed.
///
/// Units are powers of 1024 with SI-style suffixes, matching what `du -h`
/// prints, because that is the command a user will check this against.
pub fn human(size: Size) -> String {
    let _ = size;
    unimplemented!("classifier: human")
}
