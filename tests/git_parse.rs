//! The parsers, driven by the bytes git actually printed.
//!
//! Every fixture in this file is either a file from `tests/capture/` — real
//! output, captured before the parser was written — or bytes taken live from a
//! real repository built by `tests/fixtures.rs`. A fake in the shape the parser
//! wants passes a whole suite while the parser is wrong, so there is exactly
//! one constructed record here: a bare `prunable` field, which git 2.53.0 does
//! not produce because it always supplies a reason. That test says so itself.
//!
//! The records that matter are the degenerate ones: a lock with no reason, a
//! HEAD of all zeroes that still carries a branch line, a bare repository with
//! neither, a newline inside a worktree path and inside a lock reason, a rename
//! record that owns two NUL-terminated fields, and an untracked path containing
//! a literal newline.

#[path = "fixtures.rs"]
mod fixtures;

use std::path::Path;
use std::time::{Duration, SystemTime};

use shear::git::{self, BranchRow};
use shear::model::{Head, LockInfo, PrunableInfo, RepoKey};

use fixtures::{pin_git_env, Fixture};

const TIMEOUT: Duration = Duration::from_secs(60);

const WORKTREE_LIST: &[u8] = include_bytes!("capture/worktree-list.z");
const WORKTREE_LIST_NEWLINE: &[u8] = include_bytes!("capture/worktree-list-newline.z");
const WORKTREE_LIST_BARE: &[u8] = include_bytes!("capture/worktree-list-bare.z");
const FOR_EACH_REF: &[u8] = include_bytes!("capture/for-each-ref.txt");
const FOR_EACH_REF_MERGED: &[u8] = include_bytes!("capture/for-each-ref-merged.txt");
const STATUS_V2: &[u8] = include_bytes!("capture/status-v2.z");

fn key() -> RepoKey {
    RepoKey("/captured/repo/.git".into())
}

fn parsed() -> Vec<shear::model::Worktree> {
    git::parse_worktree_list(WORKTREE_LIST, &key(), Path::new("/captured/repo"))
        .expect("the captured worktree list parses")
}

/// The captured records are keyed by the tail of their path, which is the only
/// stable handle: the capture carries the absolute paths of the machine it was
/// taken on.
fn by_suffix<'a>(list: &'a [shear::model::Worktree], suffix: &str) -> &'a shear::model::Worktree {
    list.iter()
        .find(|w| w.path.ends_with(suffix))
        .unwrap_or_else(|| {
            panic!(
                "no captured worktree ending in {suffix}; captured: {:?}",
                list.iter()
                    .map(|w| w.path.display().to_string())
                    .collect::<Vec<_>>()
            )
        })
}

// ---------------------------------------------------------------------------
// worktree list --porcelain -z
// ---------------------------------------------------------------------------

/// The capture has to keep containing the shapes this file claims to test, or
/// every assertion below silently becomes a test of nothing.
#[test]
fn the_capture_still_contains_the_shapes_under_test() {
    assert!(
        WORKTREE_LIST.windows(8).any(|w| w == b"\0locked\0"),
        "the capture no longer has a bare `locked` field"
    );
    assert!(
        WORKTREE_LIST
            .windows(45)
            .any(|w| w == b"HEAD 0000000000000000000000000000000000000000"),
        "the capture no longer has an all-zero HEAD"
    );
    assert!(
        STATUS_V2.contains(&b'\n'),
        "the capture no longer has a path with a literal newline, so it can no \
         longer catch a line-splitting parser"
    );
    // A parser that split this on newlines would see more entries than there are
    // records, which is precisely the bug.
    let lines = STATUS_V2.split(|b| *b == b'\n').count();
    let records = STATUS_V2
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .count();
    assert!(
        lines > 1 && lines != records,
        "line splitting and NUL splitting now agree, so this capture proves nothing"
    );

    // The newline capture has to keep its newlines, in both of the two places a
    // newline can reach a record.
    assert!(
        WORKTREE_LIST_NEWLINE.windows(8).any(|w| w == b"wt-new\nl"),
        "the newline capture no longer has a newline inside a worktree path"
    );
    assert!(
        WORKTREE_LIST_NEWLINE.windows(7).any(|w| w == b"think\na"),
        "the newline capture no longer has a newline inside a lock reason"
    );
    assert!(
        WORKTREE_LIST_BARE.windows(6).any(|w| w == b"\0bare\0"),
        "the bare capture no longer has a bare marker"
    );
    assert!(
        !WORKTREE_LIST_BARE.windows(5).any(|w| w == b"HEAD "),
        "a bare record must have no HEAD at all"
    );
}

#[test]
fn the_main_checkout_is_the_first_record_and_only_the_first() {
    let list = parsed();
    assert_eq!(list.len(), 11, "captured worktrees: {}", list.len());
    assert!(list[0].is_main);
    assert!(list[0].path.ends_with("repo"));
    assert_eq!(
        list.iter().filter(|w| w.is_main).count(),
        1,
        "exactly one record may be the main checkout"
    );
    assert_eq!(list[0].head, Head::Branch("main".into()));
    assert_eq!(
        list[0].head_oid.as_deref(),
        Some("1338d9a0776263fed7455760e9e973db9389a29e")
    );
    // Every record carries the repo identity it was parsed under, which is what
    // stops two repositories' worktrees from ever being compared.
    assert!(list.iter().all(|w| w.repo == key()));
    assert!(list
        .iter()
        .all(|w| w.repo_root == Path::new("/captured/repo")));
}

#[test]
fn a_lock_with_a_reason_keeps_the_reason() {
    let list = parsed();
    let locked = by_suffix(&list, "wt-locked");
    assert_eq!(
        locked.locked,
        Some(LockInfo {
            reason: Some("held for demo".into())
        })
    );
    assert_eq!(locked.head, Head::Branch("locked-branch".into()));
}

/// The record a parser that splits on the first space gets wrong: git prints the
/// bare word `locked` with nothing after it, and an empty-string reason is not
/// the same fact as no reason given.
#[test]
fn a_lock_with_no_reason_is_locked_with_no_reason_not_an_empty_one() {
    let list = parsed();
    let locked = by_suffix(&list, "wt-locked2");
    assert_eq!(locked.locked, Some(LockInfo { reason: None }));
    assert_ne!(
        locked.locked,
        Some(LockInfo {
            reason: Some(String::new())
        })
    );
}

#[test]
fn a_prunable_worktree_keeps_gits_own_reason() {
    let list = parsed();
    let prunable = by_suffix(&list, "wt-prunable");
    assert_eq!(
        prunable.prunable,
        Some(PrunableInfo {
            reason: Some("gitdir file points to non-existent location".into())
        })
    );
    // The branch line is still there, and still means what it says.
    assert_eq!(prunable.head, Head::Branch("goner-branch".into()));
    assert!(!prunable.is_main);
}

/// `prunable` gets the same shape as `locked` for the same reason, and this is
/// the half of it no capture can show: git 2.53.0 always supplies a reason, so a
/// bare `prunable` field is constructed here rather than captured. It is
/// deliberately the *only* synthetic record left in this file, and it exists
/// because "flagged, no reason given" and "the reason is the empty string" are
/// different facts that `Option<String>` alone cannot tell apart.
#[test]
fn a_bare_prunable_is_flagged_with_no_reason_not_an_empty_one() {
    let bytes = b"worktree /tmp/wt-gone\0HEAD 1338d9a0776263fed7455760e9e973db9389a29e\0\
branch refs/heads/gone-branch\0prunable\0\0" as &[u8];

    let list = git::parse_worktree_list(bytes, &key(), Path::new("/captured/repo"))
        .expect("a bare prunable field parses");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].prunable, Some(PrunableInfo { reason: None }));
    assert_ne!(
        list[0].prunable,
        Some(PrunableInfo {
            reason: Some(String::new())
        })
    );
    // The flag itself is the load-bearing half, and it survives either way.
    assert!(list[0].prunable.is_some());
}

#[test]
fn a_detached_worktree_has_a_commit_and_no_branch() {
    let list = parsed();
    let detached = by_suffix(&list, "wt-detached");
    assert_eq!(detached.head, Head::Detached);
    assert_eq!(detached.head.branch(), None);
    assert_eq!(
        detached.head_oid.as_deref(),
        Some("1338d9a0776263fed7455760e9e973db9389a29e"),
        "a detached HEAD is still classifiable, by commit"
    );
}

/// The trap the doc calls out: the record still carries
/// `branch refs/heads/broken-branch` even though that ref is gone, so the branch
/// line cannot be the discriminator.
#[test]
fn an_all_zero_head_is_unborn_even_though_git_still_prints_a_branch() {
    let list = parsed();
    let broken = by_suffix(&list, "wt-broken");
    assert_eq!(broken.head, Head::Unborn);
    assert_eq!(
        broken.head_oid, None,
        "there is no commit here, and reporting the all-zero oid as one would be a lie"
    );
    assert!(
        broken
            .notes
            .iter()
            .any(|note| note.contains("broken-branch")),
        "the branch git still reports has to reach the user: {:?}",
        broken.notes
    );
}

/// Records are framed by an empty NUL field, so nothing in a path or a lock
/// reason can end a record early. Real captured bytes: `worktree-list-newline.z`
/// was taken from a repository whose worktree path holds a literal newline and
/// whose second worktree is locked with a reason that holds another one. A
/// parser that splits on lines reports five worktrees where there are four, two
/// of them at paths that do not exist.
#[test]
fn a_newline_in_a_path_and_in_a_lock_reason_both_survive() {
    let list = git::parse_worktree_list(WORKTREE_LIST_NEWLINE, &key(), Path::new("/captured/repo"))
        .expect("a newline in a record is not a parse error");
    assert_eq!(list.len(), 4, "captured: {list:#?}");

    let odd = by_suffix(&list, "wt-new\nline");
    assert_eq!(
        odd.path
            .file_name()
            .expect("a final component")
            .to_string_lossy(),
        "wt-new\nline",
        "the newline is inside one path component, not a record boundary"
    );
    assert_eq!(odd.head, Head::Branch("newline-branch".into()));
    assert!(!odd.is_main);

    // git forbids control characters in ref names, so a newline can never reach
    // a `branch` line. The lock reason is the other place it can.
    let locked = by_suffix(&list, "wt-locked-nl");
    assert_eq!(
        locked.locked,
        Some(LockInfo {
            reason: Some("held while I think\nabout it".into())
        })
    );

    // The record after the newline one is still framed correctly, which is what
    // a line-splitting parser gets wrong downstream rather than at the newline.
    let plain = by_suffix(&list, "wt-plain");
    assert_eq!(plain.head, Head::Branch("plain-branch".into()));
    assert!(list[0].is_main, "the main checkout is still first");
    assert_eq!(list.iter().filter(|w| w.is_main).count(), 1);
}

#[test]
fn a_truncated_record_is_an_error_not_a_silently_shorter_list() {
    let broken = b"HEAD 1338d9a0776263fed7455760e9e973db9389a29e\0\0";
    let err = git::parse_worktree_list(broken, &key(), Path::new("/captured/repo"))
        .expect_err("a record that does not start with `worktree ` is not parseable");
    assert!(
        err.to_string().contains("worktree"),
        "the error has to name what was wrong: {err}"
    );

    let no_head = b"worktree /tmp/wt\0\0";
    let err = git::parse_worktree_list(no_head, &key(), Path::new("/captured/repo"))
        .expect_err("a record with no branch, detached or bare marker is not parseable");
    assert!(err.to_string().contains("/tmp/wt"), "{err}");
}

#[test]
fn an_empty_list_is_an_empty_list() {
    assert!(git::parse_worktree_list(b"", &key(), Path::new("/repo"))
        .expect("empty input parses")
        .is_empty());
}

/// A bare repository's record has no `HEAD` and no `branch`, so a parser that
/// assumes every record resolves to a commit reads it as a branch record with a
/// missing tip. Real captured bytes: `worktree-list-bare.z`.
#[test]
fn a_bare_record_is_bare() {
    let list =
        git::parse_worktree_list(WORKTREE_LIST_BARE, &key(), Path::new("/captured/bare.git"))
            .expect("the captured bare repository parses");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].head, Head::Bare);
    assert_eq!(
        list[0].head_oid, None,
        "there is no HEAD line to read an oid from"
    );
    assert_eq!(list[0].locked, None);
    assert_eq!(list[0].prunable, None);
    assert!(list[0].is_main);
    assert!(
        list[0].path.ends_with("bare.git"),
        "{}",
        list[0].path.display()
    );
}

// ---------------------------------------------------------------------------
// for-each-ref
// ---------------------------------------------------------------------------

fn branch<'a>(rows: &'a [BranchRow], name: &str) -> &'a BranchRow {
    rows.iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no branch row for {name}"))
}

#[test]
fn upstream_gone_is_the_literal_gone_string_and_nothing_else() {
    let rows = git::parse_branches(FOR_EACH_REF).expect("the captured for-each-ref parses");
    assert_eq!(rows.len(), 9);

    // `[gone]`: the branch tracks a ref that no longer exists. The only offline
    // evidence that the work landed somewhere.
    let safe = branch(&rows, "safe-branch");
    assert!(safe.upstream.gone);
    assert_eq!(
        safe.upstream.name.as_deref(),
        Some("refs/remotes/origin/safe-branch")
    );

    // A live upstream with nothing to report is not gone.
    let active = branch(&rows, "active-branch");
    assert!(!active.upstream.gone);
    assert_eq!(
        active.upstream.name.as_deref(),
        Some("refs/remotes/origin/active-branch")
    );
    assert_eq!((active.upstream.ahead, active.upstream.behind), (0, 0));

    // No upstream configured at all is a third state — "never pushed" — and is
    // not evidence of anything having landed.
    let stale = branch(&rows, "stale-branch");
    assert_eq!(stale.upstream.name, None);
    assert!(!stale.upstream.gone);
}

#[test]
fn ahead_and_behind_are_read_off_the_track_field() {
    let rows = git::parse_branches(FOR_EACH_REF).expect("parse");
    let main = branch(&rows, "main");
    assert_eq!((main.upstream.ahead, main.upstream.behind), (2, 0));
    assert!(!main.upstream.gone);
    assert_eq!(
        main.oid, "1338d9a0776263fed7455760e9e973db9389a29e",
        "the tip oid is what a detached worktree is tested against"
    );
}

#[test]
fn commit_times_come_back_as_instants() {
    let rows = git::parse_branches(FOR_EACH_REF).expect("parse");
    assert_eq!(
        branch(&rows, "stale-branch").tip,
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_704_067_200))
    );
    assert_eq!(
        branch(&rows, "main").tip,
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_830_847))
    );
}

#[test]
fn a_short_for_each_ref_line_is_an_error() {
    let err = git::parse_branches(b"only-a-name\tandanoid\n")
        .expect_err("a line with the wrong field count is not parseable");
    assert!(err.to_string().contains("fields"), "{err}");
}

/// The two captures were taken from one repository, so they have to agree: the
/// branch that `--merged=main` lists as contained is the same one whose upstream
/// is `[gone]`, and the branches it omits are the ones that are still live.
#[test]
fn the_merged_capture_agrees_with_the_upstream_capture() {
    let merged: Vec<&str> = std::str::from_utf8(FOR_EACH_REF_MERGED)
        .expect("utf-8")
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    assert!(merged.contains(&"safe-branch"));
    assert!(merged.contains(&"merged-branch"));
    assert!(
        !merged.contains(&"active-branch"),
        "an unmerged branch must not appear in the merged list"
    );
    assert!(!merged.contains(&"stale-branch"));

    let rows = git::parse_branches(FOR_EACH_REF).expect("parse");
    assert!(branch(&rows, "safe-branch").upstream.gone);
    assert!(!branch(&rows, "merged-branch").upstream.gone);
}

// ---------------------------------------------------------------------------
// status --porcelain=v2 -z
// ---------------------------------------------------------------------------

/// The framing rule: the `2` record owns the next NUL field as well. A parser
/// that does not consume it reads `README.md` as a status record, and a parser
/// that splits on newlines reports a file called `new` and another called
/// `line.txt`, neither of which exists.
#[test]
fn the_rename_record_consumes_two_fields_and_the_newline_path_stays_one_file() {
    let dirt = git::parse_status(STATUS_V2).expect("the captured status parses");
    assert_eq!(
        dirt,
        shear::model::Dirt {
            paths: 5,
            staged: 2,
            unstaged: 1,
            untracked: 3,
            unmerged: 0,
        },
        "captured: a staged rename whose worktree copy is modified (RM), a \
         staged add (A.), and three untracked files, one of which has a newline \
         in its name"
    );
    assert_eq!(dirt.total(), 5);
    assert!(dirt.is_dirty());
}

#[test]
fn one_path_changed_in_both_index_and_worktree_counts_once() {
    let dirt =
        git::parse_status(b"1 MM N... 100644 100644 100644 old-object new-object tracked.txt\0")
            .expect("an ordinary MM record parses");

    assert_eq!(dirt.paths, 1);
    assert_eq!(dirt.staged, 1);
    assert_eq!(dirt.unstaged, 1);
    assert_eq!(dirt.total(), 1);
    assert!(dirt.is_dirty());
}

#[test]
fn a_rename_record_with_no_original_path_is_an_error() {
    // The captured rename record with its second field lopped off.
    let truncated = b"2 RM N... 100644 100644 100644 e3a7cb0694e9c558c90290ee89f6bde48b6250fe \
e3a7cb0694e9c558c90290ee89f6bde48b6250fe R100 READYOU.md\0" as &[u8];
    let err = git::parse_status(truncated).expect_err("a truncated rename record is not parseable");
    assert!(err.to_string().contains("original-path"), "{err}");
}

#[test]
fn an_unknown_record_type_is_an_error_not_a_zero() {
    let err = git::parse_status(b"z what is this\0").expect_err("unknown record types are loud");
    assert!(err.to_string().contains("unrecognised"), "{err}");
}

#[test]
fn a_clean_worktree_has_no_dirt() {
    assert_eq!(
        git::parse_status(b"").expect("empty status parses"),
        shear::model::Dirt::default()
    );
}

/// Header records only appear with `--branch`, which the scan does not pass, but
/// a parser that chokes on one is a parser waiting to break. Real bytes, from a
/// real repository, with the flag that produces them.
#[test]
fn header_records_are_skipped_and_unmerged_paths_are_counted() {
    pin_git_env();
    let fixture = Fixture::new("status-records");
    let conflicted = fixture.conflicted_worktree("clash");
    fixture.tricky_untracked(&conflicted);

    let bytes = git::run(
        &conflicted,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--renames",
        ],
        TIMEOUT,
    )
    .expect("status");
    assert!(
        bytes.starts_with(b"# branch."),
        "git no longer emits header records for --branch"
    );

    let dirt = git::parse_status(&bytes).expect("parse a status with headers and a conflict");
    assert_eq!(
        dirt.unmerged, 1,
        "the conflicted path is unmerged, not merely modified"
    );
    assert_eq!(
        dirt.untracked, 3,
        "a space, a quote and a literal newline in three untracked names"
    );
    assert!(dirt.is_dirty());
}

/// Ignored files are not work at risk. `target/` is not a reason to keep a
/// worktree, and `--untracked-files=all` does not list them without `--ignored`
/// anyway — this pins the parser's half of that.
#[test]
fn ignored_records_are_not_dirt() {
    let dirt = git::parse_status(b"! target/debug/thing\0! .env\0").expect("parse");
    assert_eq!(dirt, shear::model::Dirt::default());
    assert!(!dirt.is_dirty());
}
