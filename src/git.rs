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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use crate::model::{Dirt, Head, LockInfo, PrunableInfo, Repo, RepoKey, Upstream, Worktree};
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

/// The 40-zero oid `git worktree list` prints for a worktree whose HEAD does not
/// resolve.
const NULL_OID: &str = "0000000000000000000000000000000000000000";

/// Canonical identity for a repository: the absolute, canonicalized
/// `--git-common-dir`. All worktrees of one repo share it; each has its own
/// `--git-dir`. Do not use `--git-dir`, `--show-toplevel`, or the directory
/// name.
pub fn repo_key(path: &Path, timeout: Duration) -> Result<RepoKey> {
    let common = common_dir(path, timeout)?;
    Ok(RepoKey(tidy(&common).to_string_lossy().into_owned()))
}

/// Resolves any path inside a repository to the repo it belongs to.
///
/// Returns `Ok(None)` — not an error — when the path is simply not in a git
/// repository, because "this workspace is not a repo" is ordinary data.
pub fn repo_at(path: &Path, timeout: Duration) -> Result<Option<Repo>> {
    let raw = run_raw(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
    )?;
    if !raw.success {
        // git says this loudly and unambiguously, and it is the one failure that
        // is not a failure. Anything else — an unreadable directory, a corrupt
        // repository, a `safe.directory` refusal — is reported.
        if raw.stderr_text().contains("not a git repository") {
            return Ok(None);
        }
        return Err(raw.into_error(path, &["rev-parse", "--git-common-dir"]));
    }

    let common = tidy(&PathBuf::from(text(&raw.stdout).trim()));
    let key = RepoKey(common.to_string_lossy().into_owned());
    let root = main_worktree_root(path, timeout)?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    Ok(Some(Repo { key, root, name }))
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
    let key = repo_key(repo_root, timeout)?;
    let bytes = run(
        repo_root,
        &["worktree", "list", "--porcelain", "-z"],
        timeout,
    )?;
    let mut list = parse_worktree_list(&bytes, &key, repo_root)?;

    // `HEAD 0000…` is two different situations that need different words, and
    // only the worktree's own reflog tells them apart. This is I/O, so it lives
    // here rather than in the parser.
    for worktree in &mut list {
        if worktree.head != Head::Unborn {
            continue;
        }
        match has_head_reflog(&worktree.path, timeout) {
            Ok(true) => worktree.notes.push(format!(
                "{NOTE_BROKEN_HEAD}: the branch this worktree was on has been deleted, so its \
                 HEAD no longer resolves"
            )),
            Ok(false) => worktree.notes.push(format!(
                "{NOTE_UNBORN}: this worktree has never had a commit checked out"
            )),
            Err(err) => worktree.notes.push(format!(
                "could not tell an unborn worktree from a broken one here ({err})"
            )),
        }
    }
    Ok(list)
}

/// Parses the `-z` porcelain body of `git worktree list`. Split out so
/// `tests/git_parse.rs` can drive it with captured bytes, including a worktree
/// path containing a newline.
pub fn parse_worktree_list(
    bytes: &[u8],
    repo: &RepoKey,
    repo_root: &Path,
) -> Result<Vec<Worktree>> {
    let mut out: Vec<Worktree> = Vec::new();
    let mut record: Vec<&[u8]> = Vec::new();

    // Records are separated by an *empty* NUL field, and the stream ends with
    // one too, so the final `split` element is an empty tail that flushes
    // nothing. Splitting on newlines instead would corrupt any path, lock reason
    // or prunable reason containing one.
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !record.is_empty() {
                out.push(parse_worktree_record(
                    &record,
                    repo,
                    repo_root,
                    out.is_empty(),
                )?);
                record.clear();
            }
            continue;
        }
        record.push(field);
    }
    if !record.is_empty() {
        // git always terminates the last record, but a truncated stream must not
        // silently lose a worktree.
        out.push(parse_worktree_record(
            &record,
            repo,
            repo_root,
            out.is_empty(),
        )?);
    }
    Ok(out)
}

fn parse_worktree_record(
    record: &[&[u8]],
    repo: &RepoKey,
    repo_root: &Path,
    is_main: bool,
) -> Result<Worktree> {
    let path = match record[0].strip_prefix(b"worktree ") {
        Some(rest) => path_from_bytes(rest),
        None => {
            return Err(format!(
                "git worktree list record does not start with `worktree `: {:?}",
                text(record[0])
            )
            .into())
        }
    };

    let mut head_oid: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut bare = false;
    let mut locked: Option<LockInfo> = None;
    let mut prunable: Option<PrunableInfo> = None;
    let mut notes: Vec<String> = Vec::new();

    for field in &record[1..] {
        if let Some(rest) = field.strip_prefix(b"HEAD ") {
            head_oid = Some(text(rest).into_owned());
        } else if let Some(rest) = field.strip_prefix(b"branch ") {
            let full = text(rest).into_owned();
            branch = Some(short_branch(&full));
        } else if *field == b"detached" {
            detached = true;
        } else if *field == b"bare" {
            bare = true;
        } else if let Some(reason) = optional_reason(field, b"locked") {
            locked = Some(LockInfo { reason });
        } else if let Some(reason) = optional_reason(field, b"prunable") {
            // Same shape as `locked`, for the same reason: the flag is the
            // load-bearing half, and "flagged with no reason" is a different
            // fact from "the reason is the empty string". git 2.53.0 always
            // supplies one here, but it does not promise to.
            prunable = Some(PrunableInfo { reason });
        } else {
            // A newer git may add attributes. Carrying them through as notes is
            // better than dropping them silently or refusing to parse.
            notes.push(format!("unrecognised worktree attribute: {}", text(field)));
        }
    }

    let unborn = head_oid.as_deref() == Some(NULL_OID);
    let head = if bare {
        Head::Bare
    } else if unborn {
        if let Some(name) = &branch {
            notes.push(format!(
                "HEAD does not resolve, though git still reports branch {name}"
            ));
        }
        Head::Unborn
    } else if detached {
        Head::Detached
    } else if let Some(name) = branch.clone() {
        Head::Branch(name)
    } else {
        return Err(format!(
            "git worktree list record for {} has neither `branch`, `detached` nor `bare`",
            path.display()
        )
        .into());
    };

    // A bare record has no HEAD at all, and an unborn one has no commit; in both
    // cases there is no oid to record and inventing one would be a lie.
    if matches!(head, Head::Bare | Head::Unborn) {
        head_oid = None;
    }

    Ok(Worktree {
        repo: repo.clone(),
        repo_root: repo_root.to_path_buf(),
        path,
        head,
        head_oid,
        is_main,
        locked,
        prunable,
        notes,
    })
}

/// `locked`/`prunable` carry an optional reason on the same field. The bare word
/// with nothing after it means "no reason given", which is not the same as an
/// empty reason, so it is `Some(None)` rather than `Some(Some(""))`.
fn optional_reason(field: &[u8], keyword: &[u8]) -> Option<Option<String>> {
    let rest = field.strip_prefix(keyword)?;
    match rest.first() {
        None => Some(None),
        Some(b' ') => Some(Some(text(&rest[1..]).into_owned())),
        // `lockedfoo` is a different attribute, not a lock.
        Some(_) => None,
    }
}

/// Per-branch upstream state and tip commit time, from one `for-each-ref` over
/// `refs/heads/`.
///
/// `%(upstream:track)` reports the literal `[gone]` for a branch configured to
/// track a ref that no longer exists. That string is the detection; do not try
/// to infer it from a missing remote ref, which also happens when the remote has
/// simply never been fetched.
pub fn branches(repo_root: &Path, timeout: Duration) -> Result<Vec<BranchRow>> {
    let bytes = run(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)%09%(objectname)%09%(upstream)%09%(upstream:track)\
             %09%(committerdate:unix)",
            "refs/heads/",
        ],
        timeout,
    )?;
    parse_branches(&bytes)
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
    let mut out = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let line = text(line);
        // Ref names cannot contain a tab (git forbids every ASCII control
        // character in a ref), so the separator is unambiguous.
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
        if fields.len() != 5 {
            return Err(format!(
                "for-each-ref line has {} tab-separated fields, expected 5: {line:?}",
                fields.len()
            )
            .into());
        }

        let track = fields[3].trim();
        let (ahead, behind) = parse_track(track)?;
        let tip =
            match fields[4].trim() {
                "" => None,
                seconds => Some(
                    SystemTime::UNIX_EPOCH
                        + Duration::from_secs(seconds.parse::<u64>().map_err(|err| {
                            format!("for-each-ref committerdate {seconds:?}: {err}")
                        })?),
                ),
            };

        out.push(BranchRow {
            name: fields[0].to_string(),
            oid: fields[1].to_string(),
            upstream: Upstream {
                name: (!fields[2].is_empty()).then(|| fields[2].to_string()),
                gone: track == "[gone]",
                ahead,
                behind,
            },
            tip,
        });
    }
    Ok(out)
}

/// `%(upstream:track)`: empty, `[gone]`, `[ahead 2]`, `[behind 3]` or
/// `[ahead 1, behind 2]`.
fn parse_track(track: &str) -> Result<(u32, u32)> {
    let inner = match track.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        Some(inner) => inner,
        None if track.is_empty() => return Ok((0, 0)),
        None => return Err(format!("unrecognised %(upstream:track) value {track:?}").into()),
    };
    if inner == "gone" {
        return Ok((0, 0));
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in inner.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n
                .trim()
                .parse::<u32>()
                .map_err(|err| format!("{part:?}: {err}"))?;
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n
                .trim()
                .parse::<u32>()
                .map_err(|err| format!("{part:?}: {err}"))?;
        } else {
            return Err(format!("unrecognised %(upstream:track) part {part:?}").into());
        }
    }
    Ok((ahead, behind))
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
    // A ref the user typed that does not resolve is an error, not a fallback:
    // silently scanning against `origin/main` instead would answer a question
    // they did not ask.
    if let Some(reference) = configured.map(str::trim).filter(|r| !r.is_empty()) {
        return if resolves(repo_root, reference, timeout)? {
            Ok(Some(reference.to_string()))
        } else {
            Err(format!(
                "the configured integration ref {reference} does not resolve in {}",
                repo_root.display()
            )
            .into())
        };
    }

    // `origin/HEAD` is the only authoritative answer, so it is asked for by name
    // and reported as whatever it points at.
    let symbolic = run_raw(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        timeout,
    )?;
    if symbolic.success {
        let target = text(&symbolic.stdout).trim().to_string();
        if !target.is_empty() && resolves(repo_root, &target, timeout)? {
            return Ok(Some(target));
        }
    } else if resolves(repo_root, "origin/HEAD", timeout)? {
        // Set, but not symbolic. Still authoritative; just not nameable.
        return Ok(Some("origin/HEAD".to_string()));
    }

    for guess in crate::config::DEFAULT_BRANCH_GUESSES {
        if resolves(repo_root, guess, timeout)? {
            return Ok(Some(guess.to_string()));
        }
    }
    Ok(None)
}

/// Whether a rev names a commit in this repository.
fn resolves(repo_root: &Path, reference: &str, timeout: Duration) -> Result<bool> {
    let spec = format!("{reference}^{{commit}}");
    let raw = run_raw(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &spec],
        timeout,
    )?;
    if raw.success {
        return Ok(true);
    }
    // `--quiet` turns "no such ref" into a bare exit 1. Anything else is a real
    // failure and is reported rather than read as "no".
    if raw.code == Some(1) {
        return Ok(false);
    }
    Err(raw.into_error(repo_root, &["rev-parse", "--verify", &spec]))
}

/// Short names of every local branch contained in `integration_ref`, from
/// `for-each-ref --merged=<ref> refs/heads/`.
pub fn merged_branches(
    repo_root: &Path,
    integration_ref: &str,
    timeout: Duration,
) -> Result<Vec<String>> {
    let merged = format!("--merged={integration_ref}");
    let bytes = run(
        repo_root,
        &[
            "for-each-ref",
            &merged,
            "--format=%(refname:short)",
            "refs/heads/",
        ],
        timeout,
    )?;
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| text(line).into_owned())
        .collect())
}

/// Whether an arbitrary commit is contained in `integration_ref`, for a detached
/// HEAD that has no branch to look up.
pub fn is_ancestor(
    repo_root: &Path,
    oid: &str,
    integration_ref: &str,
    timeout: Duration,
) -> Result<bool> {
    let raw = run_raw(
        repo_root,
        &["merge-base", "--is-ancestor", oid, integration_ref],
        timeout,
    )?;
    match raw.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        // 128 is a bad object or a bad ref, which is a question we could not
        // ask rather than a "no".
        _ => Err(raw.into_error(
            repo_root,
            &["merge-base", "--is-ancestor", oid, integration_ref],
        )),
    }
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
    if !worktree.exists() {
        return Ok(Dirt::default());
    }
    let bytes = run(
        worktree,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--renames",
        ],
        timeout,
    )?;
    parse_status(&bytes)
}

/// Parses the `-z` porcelain v2 body of `git status`.
pub fn parse_status(bytes: &[u8]) -> Result<Dirt> {
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut dirt = Dirt::default();
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        match field[0] {
            // Header lines only appear with `--branch`/`--show-stash`, but a
            // parser that chokes on one is a parser waiting to break.
            b'#' => {}
            b'1' => {
                let (staged, unstaged) = xy(field)?;
                dirt.staged += usize::from(staged);
                dirt.unstaged += usize::from(unstaged);
                dirt.paths += 1;
            }
            b'2' => {
                let (staged, unstaged) = xy(field)?;
                dirt.staged += usize::from(staged);
                dirt.unstaged += usize::from(unstaged);
                dirt.paths += 1;
                // The framing rule: a rename/copy record owns the *next* field
                // as well, which holds the original path. Not consuming it makes
                // the parser read a path as a status record.
                // The trailing NUL of the stream leaves an empty final field, so
                // "there is a next field" is not enough: a path is never empty,
                // and consuming the tail would swallow the missing one silently.
                match fields.get(index) {
                    Some(original) if !original.is_empty() => index += 1,
                    _ => {
                        return Err(
                            "status porcelain=v2 rename record has no original-path field".into(),
                        )
                    }
                }
            }
            b'u' => {
                dirt.unmerged += 1;
                dirt.paths += 1;
            }
            b'?' => {
                dirt.untracked += 1;
                dirt.paths += 1;
            }
            // Ignored files are not work at risk; `target/` is not a reason to
            // keep a worktree.
            b'!' => {}
            _ => {
                return Err(
                    format!("unrecognised status porcelain=v2 record: {:?}", text(field)).into(),
                )
            }
        }
    }
    Ok(dirt)
}

/// `X` (index) and `Y` (worktree) of a `1` or `2` record, each reported as
/// "changed" when it is not `.`.
fn xy(field: &[u8]) -> Result<(bool, bool)> {
    // `<kind> <XY> …`
    if field.len() < 4 || field[1] != b' ' {
        return Err(format!("malformed status record: {:?}", text(field)).into());
    }
    let x = field[2];
    let y = field[3];
    if field.len() > 4 && field[4] != b' ' {
        return Err(format!("malformed status record: {:?}", text(field)).into());
    }
    Ok((x != b'.', y != b'.'))
}

/// Whether a worktree's own HEAD reflog exists. This is the discriminator
/// between an unborn branch and one deleted underneath the worktree; a worktree
/// that ever had a commit checked out has `logs/HEAD`, a freshly initialised one
/// does not.
pub fn has_head_reflog(worktree: &Path, timeout: Duration) -> Result<bool> {
    // Verified on git 2.53.0: `rev-parse HEAD@{0}` is *not* a substitute — it
    // exits 1 in a worktree whose branch was deleted, precisely the case this
    // has to detect, because HEAD itself no longer resolves.
    let raw = run_raw(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        timeout,
    )?;
    if !raw.success {
        return Err(raw.into_error(worktree, &["rev-parse", "--git-dir"]));
    }
    let git_dir = PathBuf::from(text(&raw.stdout).trim());
    Ok(git_dir.join("logs").join("HEAD").exists())
}

/// Runs one git command, read-only, with a timeout, an explicitly resolved
/// binary, and an environment scrubbed of anything that would let a repository's
/// own config change what we do.
///
/// Returns the raw stdout bytes on success. On a non-zero exit the error carries
/// git's stderr, because a git message that names the ref is more useful than
/// anything this crate could write in its place.
pub fn run(repo_or_worktree: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let raw = run_raw(repo_or_worktree, args, timeout)?;
    if raw.success {
        Ok(raw.stdout)
    } else {
        Err(raw.into_error(repo_or_worktree, args))
    }
}

/// One finished git invocation, exit code and all. Private because every caller
/// that cares about a specific non-zero code interprets it here, in this module,
/// next to the command it belongs to.
struct RawOutput {
    code: Option<i32>,
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl RawOutput {
    fn stderr_text(&self) -> String {
        text(&self.stderr).trim().to_string()
    }

    fn into_error(self, cwd: &Path, args: &[&str]) -> Box<dyn std::error::Error> {
        let status = match self.code {
            Some(code) => format!("exit {code}"),
            None => "killed by a signal".to_string(),
        };
        let stderr = self.stderr_text();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        format!(
            "git {} in {} failed ({status}){detail}",
            args.join(" "),
            cwd.display()
        )
        .into()
    }
}

fn run_raw(cwd: &Path, args: &[&str], timeout: Duration) -> Result<RawOutput> {
    let git = git_binary()?;
    let mut command = Command::new(&git);
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Anything that could redirect git at a different repository, index or
    // object store is removed: an inherited `GIT_DIR` from the pane herdr
    // spawned us in would otherwise silently answer about the wrong repo.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_INDEX_VERSION",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_PREFIX",
        "GIT_CEILING_DIRECTORIES",
        "GIT_EXTERNAL_DIFF",
        "GIT_ASKPASS",
        "GIT_EDITOR",
    ] {
        command.env_remove(key);
    }
    // The env form of `--no-optional-locks`, applied to every call rather than
    // only to `status`, so a future command added here cannot quietly reacquire
    // the right to write.
    command.env("GIT_OPTIONAL_LOCKS", "0");
    // Nothing here may block on a human: no credential prompt, no pager.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_PAGER", "cat");
    // `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` are deliberately left alone: a
    // user's own git config is a fact about their repository, not interference.

    let mut child = command
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", git.display()))?;
    let mut child_stdout = child.stdout.take().expect("stdout was piped");
    let mut child_stderr = child.stderr.take().expect("stderr was piped");

    let deadline = Instant::now() + timeout;
    // Both pipes are drained on their own threads: a command that fills the
    // stderr pipe while we wait on stdout would deadlock, and `git status` in a
    // large worktree produces enough of both to matter.
    let (stdout, stderr, status, timed_out) = std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut buffer = Vec::new();
            let _ = child_stdout.read_to_end(&mut buffer);
            buffer
        });
        let err = scope.spawn(move || {
            let mut buffer = Vec::new();
            let _ = child_stderr.read_to_end(&mut buffer);
            buffer
        });

        let mut status = None;
        let mut timed_out = false;
        let mut nap = Duration::from_millis(1);
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
            std::thread::sleep(nap);
            nap = (nap * 2).min(Duration::from_millis(20));
        }

        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
            status,
            timed_out,
        )
    });

    if timed_out {
        return Err(format!(
            "git {} in {} did not finish within {:?}",
            args.join(" "),
            cwd.display(),
            timeout
        )
        .into());
    }
    let status = status.ok_or_else(|| {
        format!(
            "could not wait for git {} in {}",
            args.join(" "),
            cwd.display()
        )
    })?;

    Ok(RawOutput {
        code: status.code(),
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Absolute path to the `git` binary. herdr runs plugin commands with a minimal
/// `PATH`, so this searches `PATH` explicitly and falls back to the usual
/// locations, failing loudly rather than letting every git call fail one by one
/// with a confusing message.
pub fn git_binary() -> Result<PathBuf> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED.get_or_init(find_git).clone().ok_or_else(|| {
        format!(
            "could not find a `git` binary on PATH ({}) or in any of {:?}",
            std::env::var("PATH").unwrap_or_else(|_| "<unset>".into()),
            FALLBACK_GIT
        )
        .into()
    })
}

/// Where to look when `PATH` is empty or does not contain git, which is what a
/// plugin spawned by herdr actually sees.
const FALLBACK_GIT: [&str; 5] = [
    "/usr/bin/git",
    "/bin/git",
    "/usr/local/bin/git",
    "/opt/homebrew/bin/git",
    "/opt/local/bin/git",
];

fn find_git() -> Option<PathBuf> {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    FALLBACK_GIT
        .iter()
        .map(PathBuf::from)
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// `--git-common-dir`, absolute. Errors when the path is not in a repository,
/// unlike [`repo_at`], which treats that as data.
fn common_dir(path: &Path, timeout: Duration) -> Result<PathBuf> {
    let bytes = run(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
    )?;
    Ok(PathBuf::from(text(&bytes).trim()))
}

/// The main checkout of the repository `path` belongs to: the first record git
/// prints, which is the only reliable way to name it — `--show-toplevel` answers
/// about whichever linked worktree we happen to be standing in.
fn main_worktree_root(path: &Path, timeout: Duration) -> Result<PathBuf> {
    let bytes = run(path, &["worktree", "list", "--porcelain", "-z"], timeout)?;
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(rest) = field.strip_prefix(b"worktree ") {
            return Ok(path_from_bytes(rest));
        }
    }
    Err(format!(
        "git worktree list printed no worktree record for {}",
        path.display()
    )
    .into())
}

/// Canonicalizes when the path exists, so two spellings of one repository
/// compare equal, and leaves it alone when it does not — a prunable worktree's
/// path cannot be canonicalized and must still be reported.
fn tidy(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn short_branch(full: &str) -> String {
    full.strip_prefix("refs/heads/").unwrap_or(full).to_string()
}

/// Lossy text for a message or a name. Never used for a path — see
/// [`path_from_bytes`] — because a lossy path would not open.
fn text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}
