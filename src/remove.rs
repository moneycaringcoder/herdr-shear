//! Removal, and the guard rails that are the actual product.
//!
//! This is the only module in the crate that may change anything. Every path
//! into it is an explicit selection; nothing here is ever reached by a scan.
//!
//! The rules, each of which needs a test that proves it *refuses*:
//!
//! 1. The main checkout is never removable. No override exists.
//! 2. A locked worktree is never removable. The user must `git worktree unlock`
//!    it themselves — shear will not unlock on their behalf, because the lock is
//!    somebody's explicit "do not touch this".
//! 3. A worktree open in a herdr workspace is removable only with
//!    [`Permissions::close_workspace`], and then only through
//!    [`RemovalRoute::Herdr`], which closes the workspace as part of the
//!    removal.
//! 4. A dirty worktree is removable only with [`Permissions::force_dirty`],
//!    which itself requires the caller to have named the exact at-risk file
//!    count. A confirmation that can be given without reading the number is not
//!    a confirmation.
//! 5. **Never `rm -rf`.** Removal is `worktree.remove` over the socket for a
//!    worktree herdr holds open, and `git worktree remove` otherwise. Both leave
//!    the branch and every commit on it in place.
//! 6. Every removal is appended to the undo log *before* it is attempted, so a
//!    removal that half-succeeds is still recoverable.

use std::path::Path;

use crate::config::Config;
use crate::herdr::Herdr;
use crate::model::{Candidate, RemovalRecord, RemovalRoute};
use crate::Result;

/// What the user has explicitly permitted for this run. Defaults to nothing:
/// every field must be turned on by an argument the user typed or a key they
/// pressed in the review pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Permissions {
    /// Permit removing a worktree with uncommitted changes.
    pub force_dirty: bool,
    /// The file count the user acknowledged. Must equal the candidate's actual
    /// at-risk count or the removal is refused — this is what stops a
    /// `--force-dirty` typed once from applying to a worktree whose contents
    /// changed since the user looked.
    pub acknowledged_files: Option<usize>,
    /// Permit closing a herdr workspace that holds the worktree open.
    pub close_workspace: bool,
}

/// Why a removal was refused. Each variant renders as a sentence naming the
/// unblocking action, because "refused" on its own teaches the user nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    MainCheckout,
    Locked {
        reason: Option<String>,
    },
    OpenInHerdr {
        workspace: String,
        label: String,
    },
    Dirty {
        files: usize,
    },
    /// `--force-dirty` was given but the acknowledged count does not match.
    DirtyCountMismatch {
        acknowledged: usize,
        actual: usize,
    },
    /// The worktree is not in the inventory at all.
    Unknown {
        path: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f;
        unimplemented!("surface: Refusal::fmt")
    }
}

impl std::error::Error for Refusal {}

/// Decides whether one candidate may be removed under the given permissions.
///
/// Pure, and separated from [`remove_one`] precisely so the guard tests never
/// need a repository to prove a refusal.
pub fn check(candidate: &Candidate, permissions: Permissions) -> std::result::Result<(), Refusal> {
    let _ = (candidate, permissions);
    unimplemented!("surface: check")
}

/// The route a candidate's removal must take.
///
/// A worktree herdr holds open goes through the socket, because
/// `worktree.remove` removes the checkout, prunes git's admin entry and closes
/// the workspace as one operation; doing it with git alone would leave herdr
/// showing a workspace whose directory has vanished.
///
/// Everything else — which in practice is most of it, since the worktrees that
/// pile up are exactly the ones nobody has open — goes through
/// `git worktree remove`.
pub fn route_for(candidate: &Candidate) -> RemovalRoute {
    let _ = candidate;
    unimplemented!("surface: route_for")
}

/// Removes one candidate. Appends to the undo log first, then acts.
///
/// `herdr` is `None` when there is no socket to reach, which makes
/// [`RemovalRoute::Herdr`] unavailable; a candidate that needs it is refused
/// with a message that says so rather than silently falling back to git and
/// leaving herdr inconsistent.
pub fn remove_one(
    candidate: &Candidate,
    permissions: Permissions,
    herdr: Option<&mut Herdr>,
    config: &Config,
) -> Result<RemovalRecord> {
    let _ = (candidate, permissions, herdr, config);
    unimplemented!("surface: remove_one")
}

/// `git worktree remove <path>`, run from the repo root.
///
/// git refuses a dirty or locked worktree itself (exit 128), which is a second
/// guard behind [`check`] rather than the only one. Verified: this also succeeds
/// on a *prunable* worktree whose directory no longer exists, so there is no
/// need to reach for `git worktree prune` — which would be wrong anyway, since
/// it prunes every prunable worktree in the repo rather than the one selected.
pub fn git_remove(repo_root: &Path, worktree: &Path, force: bool, config: &Config) -> Result<()> {
    let _ = (repo_root, worktree, force, config);
    unimplemented!("surface: git_remove")
}

/// The command that puts a removed checkout back. Recorded in the undo log and
/// printed after every removal, because "the commits survive" is only reassuring
/// if the user can see how to get at them.
pub fn restore_command(candidate: &Candidate) -> String {
    let _ = candidate;
    unimplemented!("surface: restore_command")
}

/// Appends one record to the undo log, creating it if needed.
///
/// Best-effort in the sense that an unwritable state directory must not stop a
/// removal the user asked for — but it must be *reported*, loudly, because a
/// removal with no undo record is exactly the situation the log exists to
/// prevent.
pub fn append_log(record: &RemovalRecord) -> Result<()> {
    let _ = record;
    unimplemented!("surface: append_log")
}

/// Every record in the undo log, newest first.
pub fn read_log() -> Result<Vec<RemovalRecord>> {
    unimplemented!("surface: read_log")
}

/// `--remove <PATH>` — non-interactive removal of explicitly named worktrees.
pub fn run_remove(config: &Config, args: &[String]) -> Result<()> {
    let _ = (config, args);
    unimplemented!("surface: run_remove")
}

/// `--undo-log`.
pub fn run_undo_log() -> Result<()> {
    unimplemented!("surface: run_undo_log")
}
