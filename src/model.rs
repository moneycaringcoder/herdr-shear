//! Shared types. This module is the contract between the git layer, the herdr
//! socket client, the classifier, the removal path, and the renderers, so each
//! can be developed and tested independently.
//!
//! Nothing here performs I/O and nothing here decides policy beyond the
//! [`Verdict`] rules, which are the product: everything else in the crate exists
//! to fill these structs in honestly.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;

/// Canonical identity for "the same repository": the absolute, canonicalized
/// `git rev-parse --git-common-dir`. herdr reports the same value as
/// `workspace.worktree.repo_key`, and the two are compared as strings after
/// canonicalization on both sides.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoKey(pub String);

/// One repository that the session, or an explicit `--repo`, points us at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Repo {
    pub key: RepoKey,
    /// Main checkout root, which is where every `git` invocation for this repo
    /// is rooted.
    pub root: PathBuf,
    /// Display name. herdr's `repo_name`, or the last path component.
    pub name: String,
}

/// What a worktree's HEAD points at.
///
/// `git worktree list --porcelain` reports these as three mutually exclusive
/// shapes, and each one changes what may be said about the worktree: a detached
/// HEAD has no branch to be merged or gone, and an unborn one has no commit at
/// all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Head {
    /// `branch refs/heads/<name>` with a resolvable HEAD.
    Branch(String),
    /// `detached`, with the raw HEAD oid.
    Detached,
    /// `HEAD 0000000…` — either a freshly initialised worktree with no commit,
    /// or one whose branch was deleted underneath it. `git.rs` distinguishes
    /// them via the worktree's own `logs/HEAD` and records which in
    /// [`Worktree::notes`].
    Unborn,
    /// A bare repository record. Never a removal candidate.
    Bare,
}

impl Head {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch(name) => Some(name),
            _ => None,
        }
    }
}

/// One raw record from `git worktree list --porcelain -z`, plus the facts that
/// need a second git call. This is what `git.rs` produces; it carries no
/// judgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub repo: RepoKey,
    pub repo_root: PathBuf,
    /// Absolute path git reports. Not canonicalized: for a prunable worktree it
    /// does not exist, and canonicalizing would fail.
    pub path: PathBuf,
    pub head: Head,
    /// 40-hex HEAD oid. `None` for bare and unborn records.
    pub head_oid: Option<String>,
    /// The first record git prints for a repo is the main checkout. It is never
    /// a removal candidate, whatever else is true of it.
    pub is_main: bool,
    /// `Some(reason)` when git reports `locked`. The reason is `None` when git
    /// gave the flag with no text, which it does for a bare `git worktree lock`.
    pub locked: Option<LockInfo>,
    /// `Some(reason)` when git reports `prunable`, e.g. "gitdir file points to
    /// non-existent location".
    pub prunable: Option<String>,
    /// Anything read that the user should see but that is not a classification —
    /// a failed sub-command, an ambiguous state.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub reason: Option<String>,
}

/// Working-tree cleanliness, from `status --porcelain=v2 -z -uall`.
///
/// A worktree is dirty if *any* of these is non-zero. They are counted
/// separately because the second confirmation for a dirty removal has to name
/// what is at risk, and "3 untracked files" and "3 unmerged paths" are very
/// different sentences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dirt {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub unmerged: usize,
}

impl Dirt {
    pub fn total(&self) -> usize {
        self.staged + self.unstaged + self.untracked + self.unmerged
    }

    pub fn is_dirty(&self) -> bool {
        self.total() > 0
    }
}

/// Upstream tracking state for a branch, from
/// `for-each-ref --format='%(upstream) %(upstream:track)'`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Upstream {
    /// Full upstream ref, e.g. `refs/remotes/origin/topic`.
    pub name: Option<String>,
    /// `%(upstream:track)` reported `[gone]`: the branch is configured to track
    /// a remote ref that no longer exists. This is the strongest offline signal
    /// that a branch's work has landed and its remote branch was tidied away.
    pub gone: bool,
    pub ahead: u32,
    pub behind: u32,
}

/// Every reason a worktree looks dead. A worktree usually carries several; the
/// set is kept whole because the review table shows the reasons, not a single
/// verdict word.
///
/// Ordering is significance order, so `BTreeSet::iter` presents the most
/// alarming reason first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Uncommitted changes. Overrides everything for the purposes of safety.
    Dirty,
    /// `git worktree list` reports `locked`. Never removable without unlocking.
    Locked,
    /// Open as a herdr workspace. Removable only after closing the workspace.
    OpenInHerdr,
    /// git reports `prunable` — the checkout directory is gone but the admin
    /// entry survives.
    Prunable,
    /// The branch tracks a remote ref that no longer exists.
    GoneUpstream,
    /// The branch tip is contained in the integration ref.
    Merged,
    /// The branch tip is older than the staleness threshold.
    Stale,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Dirty => "dirty",
            Class::Locked => "locked",
            Class::OpenInHerdr => "open",
            Class::Prunable => "prunable",
            Class::GoneUpstream => "gone",
            Class::Merged => "merged",
            Class::Stale => "stale",
        }
    }
}

/// What shear is willing to do with a worktree.
///
/// The whole product is this enum being trustworthy. `Safe` is the only value a
/// bulk action may preselect, and its rule is deliberately conservative: a
/// worktree is safe when there is no way left for it to hold work that is not
/// also somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Clean, merged into the integration ref, upstream gone, not open in
    /// herdr, not locked, not the main checkout. Preselectable.
    Safe,
    /// Some evidence of death but not all of it. Removable, never preselected.
    Review,
    /// Nothing suggests this is dead. Removable only by explicit selection, and
    /// the table says so.
    Keep,
    /// Cannot be removed as things stand: locked, open in herdr, or the main
    /// checkout. The row names the unblocking action.
    Blocked,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Safe => "safe",
            Verdict::Review => "review",
            Verdict::Keep => "keep",
            Verdict::Blocked => "blocked",
        }
    }
}

/// Disk usage of a checkout. Sizing walks the whole tree, so it is measured
/// lazily — scanning forty worktrees before drawing the first row feels broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Size {
    /// Not measured yet.
    #[default]
    Pending,
    /// Bytes actually occupied on disk (`st_blocks * 512` on unix), with
    /// hardlinks counted once.
    Bytes(u64),
    /// The path does not exist — a prunable worktree reclaims nothing.
    Gone,
    /// Measurement failed; the reason is carried so the row can say why rather
    /// than showing a plausible zero.
    Failed,
}

/// One worktree, fully classified. This is what the review table renders and
/// what `remove.rs` guards against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub worktree: Worktree,
    pub dirt: Dirt,
    pub upstream: Upstream,
    /// The ref this worktree's branch was tested against, and whether it is
    /// contained in it. `None` when the test could not run (detached, unborn, or
    /// the integration ref does not resolve) — which is *not* the same as "not
    /// merged" and must never be rendered as if it were.
    pub merged_into: Option<String>,
    /// Commit time of the branch tip, for staleness.
    pub last_commit: Option<SystemTime>,
    /// herdr workspace holding this checkout open, if any.
    pub open_workspace: Option<OpenWorkspace>,
    pub classes: BTreeSet<Class>,
    pub verdict: Verdict,
    pub size: Size,
    /// Why the verdict is what it is, in one sentence, for the detail line.
    pub reason: String,
}

impl Candidate {
    pub fn path(&self) -> &std::path::Path {
        &self.worktree.path
    }

    pub fn branch(&self) -> Option<&str> {
        self.worktree.head.branch()
    }

    pub fn is(&self, class: Class) -> bool {
        self.classes.contains(&class)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspace {
    pub workspace_id: String,
    pub label: String,
}

/// Everything one scan found, grouped by repo in the order the repos were
/// discovered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub repos: Vec<Repo>,
    pub candidates: Vec<Candidate>,
    /// Non-fatal problems: a repo that could not be read, a herdr call that
    /// failed, a worktree whose status could not be taken. These are rendered,
    /// never swallowed — a scan that silently drops a repo looks exactly like a
    /// tidy machine.
    pub notes: Vec<String>,
}

impl Inventory {
    pub fn find(&self, path: &std::path::Path) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.worktree.path == path)
    }

    /// The candidates a bulk action may preselect. Nothing else, ever.
    pub fn safe(&self) -> impl Iterator<Item = &Candidate> {
        self.candidates
            .iter()
            .filter(|c| c.verdict == Verdict::Safe)
    }

    pub fn repo(&self, key: &RepoKey) -> Option<&Repo> {
        self.repos.iter().find(|r| &r.key == key)
    }
}

/// How a removal was carried out. Recorded in the undo log so a reader can tell
/// which bookkeeping was updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalRoute {
    /// `worktree.remove` over the herdr socket. Removes the checkout, prunes
    /// the git admin entry, and closes the workspace.
    Herdr,
    /// `git worktree remove`, for a worktree herdr does not have open.
    Git,
}

/// One entry in the undo log. The log exists so that a removal is always
/// recoverable: the commits survive, and this is the note that says where from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovalRecord {
    /// RFC 3339, UTC.
    pub at: String,
    pub path: String,
    pub repo_root: String,
    pub branch: Option<String>,
    /// HEAD oid at the moment of removal. This is the recovery handle.
    pub head_oid: Option<String>,
    pub route: String,
    pub classes: Vec<String>,
    pub verdict: String,
    pub bytes_reclaimed: Option<u64>,
    /// The exact command that puts the checkout back.
    pub restore_command: String,
}
