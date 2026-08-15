//! Rendering. Pure functions from an [`Inventory`] to text.
//!
//! Nothing here does I/O except [`run_list`], which is the `--list` verb. That
//! split is what lets `tests/render.rs` pin the table's alignment and truncation
//! without a repository or a session.
//!
//! Cosmetic output is a feature here, not polish. A janitor is judged on whether
//! its columns line up and its paths are readable at 80 columns, because that is
//! the whole interface.

use crate::config::Config;
use crate::model::{Candidate, Inventory};
use crate::Result;

/// Width assumed when the real terminal width is unknown.
pub const DEFAULT_COLUMNS: usize = 100;
/// Below this the table stops trying to stay pretty, but it still never emits a
/// line wider than the width it was given.
pub const MIN_COLUMNS: usize = 40;

/// The full review table, grouped by repository.
///
/// Columns: selection marker, verdict, classes, age, disk, branch, and the
/// worktree path. Paths truncate from the **left**, because the tail is the
/// informative half; branches and labels truncate from the right.
pub fn table(inventory: &Inventory, columns: usize) -> String {
    let _ = (inventory, columns);
    unimplemented!("interface: table")
}

/// One row of the table, at the column widths the table computed.
pub fn row(candidate: &Candidate, widths: &Widths, selected: bool) -> String {
    let _ = (candidate, widths, selected);
    unimplemented!("interface: row")
}

/// Column widths, computed from the content so a session of short paths does not
/// get a table sized for long ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Widths {
    pub verdict: usize,
    pub classes: usize,
    pub age: usize,
    pub size: usize,
    pub branch: usize,
    pub path: usize,
}

/// Widths for one inventory at a given terminal width.
pub fn widths_for(inventory: &Inventory, columns: usize) -> Widths {
    let _ = (inventory, columns);
    unimplemented!("interface: widths_for")
}

/// The summary line under the table: how many worktrees, how many safe, and what
/// the total reclaimable space is.
///
/// Both halves of the disk question are answered — per row and as a total —
/// because the per-row number is what justifies a particular pick and the total
/// is what makes anyone bother.
pub fn summary(inventory: &Inventory, columns: usize) -> String {
    let _ = (inventory, columns);
    unimplemented!("interface: summary")
}

/// Compact age, never wider than four display columns: `3d`, `12d`, `4w`, `7mo`,
/// `2y`, `-` when there is no commit to date.
pub fn human_age(age: Option<std::time::Duration>) -> String {
    let _ = age;
    unimplemented!("interface: human_age")
}

/// Width of `text` in terminal display columns. Hand-rolled because the crate
/// takes no width dependency.
pub fn display_width(text: &str) -> usize {
    let _ = text;
    unimplemented!("interface: display_width")
}

/// Trims to `max` display columns, dropping characters from the LEFT and marking
/// the cut with `…`. For paths.
pub fn truncate_left(text: &str, max: usize) -> String {
    let _ = (text, max);
    unimplemented!("interface: truncate_left")
}

/// Trims to `max` display columns from the right. For labels and headings.
pub fn truncate_right(text: &str, max: usize) -> String {
    let _ = (text, max);
    unimplemented!("interface: truncate_right")
}

/// `--list`: scan, size, print the table, exit. A dry run by construction —
/// this verb has no path to `remove`.
pub fn run_list(config: &Config) -> Result<()> {
    let _ = config;
    unimplemented!("interface: run_list")
}
