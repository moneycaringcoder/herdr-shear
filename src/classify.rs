//! Classification: turning raw git and herdr facts into a [`Verdict`] a user
//! can act on without reading the code.
//!
//! This module is pure. It takes the facts `git.rs` and `herdr.rs` gathered and
//! decides nothing by asking the world again, which is what lets
//! `tests/classify.rs` drive every degenerate combination without a repository.

use std::collections::BTreeSet;
use std::time::SystemTime;

use crate::model::{Candidate, Class, Head, Merged, OpenWorkspace, Size, Verdict};
use crate::model::{Dirt, Upstream, Worktree};

/// Everything known about one worktree at classification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub worktree: Worktree,
    pub dirt: Dirt,
    pub upstream: Upstream,
    /// Merged-ness in three states. [`Merged::Into`] is the branch (or detached
    /// commit) being contained in that ref; [`Merged::No`] is the test having
    /// run and said no; [`Merged::Unknown`] is the question not having been
    /// askable at all — a detached HEAD with no integration ref, an unborn
    /// branch, a repo with no default branch.
    ///
    /// [`Merged::Unknown`] is **not** "not merged" and must never be classified
    /// as if it were.
    pub merged: Merged,
    pub last_commit: Option<SystemTime>,
    pub open_workspace: Option<OpenWorkspace>,
    /// Protection pattern that matched the checkout path or branch name.
    pub protected: Option<String>,
}

/// Classifies one worktree.
///
/// The rules, in the order they matter:
///
/// - The **main checkout** is never a candidate, whatever else is true of it:
///   `Verdict::Blocked`, reason "main checkout".
/// - A configured **protection pattern** is `Blocked` and cannot be overridden.
///   The reason names the pattern and the config file to edit.
/// - **Locked** or **open in herdr** is `Blocked`. The reason names the
///   unblocking action (`git worktree unlock`, or closing the workspace).
/// - **Dirty** is at most `Review`, never `Safe`, and never preselected. The
///   reason names the file count at risk.
/// - **Safe** requires all of: clean, merged into the integration ref, upstream
///   gone, not protected, not open in herdr, not locked, not the main checkout.
///   Every one of those must be a positive observation; an unanswerable question
///   fails the test.
/// - **Prunable** is `Review`, not `Safe`, even though it is clean by
///   construction. git's own reason for a prunable worktree is usually "gitdir
///   file points to non-existent location", which is also what a temporarily
///   unmounted filesystem looks like. The row shows git's reason verbatim so the
///   user can tell the two apart.
/// - Anything with at least one death signal (merged, gone, stale, prunable) is
///   `Review`. Anything with none is `Keep`.
pub fn classify(facts: Facts, stale_after: std::time::Duration, now: SystemTime) -> Candidate {
    let classes = classes_of(&facts, stale_after, now);
    let verdict = verdict_of(&facts, &classes);
    let reason = reason_for(&facts, &classes, verdict);

    Candidate {
        dirt: facts.dirt,
        upstream: facts.upstream,
        merged: facts.merged,
        last_commit: facts.last_commit,
        open_workspace: facts.open_workspace,
        protected: facts.protected,
        worktree: facts.worktree,
        classes,
        verdict,
        // Sizing walks the whole tree, so a scan never does it. `disk::measure`
        // fills this in behind the rendering.
        size: Size::Pending,
        reason,
    }
}

/// The set of classes a worktree carries. Split out from [`classify`] so the
/// tests can assert the reasons and the verdict independently.
pub fn classes_of(
    facts: &Facts,
    stale_after: std::time::Duration,
    now: SystemTime,
) -> std::collections::BTreeSet<Class> {
    let mut classes = BTreeSet::new();

    if facts.protected.is_some() {
        classes.insert(Class::Protected);
    }
    if facts.dirt.is_dirty() {
        classes.insert(Class::Dirty);
    }
    if facts.worktree.locked.is_some() {
        classes.insert(Class::Locked);
    }
    if facts.open_workspace.is_some() {
        classes.insert(Class::OpenInHerdr);
    }
    if facts.worktree.prunable.is_some() {
        classes.insert(Class::Prunable);
    }
    if facts.upstream.gone {
        classes.insert(Class::GoneUpstream);
    }
    // Only `Into` is evidence. `No` is evidence of the opposite and `Unknown` is
    // no evidence at all; neither may add the class.
    if facts.merged.is_merged() {
        classes.insert(Class::Merged);
    }
    if is_stale(facts, stale_after, now) {
        classes.insert(Class::Stale);
    }

    classes
}

/// A branch whose tip is older than the threshold. A worktree with no known
/// commit time is never stale: "I do not know when this was last touched" is not
/// "this was last touched a long time ago".
fn is_stale(facts: &Facts, stale_after: std::time::Duration, now: SystemTime) -> bool {
    match facts.last_commit {
        Some(tip) => now
            .duration_since(tip)
            .map(|age| age > stale_after)
            .unwrap_or(false),
        None => false,
    }
}

/// The verdict implied by a class set plus the facts that are not classes (main
/// checkout, whether the merged question was answerable at all).
pub fn verdict_of(facts: &Facts, classes: &std::collections::BTreeSet<Class>) -> Verdict {
    // 1. The main checkout is never a candidate. No override exists, and no
    //    later rule may promote it.
    if facts.worktree.is_main {
        return Verdict::Blocked;
    }
    // 2. Protection only narrows what can be removed. No permission can
    //    override it, and no later rule may promote it.
    if classes.contains(&Class::Protected) {
        return Verdict::Blocked;
    }
    // 3. Removable only after the user does something themselves.
    if classes.contains(&Class::Locked) || classes.contains(&Class::OpenInHerdr) {
        return Verdict::Blocked;
    }

    // 4. Safe. Every condition is a positive observation, and every one of them
    //    is checked here rather than inferred from the class set alone, so that
    //    adding a class can never accidentally widen what is preselectable.
    let safe = !classes.contains(&Class::Dirty)
        && facts.merged.is_merged()
        && facts.upstream.gone
        // A prunable worktree is clean and merged by construction, and its
        // reason is indistinguishable from an unmounted filesystem. It gets
        // looked at, never preselected.
        && !classes.contains(&Class::Prunable)
        // Implied by `upstream.gone`, since only a branch can have an upstream,
        // but stated so that the safe rule reads as its own complete argument.
        && facts.worktree.head.branch().is_some();
    if safe {
        return Verdict::Safe;
    }

    // 5. Some evidence of death, but not all of it.
    let dying = [
        Class::Merged,
        Class::GoneUpstream,
        Class::Stale,
        Class::Prunable,
    ]
    .iter()
    .any(|class| classes.contains(class));
    if dying {
        return Verdict::Review;
    }

    Verdict::Keep
}

/// One sentence explaining the verdict, for the row's detail line. It must be
/// specific enough to act on: "merged into origin/main, upstream gone, clean"
/// rather than "safe".
pub fn reason_for(
    facts: &Facts,
    classes: &std::collections::BTreeSet<Class>,
    verdict: Verdict,
) -> String {
    if facts.worktree.is_main {
        return "main checkout; never a removal candidate".to_string();
    }
    if let Some(pattern) = &facts.protected {
        let path = crate::config::config_file();
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.json");
        return format!(
            "protected by pattern `{pattern}`; edit or remove that pattern in {file} to unblock"
        );
    }
    if let Some(lock) = &facts.worktree.locked {
        let reason = match &lock.reason {
            Some(reason) if !reason.trim().is_empty() => format!(" ({reason})"),
            // git prints a bare `locked` for `git worktree lock` with no
            // `--reason`, which is a different fact from an empty reason.
            _ => " (no reason given)".to_string(),
        };
        return format!(
            "locked{reason}; run `git worktree unlock {}` to unblock",
            facts.worktree.path.display()
        );
    }
    if let Some(workspace) = &facts.open_workspace {
        return format!(
            "open in the herdr workspace {}; close it to unblock",
            workspace.label
        );
    }

    let mut parts: Vec<String> = Vec::new();
    if classes.contains(&Class::Dirty) {
        parts.push(dirt_phrase(&facts.dirt));
    } else if verdict == Verdict::Safe {
        parts.push("clean".to_string());
    }
    if let Some(prunable) = &facts.worktree.prunable {
        parts.push(match &prunable.reason {
            // Shown verbatim: "gitdir file points to non-existent location" is
            // also what a temporarily unmounted filesystem looks like, and only
            // the user can tell the two apart.
            Some(reason) if !reason.trim().is_empty() => format!("prunable ({reason})"),
            _ => "prunable: git's admin entry survives a checkout that is no longer there"
                .to_string(),
        });
    }
    parts.push(merged_phrase(facts));
    parts.push(upstream_phrase(&facts.upstream));
    if classes.contains(&Class::Stale) {
        parts.push("no commit within the staleness window".to_string());
    }

    if verdict == Verdict::Keep {
        parts.push("nothing here says this checkout is finished".to_string());
    }
    parts.join(", ")
}

/// The three merged states, each said in words that cannot be mistaken for
/// another one. "cannot tell" is never abbreviated to "not merged".
fn merged_phrase(facts: &Facts) -> String {
    match &facts.merged {
        Merged::Into(reference) => format!("merged into {reference}"),
        Merged::No(reference) => format!("not merged into {reference}"),
        // Two ways to be unanswerable, and the row has to say which, because the
        // user's next action differs: set a default branch, or look at a
        // worktree that has never had a commit.
        Merged::Unknown => match facts.worktree.head {
            Head::Unborn | Head::Bare => {
                "cannot tell whether it is merged: there is no commit here to test".to_string()
            }
            _ => "cannot tell whether it is merged: no integration ref resolved here".to_string(),
        },
    }
}

fn upstream_phrase(upstream: &Upstream) -> String {
    match (&upstream.name, upstream.gone) {
        (Some(name), true) => format!("upstream {name} is gone"),
        (Some(name), false) => format!("upstream {name} still exists"),
        // Two empty fields from `for-each-ref` is a third state — never pushed —
        // and is not evidence that the work has landed anywhere.
        (None, _) => "no upstream configured".to_string(),
    }
}

/// Names what is at risk, counted the way the second confirmation has to say it:
/// "3 untracked files" and "3 unmerged paths" are very different sentences.
fn dirt_phrase(dirt: &Dirt) -> String {
    let mut parts: Vec<String> = Vec::new();
    if dirt.staged > 0 {
        parts.push(format!("{} staged", dirt.staged));
    }
    if dirt.unstaged > 0 {
        parts.push(format!("{} unstaged", dirt.unstaged));
    }
    if dirt.untracked > 0 {
        parts.push(format!("{} untracked", dirt.untracked));
    }
    if dirt.unmerged > 0 {
        parts.push(format!("{} unmerged", dirt.unmerged));
    }
    if parts.is_empty() {
        return "clean".to_string();
    }
    format!("{} at risk", parts.join(", "))
}
