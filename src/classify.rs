//! Classification: turning raw git and herdr facts into a [`Verdict`] a user
//! can act on without reading the code.
//!
//! This module is pure. It takes the facts `git.rs` and `herdr.rs` gathered and
//! decides nothing by asking the world again, which is what lets
//! `tests/classify.rs` drive every degenerate combination without a repository.

use std::time::SystemTime;

use crate::model::{Candidate, Class, OpenWorkspace, Verdict};
use crate::model::{Dirt, Upstream, Worktree};

/// Everything known about one worktree at classification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub worktree: Worktree,
    pub dirt: Dirt,
    pub upstream: Upstream,
    /// `Some(ref)` when the branch (or detached commit) is contained in that
    /// ref. `None` means the question could not be asked — a detached HEAD with
    /// no integration ref, an unborn branch, a repo with no default branch.
    ///
    /// `None` is **not** "not merged" and must never be classified as if it
    /// were.
    pub merged_into: Option<String>,
    pub last_commit: Option<SystemTime>,
    pub open_workspace: Option<OpenWorkspace>,
}

/// Classifies one worktree.
///
/// The rules, in the order they matter:
///
/// - The **main checkout** is never a candidate, whatever else is true of it:
///   `Verdict::Blocked`, reason "main checkout".
/// - **Locked** or **open in herdr** is `Blocked`. The reason names the
///   unblocking action (`git worktree unlock`, or closing the workspace).
/// - **Dirty** is at most `Review`, never `Safe`, and never preselected. The
///   reason names the file count at risk.
/// - **Safe** requires all of: clean, merged into the integration ref, upstream
///   gone, not open in herdr, not locked, not the main checkout. Every one of
///   those must be a positive observation; an unanswerable question fails the
///   test.
/// - **Prunable** is `Review`, not `Safe`, even though it is clean by
///   construction. git's own reason for a prunable worktree is usually "gitdir
///   file points to non-existent location", which is also what a temporarily
///   unmounted filesystem looks like. The row shows git's reason verbatim so the
///   user can tell the two apart.
/// - Anything with at least one death signal (merged, gone, stale, prunable) is
///   `Review`. Anything with none is `Keep`.
pub fn classify(facts: Facts, stale_after: std::time::Duration, now: SystemTime) -> Candidate {
    let _ = (facts, stale_after, now);
    unimplemented!("classifier: classify")
}

/// The set of classes a worktree carries. Split out from [`classify`] so the
/// tests can assert the reasons and the verdict independently.
pub fn classes_of(
    facts: &Facts,
    stale_after: std::time::Duration,
    now: SystemTime,
) -> std::collections::BTreeSet<Class> {
    let _ = (facts, stale_after, now);
    unimplemented!("classifier: classes_of")
}

/// The verdict implied by a class set plus the facts that are not classes (main
/// checkout, whether the merged question was answerable at all).
pub fn verdict_of(facts: &Facts, classes: &std::collections::BTreeSet<Class>) -> Verdict {
    let _ = (facts, classes);
    unimplemented!("classifier: verdict_of")
}

/// One sentence explaining the verdict, for the row's detail line. It must be
/// specific enough to act on: "merged into origin/main, upstream gone, clean"
/// rather than "safe".
pub fn reason_for(
    facts: &Facts,
    classes: &std::collections::BTreeSet<Class>,
    verdict: Verdict,
) -> String {
    let _ = (facts, classes, verdict);
    unimplemented!("classifier: reason_for")
}
