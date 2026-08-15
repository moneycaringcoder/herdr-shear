//! The interactive review pane.
//!
//! It runs in a herdr **overlay pane**, not a popup. The reason is written down
//! in the README, and it is this: the pane is a working surface. A user scans
//! forty rows, selects several, and then confirms something destructive. A popup
//! is dismissed by a stray key, and losing a selection to a mis-key is worse
//! than having to press `q`. An overlay pane survives the mis-key.
//!
//! Terminal discipline, which matters more here than anywhere else in the crate:
//! raw mode is entered once, restored from `Drop`, from a panic hook, and from
//! SIGINT/SIGTERM. A janitor that leaves somebody's pane in raw mode has done
//! more damage than the worktrees it removed.

use crate::config::Config;
use crate::model::Inventory;
use crate::Result;

/// Keys. Deliberately small, and deliberately not vim-only: `j`/`k` and the
/// arrow keys both move.
///
/// ```text
///   ↑ / k        previous row
///   ↓ / j        next row
///   space        toggle selection
///   a            select every `safe` row (and nothing else, ever)
///   n            clear the selection
///   r            remove the selection, after confirming
///   q / Esc      quit without removing anything
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Toggle,
    SelectSafe,
    SelectNone,
    Remove,
    Quit,
    Confirm,
    Cancel,
    Other,
}

/// The review pane's state. Pure: [`apply`] is a total function from state and
/// key to state, so `tests/render.rs` can drive an entire session — including
/// both confirmations for a dirty removal — without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub inventory: Inventory,
    pub cursor: usize,
    /// Indices into `inventory.candidates`.
    pub selected: std::collections::BTreeSet<usize>,
    pub mode: Mode,
    /// Messages from the last action, shown under the table.
    pub messages: Vec<String>,
}

/// What the pane is currently asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    /// First confirmation, for a selection of clean worktrees.
    ConfirmClean {
        count: usize,
        bytes: u64,
    },
    /// Second, differently-worded confirmation for a selection that includes
    /// dirty worktrees. It names the file count at risk and requires typing that
    /// number, not pressing `y` — a confirmation that can be given without
    /// reading it is not a confirmation.
    ConfirmDirty {
        files: usize,
        typed: String,
        worktrees: usize,
    },
    Removing,
    Done,
}

/// Applies one key. Never performs I/O; a transition into [`Mode::Removing`] is
/// what tells the driver to act.
pub fn apply(review: Review, key: Key) -> Review {
    let _ = (review, key);
    unimplemented!("interface: apply")
}

/// Renders the current state to a frame of exactly the given size.
pub fn frame(review: &Review, columns: usize, rows: usize) -> String {
    let _ = (review, columns, rows);
    unimplemented!("interface: frame")
}

/// `--review`: the interactive verb.
pub fn run_review(config: &Config) -> Result<()> {
    let _ = config;
    unimplemented!("interface: run_review")
}
