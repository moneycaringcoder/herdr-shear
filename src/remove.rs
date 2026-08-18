//! Removal, and the guard rails that are the actual product.
//!
//! This is the only module in the crate that may change anything. Every path
//! into it is an explicit selection; nothing here is ever reached by a scan.
//!
//! The rules, each of which needs a test that proves it *refuses*:
//!
//! 1. The main checkout is never removable. No override exists.
//! 2. A protected worktree is never removable. The user must edit or remove the
//!    matching pattern from `config.json`; no permission overrides protection.
//! 3. A locked worktree is never removable. The user must `git worktree unlock`
//!    it themselves — shear will not unlock on their behalf, because the lock is
//!    somebody's explicit "do not touch this".
//! 4. A worktree open in a herdr workspace is removable only with
//!    [`Permissions::close_workspace`], and then only through
//!    [`RemovalRoute::Herdr`], which closes the workspace as part of the
//!    removal.
//! 5. A dirty worktree is removable only with [`Permissions::force_dirty`],
//!    which itself requires the caller to have named the exact at-risk file
//!    count. A confirmation that can be given without reading the number is not
//!    a confirmation.
//! 6. **Never `rm -rf`.** Removal is `worktree.remove` over the socket for a
//!    worktree herdr holds open, and `git worktree remove` otherwise. Both leave
//!    the branch and every commit on it in place.
//! 7. Every removal is appended to the undo log *before* it is attempted, so a
//!    removal that half-succeeds is still recoverable.
//!
//! The one git invocation that writes lives here rather than in `git.rs`. That
//! module is documented and tested as read-only, so the mutating command has to
//! be somewhere the read-only claim is not made about.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::herdr::Herdr;
use crate::model::{Candidate, Head, RemovalRecord, RemovalRoute, Size};
use crate::shear;
use crate::Result;

/// Prefix of the acknowledgement flag: `--i-understand-<N>-files`.
const ACK_PREFIX: &str = "--i-understand-";
const ACK_SUFFIX: &str = "-files";

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
///
/// The rendered sentence carries the literal token `<path>` wherever the
/// worktree's own path belongs; [`Refusal::about`] substitutes it. Keeping the
/// path out of the variants means [`check`] stays comparable with `==` in a
/// test without every expectation having to spell a temporary directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    MainCheckout,
    Protected {
        pattern: String,
    },
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

impl Refusal {
    /// The refusal as a sentence about one concrete worktree.
    pub fn about(&self, path: &Path) -> String {
        self.to_string()
            .replace("<path>", &shell_quote(&path.to_string_lossy()))
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::MainCheckout => write!(
                f,
                "this is the repository's main checkout, which shear never removes. \
                 There is no override: removing it would take the repository with it"
            ),
            Refusal::Protected { pattern } => write!(
                f,
                "the worktree is protected by pattern `{pattern}` in config.json. Edit or \
                 remove that pattern from config.json first; no flag overrides protection"
            ),
            Refusal::Locked { reason } => {
                match reason {
                    Some(reason) => write!(f, "the worktree is locked ({reason}). ")?,
                    // A bare `git worktree lock` gives no reason at all, which is
                    // not the same as an empty one, and must not be rendered as
                    // "locked ()".
                    None => write!(f, "the worktree is locked, with no reason recorded. ")?,
                }
                write!(
                    f,
                    "A lock is somebody's explicit \"do not touch this\", so shear will not \
                     lift it on their behalf: run `git worktree unlock <path>` yourself first"
                )
            }
            Refusal::OpenInHerdr { workspace, label } => {
                // A workspace with no label of its own falls back to its id, and
                // "workspace w1X (w1X)" is the sort of thing that makes a reader
                // wonder what the second one means.
                let named = if label.is_empty() || label == workspace {
                    workspace.clone()
                } else {
                    format!("{workspace} ({label})")
                };
                write!(
                    f,
                    "herdr holds this worktree open as workspace {named}. \
                     Pass --close-workspace to let shear close it as part of the removal, \
                     or close it yourself first"
                )
            }
            Refusal::Dirty { files } => write!(
                f,
                "the worktree has {files} uncommitted {} at risk. Pass --force-dirty \
                 together with --i-understand-{files}-files to remove it anyway",
                plural(*files, "file", "files")
            ),
            Refusal::DirtyCountMismatch {
                acknowledged,
                actual,
            } => write!(
                f,
                "--force-dirty was given with --i-understand-{acknowledged}-files, but this \
                 worktree has {actual} uncommitted {} at risk. The acknowledgement has to name \
                 the real number, so re-read the count and pass --i-understand-{actual}-files",
                plural(*actual, "file", "files")
            ),
            Refusal::Unknown { path } => write!(
                f,
                "{path} is not a worktree in this scan. Check the path, or pass --repo <PATH> \
                 if it lives in a repository the herdr session does not know about"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Decides whether one candidate may be removed under the given permissions.
///
/// Pure, and separated from [`remove_one`] precisely so the guard tests never
/// need a repository to prove a refusal.
///
/// The order is the order of severity, and it matters: protection is checked
/// before every overridable refusal because it is the refusal the user cannot
/// talk shear out of.
pub fn check(candidate: &Candidate, permissions: Permissions) -> std::result::Result<(), Refusal> {
    if candidate.worktree.is_main {
        return Err(Refusal::MainCheckout);
    }
    if let Some(pattern) = &candidate.protected {
        return Err(Refusal::Protected {
            pattern: pattern.clone(),
        });
    }
    if let Some(lock) = &candidate.worktree.locked {
        return Err(Refusal::Locked {
            reason: lock.reason.clone(),
        });
    }
    if let Some(open) = &candidate.open_workspace {
        if !permissions.close_workspace {
            return Err(Refusal::OpenInHerdr {
                workspace: open.workspace_id.clone(),
                label: open.label.clone(),
            });
        }
    }
    if candidate.dirt.is_dirty() {
        let actual = candidate.dirt.total();
        if !permissions.force_dirty {
            return Err(Refusal::Dirty { files: actual });
        }
        match permissions.acknowledged_files {
            // No count at all is not a mismatch, it is an unanswered question:
            // the user has not yet been made to read the number.
            None => return Err(Refusal::Dirty { files: actual }),
            Some(acknowledged) if acknowledged != actual => {
                return Err(Refusal::DirtyCountMismatch {
                    acknowledged,
                    actual,
                })
            }
            Some(_) => {}
        }
    }
    Ok(())
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
    match candidate.open_workspace {
        Some(_) => RemovalRoute::Herdr,
        None => RemovalRoute::Git,
    }
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
    if let Err(refusal) = check(candidate, permissions) {
        return Err(Box::new(refusal));
    }

    let route = route_for(candidate);
    let path = candidate.path().to_path_buf();

    // Resolve the socket before writing anything down. A removal that cannot
    // take its required route has not happened and must not be logged as if it
    // had.
    let mut client = match (route, herdr) {
        (RemovalRoute::Herdr, Some(client)) => Some(client),
        (RemovalRoute::Herdr, None) => {
            let open = candidate
                .open_workspace
                .as_ref()
                .map(|w| format!("{} ({})", w.workspace_id, w.label))
                .unwrap_or_else(|| "an unknown workspace".to_string());
            return Err(format!(
                "{}: herdr holds this worktree open as workspace {open}, so it can only be \
                 removed through the herdr socket — and there is no socket to reach. \
                 shear will not remove it with git instead: that would leave herdr showing a \
                 workspace whose directory has vanished. Run this from inside herdr, or close \
                 the workspace yourself first.",
                path.display()
            )
            .into());
        }
        (RemovalRoute::Git, _) => None,
    };

    // Only a genuinely dirty worktree is forced. Passing --force at large would
    // make the flag mean "and anything else you find", which is exactly the
    // habit the acknowledgement exists to break.
    let force = permissions.force_dirty && candidate.dirt.is_dirty();
    let record = record_for(candidate, route);

    // Rule 6: the note goes down before the act, so a removal that half-succeeds
    // is still recoverable.
    if let Err(err) = append_log(&record) {
        eprintln!(
            "shear: WARNING: no undo record could be written for {}: {err}\n\
             shear: the removal is going ahead because you asked for it, but nothing will \
             remember it. Keep this: {}",
            path.display(),
            record.restore_command
        );
    }

    match route {
        RemovalRoute::Herdr => {
            let client = client
                .as_mut()
                .expect("the herdr route without a client is refused above");
            let workspace = candidate
                .open_workspace
                .as_ref()
                .expect("the herdr route implies an open workspace");
            client
                .remove_worktree(&workspace.workspace_id, force)
                .map_err(|err| -> Box<dyn std::error::Error> {
                    format!("{}: herdr refused the removal: {err}", path.display()).into()
                })?;
        }
        RemovalRoute::Git => {
            git_remove(&candidate.worktree.repo_root, &path, force, config)?;
        }
    }

    Ok(record)
}

/// `git worktree remove <path>`, run from the repo root.
///
/// git refuses a dirty or locked worktree itself (exit 128), which is a second
/// guard behind [`check`] rather than the only one. Verified: this also succeeds
/// on a *prunable* worktree whose directory no longer exists, so there is no
/// need to reach for `git worktree prune` — which would be wrong anyway, since
/// it prunes every prunable worktree in the repo rather than the one selected.
///
/// `force` is git's single `--force`, never `-f -f`: the doubled form overrides
/// a lock, and shear does not override locks.
pub fn git_remove(repo_root: &Path, worktree: &Path, force: bool, config: &Config) -> Result<()> {
    let repo_root = repo_root.to_string_lossy().into_owned();
    let worktree = worktree.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["-C", &repo_root, "worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push("--");
    args.push(&worktree);
    run_git(&args, config.git_timeout)
}

/// The command that puts a removed checkout back. Recorded in the undo log and
/// printed after every removal, because "the commits survive" is only reassuring
/// if the user can see how to get at them.
///
/// It is a shell command, quoted for a shell, because that is where the user
/// will paste it.
pub fn restore_command(candidate: &Candidate) -> String {
    let root = shell_quote(&candidate.worktree.repo_root.to_string_lossy());
    let path = shell_quote(&candidate.path().to_string_lossy());
    match (
        &candidate.worktree.head,
        candidate.worktree.head_oid.as_deref(),
    ) {
        (Head::Branch(branch), _) => {
            format!("git -C {root} worktree add {path} {}", shell_quote(branch))
        }
        // No branch to check out, so the oid is the handle. `--detach` is
        // explicit rather than relying on git inferring it from a commit-ish.
        (_, Some(oid)) => format!("git -C {root} worktree add --detach {path} {oid}"),
        // An unborn worktree never had a commit, so there is nothing to restore
        // *to*; the honest command is the one that puts a checkout back at that
        // path, and it says so by naming HEAD.
        (_, None) => format!("git -C {root} worktree add --detach {path} HEAD"),
    }
}

/// Appends one record to the undo log, creating it if needed.
///
/// Best-effort in the sense that an unwritable state directory must not stop a
/// removal the user asked for — but it must be *reported*, loudly, because a
/// removal with no undo record is exactly the situation the log exists to
/// prevent. Reporting is [`remove_one`]'s job; this one simply fails.
pub fn append_log(record: &RemovalRecord) -> Result<()> {
    use std::io::Write;

    let path = config::undo_log();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| format!("could not open {}: {err}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(())
}

/// Every record in the undo log, newest first.
///
/// A line that will not parse is reported and skipped rather than failing the
/// read: one corrupt record must not hide every good one.
pub fn read_log() -> Result<Vec<RemovalRecord>> {
    let path = config::undo_log();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("could not read {}: {err}", path.display()).into()),
    };

    let mut records = Vec::new();
    for (number, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RemovalRecord>(line) {
            Ok(record) => records.push(record),
            Err(err) => eprintln!(
                "shear: {}:{}: unreadable undo record ({err})",
                path.display(),
                number + 1
            ),
        }
    }
    records.reverse();
    Ok(records)
}

/// `--remove <PATH>` — non-interactive removal of explicitly named worktrees.
pub fn run_remove(config: &Config, args: &[String]) -> Result<()> {
    let (paths, permissions) = parse_remove_args(args)?;
    if paths.is_empty() {
        return Err("--remove needs a worktree path".into());
    }

    let inventory = shear::scan(config)?;
    for note in &inventory.notes {
        eprintln!("shear: {note}");
    }

    // Resolve and check everything before removing anything. check() is pure
    // exactly so this pre-flight is possible: one refused path in a batch stops
    // the whole batch, rather than leaving the user to work out which half ran.
    let mut selected = Vec::new();
    let mut refusals = Vec::new();
    for path in &paths {
        let Some(candidate) = shear::resolve(&inventory, path) else {
            refusals.push((
                path.clone(),
                Refusal::Unknown {
                    path: path.display().to_string(),
                },
            ));
            continue;
        };
        match check(candidate, permissions) {
            Ok(()) => selected.push(candidate),
            Err(refusal) => refusals.push((candidate.path().to_path_buf(), refusal)),
        }
    }

    if !refusals.is_empty() {
        for (path, refusal) in &refusals {
            match refusal {
                // This one already names the path, and saying it twice reads
                // like a stutter rather than emphasis.
                Refusal::Unknown { .. } => eprintln!("shear: {}", refusal.about(path)),
                _ => eprintln!(
                    "shear: refusing {}: {}",
                    path.display(),
                    refusal.about(path)
                ),
            }
        }
        return Err(format!(
            "{} of {} selected {} refused; nothing was removed",
            refusals.len(),
            paths.len(),
            plural(paths.len(), "worktree", "worktrees")
        )
        .into());
    }

    // Say what is about to happen, in full, before any of it happens.
    println!(
        "shear: removing {} {}:",
        selected.len(),
        plural(selected.len(), "worktree", "worktrees")
    );
    for candidate in &selected {
        println!(
            "  {} [{}] via {}{}{}",
            candidate.path().display(),
            candidate.branch().unwrap_or("no branch"),
            route_label(route_for(candidate)),
            match candidate.dirt.total() {
                0 => String::new(),
                files => format!(
                    ", discarding {files} uncommitted {}",
                    plural(files, "file", "files")
                ),
            },
            prunable_note(candidate),
        );
    }

    // The socket is opened only if something actually needs it, so a plain
    // shell run with --repo never reports a missing herdr it did not want.
    let needs_herdr = selected
        .iter()
        .any(|candidate| route_for(candidate) == RemovalRoute::Herdr);
    let mut client = if needs_herdr {
        match Herdr::connect() {
            Ok(client) => Some(client),
            Err(err) => {
                eprintln!("shear: herdr is not reachable ({err})");
                None
            }
        }
    } else {
        None
    };

    let mut failures = 0usize;
    for candidate in selected {
        match remove_one(candidate, permissions, client.as_mut(), config) {
            Ok(record) => {
                println!("removed {}", record.path);
                println!("  restore with: {}", record.restore_command);
            }
            Err(err) => {
                failures += 1;
                eprintln!("shear: {}: {err}", candidate.path().display());
            }
        }
    }

    if failures > 0 {
        return Err(format!(
            "{failures} {} not removed",
            plural(failures, "worktree was", "worktrees were")
        )
        .into());
    }
    Ok(())
}

/// `--undo-log`.
pub fn run_undo_log() -> Result<()> {
    let records = read_log()?;
    if records.is_empty() {
        println!(
            "shear: no removals recorded in {}",
            config::undo_log().display()
        );
        return Ok(());
    }
    for record in records {
        println!(
            "{}  {}  [{}]  {}",
            record.at,
            record.path,
            record.branch.as_deref().unwrap_or("no branch"),
            record.verdict
        );
        if let Some(oid) = &record.head_oid {
            println!("  head was {oid}");
        }
        println!("  removed via {}", record.route);
        println!("  restore with: {}", record.restore_command);
    }
    Ok(())
}

/// The paths and permissions `--remove` was given.
///
/// The acknowledgement is a flag whose *name* carries the number —
/// `--i-understand-7-files` — so it cannot be typed once and reused: the count
/// has to be read off the row before the flag can be spelled.
pub fn parse_remove_args(args: &[String]) -> Result<(Vec<PathBuf>, Permissions)> {
    let mut paths = Vec::new();
    let mut permissions = Permissions::default();

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix("--remove=") {
            paths.push(PathBuf::from(value));
        } else if arg == "--remove" {
            let value = rest.next().ok_or("--remove needs a worktree path")?;
            paths.push(PathBuf::from(value));
        } else if arg == "--force-dirty" {
            permissions.force_dirty = true;
        } else if arg == "--close-workspace" {
            permissions.close_workspace = true;
        } else if let Some(count) = acknowledged_count(arg) {
            let count = count?;
            if permissions
                .acknowledged_files
                .is_some_and(|earlier| earlier != count)
            {
                return Err(format!(
                    "two different acknowledgements were given ({} and {count} files); \
                     say the number once",
                    permissions.acknowledged_files.unwrap_or_default()
                )
                .into());
            }
            permissions.acknowledged_files = Some(count);
        }
    }

    Ok((paths, permissions))
}

/// `--i-understand-<N>-files`, or `None` for anything that is not that flag.
/// A malformed count is an error rather than a silent miss: the user is looking
/// straight at it.
fn acknowledged_count(arg: &str) -> Option<Result<usize>> {
    let digits = arg.strip_prefix(ACK_PREFIX)?.strip_suffix(ACK_SUFFIX)?;
    Some(
        digits
            .parse::<usize>()
            .map_err(|err| format!("{arg}: {err}").into()),
    )
}

/// The clause the plan line adds for a worktree git reports as prunable, so a
/// user is not told that a directory is about to be removed when it is already
/// gone and only git's admin entry survives. `0 B reclaimed` and `gone` are
/// different claims.
///
/// git reports `prunable` with a reason ("gitdir file points to non-existent
/// location") but is not obliged to, so the two states are rendered as two
/// sentences rather than one with an empty parenthesis in it.
pub fn prunable_note(candidate: &Candidate) -> String {
    match &candidate.worktree.prunable {
        None => String::new(),
        Some(prunable) => match &prunable.reason {
            Some(reason) => format!("; the checkout is already gone ({reason})"),
            None => "; the checkout is already gone, and git gave no reason".to_string(),
        },
    }
}

fn route_label(route: RemovalRoute) -> &'static str {
    match route {
        RemovalRoute::Herdr => "herdr (the workspace is closed too)",
        RemovalRoute::Git => "git",
    }
}

fn record_for(candidate: &Candidate, route: RemovalRoute) -> RemovalRecord {
    RemovalRecord {
        at: rfc3339_utc(SystemTime::now()),
        path: candidate.path().to_string_lossy().into_owned(),
        repo_root: candidate.worktree.repo_root.to_string_lossy().into_owned(),
        branch: candidate.branch().map(str::to_string),
        head_oid: candidate.worktree.head_oid.clone(),
        route: match route {
            RemovalRoute::Herdr => "herdr".to_string(),
            RemovalRoute::Git => "git".to_string(),
        },
        classes: candidate
            .classes
            .iter()
            .map(|class| class.label().to_string())
            .collect(),
        verdict: candidate.verdict.label().to_string(),
        // A pending or failed measurement contributes nothing rather than a
        // plausible zero; `Gone` genuinely reclaims nothing.
        bytes_reclaimed: match candidate.size {
            Size::Bytes(bytes) => Some(bytes),
            Size::Gone => Some(0),
            Size::Pending | Size::Failed => None,
        },
        restore_command: restore_command(candidate),
    }
}

/// Runs one git command with an explicitly resolved binary and a timeout.
///
/// Unlike `git::run` this one is allowed to change a repository, which is why it
/// lives here: `git.rs` carries a read-only claim that `tests/read_only.rs`
/// checks, and a mutating command must not sit behind it.
fn run_git(args: &[&str], timeout: Duration) -> Result<()> {
    let git = git_binary()?;
    let mut child = Command::new(&git)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A repository's own environment must not redirect the command: the
        // worktree we were told to remove is the one that goes.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", git.display()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        format!("git {} timed out after {timeout:?}", args.join(" ")).into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("git {}: {err}", args.join(" ")).into()),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    // git's own message names the worktree and the reason; nothing this crate
    // could write in its place would be more useful.
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git {} failed ({})", args.join(" "), output.status).into()
    } else {
        stderr.into()
    })
}

/// Absolute path to the `git` binary. herdr runs plugin commands with no shell
/// and a minimal `PATH`, so this searches `PATH` explicitly and falls back to
/// the usual locations.
fn git_binary() -> Result<PathBuf> {
    if let Some(path) = config::non_empty_env("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("git");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    for fallback in ["/usr/bin/git", "/bin/git", "/usr/local/bin/git"] {
        let candidate = PathBuf::from(fallback);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("cannot find a `git` binary on PATH or in the usual locations".into())
}

/// Single-quotes anything a shell would not take literally, so a path with a
/// space in it survives being pasted back.
fn shell_quote(raw: &str) -> String {
    let safe = !raw.is_empty()
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '@' | '+'));
    if safe {
        return raw.to_string();
    }
    format!("'{}'", raw.replace('\'', r"'\''"))
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// RFC 3339 in UTC, without pulling in a date library for one field.
/// Civil-from-days is Howard Hinnant's, which is exact for the whole range we
/// can be handed.
fn rfc3339_utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_and_a_known_instant_render_as_rfc_3339() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_utc(UNIX_EPOCH + Duration::from_secs(1_786_830_847)),
            "2026-08-15T21:54:07Z"
        );
    }

    #[test]
    fn a_path_with_a_space_survives_being_pasted_into_a_shell() {
        assert_eq!(shell_quote("/tmp/wt-safe"), "/tmp/wt-safe");
        assert_eq!(shell_quote("/tmp/wt safe"), "'/tmp/wt safe'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
