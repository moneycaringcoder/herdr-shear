//! Read-only git plumbing.
//!
//! Every function here reads. Nothing in this module may write to a user's
//! repository, and `tests/read_only.rs` fingerprints the index, working tree,
//! refs, reflogs and object store before and after a full scan to prove it.
//!
//! Hard rules, verified rather than assumed — see `docs/git-plumbing.md`:
//!
//! 1. Always pass `--no-optional-locks` to `status`. Plain `status` takes
//!    `<gitdir>/index.lock` to write back its stat cache.
//! 2. Never touch a worktree's real index, and never write an object.
//! 3. `git` is resolved explicitly: herdr runs plugin commands with no shell and
//!    a minimal `PATH`.
//! 4. Every invocation is bounded by a timeout, so one wedged repo cannot stall
//!    a scan.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::{Dirt, Repo, RepoKey, Upstream, Worktree};
use crate::Result;

/// Reason code recorded on a worktree whose HEAD is all zeroes and which has no
/// `logs/HEAD`: a freshly initialised worktree that has never had a commit.
pub const NOTE_UNBORN: &str = "unborn";

/// Reason code for a worktree whose HEAD is all zeroes but which *does* have a
/// `logs/HEAD`: its branch was deleted underneath it. `symbolic-ref -q HEAD`
/// does not distinguish the two — verified on git 2.53.0, where it exits 0 and
/// prints the same ref name in both cases.
pub const NOTE_BROKEN_HEAD: &str = "broken-head";

/// Reason code for a repo where no integration ref resolved, so the merged
/// question could never be asked.
pub const NOTE_NO_INTEGRATION_REF: &str = "no-integration-ref";

/// Canonical identity for a repository: the absolute, canonicalized
/// `--git-common-dir`. All worktrees of one repo share it; each has its own
/// `--git-dir`. Do not use `--git-dir`, `--show-toplevel`, or the directory
/// name.
pub fn repo_key(path: &Path, timeout: Duration) -> Result<RepoKey> {
    let _ = (path, timeout);
    unimplemented!("classifier: repo_key")
}

/// Resolves any path inside a repository to the repo it belongs to.
///
/// Returns `Ok(None)` — not an error — when the path is simply not in a git
/// repository, because "this workspace is not a repo" is ordinary data.
pub fn repo_at(path: &Path, timeout: Duration) -> Result<Option<Repo>> {
    let _ = (path, timeout);
    unimplemented!("classifier: repo_at")
}

/// Every worktree of one repository, from `git worktree list --porcelain -z`.
///
/// Records are separated by an empty NUL field; `worktree <abs-path>` is always
/// first and after that no ordering may be assumed. A `bare` record has no
/// `HEAD` and no `branch`; `HEAD 0000…0000` means unborn or dangling;
/// `detached` replaces `branch`; `locked` and `prunable` may carry an optional
/// reason on the same line.
///
/// The first record git prints is the main checkout, which is marked
/// [`Worktree::is_main`] and is never a removal candidate.
///
/// This — not herdr — is the authority for worktree enumeration. herdr's
/// `worktree.list` does not report locking at all and reports every worktree's
/// `label` as the *repository* name; see `docs/herdr-protocol.md`.
pub fn worktrees(repo_root: &Path, timeout: Duration) -> Result<Vec<Worktree>> {
    let _ = (repo_root, timeout);
    unimplemented!("classifier: worktrees")
}

/// Parses the `-z` porcelain body of `git worktree list`. Split out so
/// `tests/git_parse.rs` can drive it with captured bytes, including a worktree
/// path containing a newline.
pub fn parse_worktree_list(
    bytes: &[u8],
    repo: &RepoKey,
    repo_root: &Path,
) -> Result<Vec<Worktree>> {
    let _ = (bytes, repo, repo_root);
    unimplemented!("classifier: parse_worktree_list")
}

/// Per-branch upstream state and tip commit time, from one `for-each-ref` over
/// `refs/heads/`.
///
/// `%(upstream:track)` reports the literal `[gone]` for a branch configured to
/// track a ref that no longer exists. That string is the detection; do not try
/// to infer it from a missing remote ref, which also happens when the remote has
/// simply never been fetched.
pub fn branches(repo_root: &Path, timeout: Duration) -> Result<Vec<BranchRow>> {
    let _ = (repo_root, timeout);
    unimplemented!("classifier: branches")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub name: String,
    pub oid: String,
    pub upstream: Upstream,
    pub tip: Option<SystemTime>,
}

/// Parses `for-each-ref` output. Split out for the same reason as
/// [`parse_worktree_list`].
pub fn parse_branches(bytes: &[u8]) -> Result<Vec<BranchRow>> {
    let _ = bytes;
    unimplemented!("classifier: parse_branches")
}

/// The ref every branch's merged-ness is measured against.
///
/// Order: the caller's explicit choice, then `origin/HEAD` (the only
/// authoritative answer), then [`crate::config::DEFAULT_BRANCH_GUESSES`] in
/// order. `Ok(None)` means no candidate resolved — the merged question cannot be
/// asked in this repo, which must be rendered as "unknown", never as "not
/// merged".
pub fn integration_ref(
    repo_root: &Path,
    configured: Option<&str>,
    timeout: Duration,
) -> Result<Option<String>> {
    let _ = (repo_root, configured, timeout);
    unimplemented!("classifier: integration_ref")
}

/// Short names of every local branch contained in `integration_ref`, from
/// `for-each-ref --merged=<ref> refs/heads/`.
pub fn merged_branches(
    repo_root: &Path,
    integration_ref: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    let _ = (repo_root, integration_ref, timeout);
    unimplemented!("classifier: merged_branches")
}

/// Whether an arbitrary commit is contained in `integration_ref`, for a detached
/// HEAD that has no branch to look up.
pub fn is_ancestor(
    repo_root: &Path,
    oid: &str,
    integration_ref: &str,
    timeout: Duration,
) -> Result<bool> {
    let _ = (repo_root, oid, integration_ref, timeout);
    unimplemented!("classifier: is_ancestor")
}

/// Uncommitted state of one worktree, from
/// `status --porcelain=v2 -z --untracked-files=all --renames`.
///
/// `-z` disables path quoting, so paths are raw bytes. The framing rule naive
/// parsers get wrong: a `2` (rename/copy) record consumes **two** NUL-terminated
/// fields — the new path, then the original path as the very next field.
///
/// A worktree whose directory does not exist (prunable) is not an error here:
/// return `Ok(Dirt::default())` and let the classifier see the prunable flag.
pub fn dirt(worktree: &Path, timeout: Duration) -> Result<Dirt> {
    let _ = (worktree, timeout);
    unimplemented!("classifier: dirt")
}

/// Parses the `-z` porcelain v2 body of `git status`.
pub fn parse_status(bytes: &[u8]) -> Result<Dirt> {
    let _ = bytes;
    unimplemented!("classifier: parse_status")
}

/// Whether a worktree's own HEAD reflog exists. This is the discriminator
/// between an unborn branch and one deleted underneath the worktree; a worktree
/// that ever had a commit checked out has `logs/HEAD`, a freshly initialised one
/// does not.
pub fn has_head_reflog(worktree: &Path, timeout: Duration) -> Result<bool> {
    let _ = (worktree, timeout);
    unimplemented!("classifier: has_head_reflog")
}

/// Runs one git command, read-only, with a timeout, an explicitly resolved
/// binary, and an environment scrubbed of anything that would let a repository's
/// own config change what we do.
///
/// Returns the raw stdout bytes on success. On a non-zero exit the error carries
/// git's stderr, because a git message that names the ref is more useful than
/// anything this crate could write in its place.
pub fn run(repo_or_worktree: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let _ = (repo_or_worktree, args, timeout);
    unimplemented!("classifier: run")
}

/// Absolute path to the `git` binary. herdr runs plugin commands with a minimal
/// `PATH`, so this searches `PATH` explicitly and falls back to the usual
/// locations, failing loudly rather than letting every git call fail one by one
/// with a confusing message.
pub fn git_binary() -> Result<PathBuf> {
    unimplemented!("classifier: git_binary")
}
