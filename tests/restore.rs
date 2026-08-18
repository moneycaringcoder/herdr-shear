//! Restoring removed checkouts without creating or moving branches.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use shear::config::Config;
use shear::model::{
    Candidate, Dirt, Head, Merged, RemovalRecord, RepoKey, Size, Upstream, Verdict, Worktree,
};
use shear::remove::{
    read_log_numbered, remove_one, restore_one, LoggedRemoval, Permissions, RestoreOutcome,
};

#[path = "fixtures.rs"]
mod fixtures;
use fixtures::Fixture;

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn arrange() -> &'static Path {
    fixtures::pin_git_env();
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = scratch_root().join(format!("state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch state directory");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
        dir
    })
}

fn scratch_root() -> PathBuf {
    std::env::var_os("SHEAR_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("shear-restore")
}

fn config() -> Config {
    Config {
        git_timeout: Duration::from_secs(30),
        ..Config::default()
    }
}

fn candidate(path: &Path, repo_root: &Path, branch: &str, oid: &str) -> Candidate {
    Candidate {
        worktree: Worktree {
            repo: RepoKey(repo_root.to_string_lossy().into_owned()),
            repo_root: repo_root.to_path_buf(),
            path: path.to_path_buf(),
            head: Head::Branch(branch.to_string()),
            head_oid: Some(oid.to_string()),
            is_main: false,
            locked: None,
            prunable: None,
            notes: Vec::new(),
        },
        dirt: Dirt::default(),
        upstream: Upstream::default(),
        merged: Merged::Unknown,
        last_commit: None,
        open_workspace: None,
        classes: BTreeSet::new(),
        verdict: Verdict::Safe,
        size: Size::Bytes(0),
        reason: "fixture is safe".to_string(),
    }
}

fn remove_and_find(fixture: &Fixture, path: &Path, branch: &str) -> (LoggedRemoval, String) {
    let oid = fixture.git(path, &["rev-parse", "HEAD"]);
    remove_one(
        &candidate(path, &fixture.repo, branch, &oid),
        Permissions::default(),
        None,
        &config(),
    )
    .expect("remove the fixture worktree");
    let entry = read_log_numbered()
        .expect("read the undo log")
        .into_iter()
        .find(|entry| entry.record.path == path.to_string_lossy())
        .expect("find this test's removal by its path");
    (entry, oid)
}

fn record(path: &Path, repo_root: &Path, head_oid: Option<&str>) -> RemovalRecord {
    RemovalRecord {
        at: "2026-08-18T00:00:00Z".to_string(),
        path: path.to_string_lossy().into_owned(),
        repo_root: repo_root.to_string_lossy().into_owned(),
        branch: Some("hand-built-branch".to_string()),
        head_oid: head_oid.map(str::to_string),
        route: "git".to_string(),
        classes: Vec::new(),
        verdict: "safe".to_string(),
        bytes_reclaimed: None,
        restore_command: "not executed".to_string(),
    }
}

#[test]
fn a_removed_worktree_is_restored_on_its_unchanged_branch_at_the_same_oid() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-on-branch");
    let path = fixture.safe_worktree("restored");
    let branch = "restored-branch";
    let (entry, oid) = remove_and_find(&fixture, &path, branch);
    assert!(!path.exists(), "the checkout was removed first");

    let outcome = restore_one(&entry, &config()).expect("restore the checkout");

    assert_eq!(
        outcome,
        RestoreOutcome::OnBranch {
            branch: branch.to_string(),
            oid: oid.clone(),
        }
    );
    assert!(path.is_dir(), "the checkout is back at its recorded path");
    assert_eq!(fixture.git(&path, &["rev-parse", "HEAD"]), oid);
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", branch]),
        oid,
        "restoring did not move the branch"
    );
}

#[test]
fn a_second_restore_refuses_the_existing_path_and_leaves_that_checkout_alone() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-twice");
    let path = fixture.safe_worktree("twice");
    let (entry, _) = remove_and_find(&fixture, &path, "twice-branch");
    restore_one(&entry, &config()).expect("first restore");
    fixture.write(&path, "left-alone.txt", "still here\n");

    let err = restore_one(&entry, &config()).expect_err("the existing path is refused");

    assert!(err.to_string().starts_with(&format!("#{}: ", entry.id)));
    assert!(err.to_string().contains("already exists"));
    assert_eq!(
        std::fs::read_to_string(path.join("left-alone.txt")).expect("read marker"),
        "still here\n",
        "the checkout already at that path was not touched"
    );
}

#[test]
fn a_deleted_branch_is_not_recreated_and_the_checkout_is_restored_detached() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-deleted-branch");
    let path = fixture.safe_worktree("deleted");
    let branch = "deleted-branch";
    let (entry, oid) = remove_and_find(&fixture, &path, branch);
    fixture.git(&fixture.repo, &["branch", "-D", branch]);

    let outcome = restore_one(&entry, &config()).expect("restore detached");

    assert_eq!(fixture.git(&path, &["rev-parse", "HEAD"]), oid);
    assert_eq!(
        fixture.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    assert!(matches!(
        outcome,
        RestoreOutcome::Detached { oid: ref restored, ref why }
            if restored == &oid && why.contains("does not create a branch")
    ));
    assert!(
        fixture
            .try_git(
                &fixture.repo,
                &["show-ref", "--verify", &format!("refs/heads/{branch}")],
            )
            .is_err(),
        "restore must not recreate the deleted branch"
    );
}

#[test]
fn a_recorded_branch_name_beginning_with_a_dash_restores_detached_and_reports_the_dash_problem() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-dash-branch");
    let path = fixture.safe_worktree("dash");
    let (mut entry, oid) = remove_and_find(&fixture, &path, "dash-source-branch");
    let branch = "-recorded-branch";
    fixture.git(
        &fixture.repo,
        &["update-ref", &format!("refs/heads/{branch}"), &oid],
    );
    entry.record.branch = Some(branch.to_string());

    let outcome = restore_one(&entry, &config()).expect("restore detached");

    assert_eq!(fixture.git(&path, &["rev-parse", "HEAD"]), oid);
    assert_eq!(
        fixture.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    assert!(matches!(
        outcome,
        RestoreOutcome::Detached {
            oid: ref restored,
            ref why,
        } if restored == &oid
            && why.contains("begins with a dash")
            && why.contains("cannot be handed to git")
            && !why.contains("no longer exists")
    ));
}

#[test]
fn a_tag_sharing_a_deleted_branch_name_restores_the_recorded_oid_detached_without_a_branch() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-tag-collision");
    let path = fixture.safe_worktree("tag-collision");
    let branch = "tag-collision-branch";
    let (entry, oid) = remove_and_find(&fixture, &path, branch);
    fixture.git(&fixture.repo, &["branch", "-D", branch]);
    fixture.git(&fixture.repo, &["tag", branch, &oid]);

    let outcome = restore_one(&entry, &config()).expect("restore detached");

    assert_eq!(fixture.git(&path, &["rev-parse", "HEAD"]), oid);
    assert_eq!(
        fixture.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    assert!(matches!(
        outcome,
        RestoreOutcome::Detached { oid: ref restored, .. } if restored == &oid
    ));
    assert!(
        fixture
            .try_git(
                &fixture.repo,
                &["show-ref", "--verify", &format!("refs/heads/{branch}")],
            )
            .is_err(),
        "restore must not create a branch that merely shares the tag's name"
    );
}

#[test]
fn a_moved_branch_is_not_moved_back_and_the_checkout_uses_the_recorded_oid_detached() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-moved-branch");
    let path = fixture.safe_worktree("moved");
    let branch = "moved-branch";
    let (entry, recorded_oid) = remove_and_find(&fixture, &path, branch);
    fixture.git(&fixture.repo, &["checkout", "-q", branch]);
    fixture.append(&fixture.repo, "README.md", "newer branch work\n");
    fixture.git(&fixture.repo, &["add", "-A"]);
    fixture.commit(&fixture.repo, "move the branch");
    let newer_oid = fixture.git(&fixture.repo, &["rev-parse", "HEAD"]);
    fixture.git(&fixture.repo, &["checkout", "-q", "main"]);

    let outcome = restore_one(&entry, &config()).expect("restore detached");

    assert_eq!(fixture.git(&path, &["rev-parse", "HEAD"]), recorded_oid);
    assert_eq!(
        fixture.git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    assert!(matches!(
        outcome,
        RestoreOutcome::Detached { ref oid, ref why }
            if oid == &recorded_oid && why.contains(&newer_oid) && why.contains("does not move a branch")
    ));
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", branch]),
        newer_oid,
        "restore must leave the moved branch at its newer commit"
    );
}

#[test]
fn a_record_without_a_head_oid_has_nothing_to_restore_to() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-no-head");
    let entry = LoggedRemoval {
        id: 41,
        record: record(&fixture.root().join("never-born"), &fixture.repo, None),
    };

    let err = restore_one(&entry, &config()).expect_err("an unborn checkout cannot be restored");

    assert!(err.to_string().starts_with("#41: "));
    assert!(err.to_string().contains("nothing to restore to"));
}

#[test]
fn an_existing_recorded_path_is_refused() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-existing-path");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "HEAD"]);
    let entry = LoggedRemoval {
        id: 42,
        record: record(&fixture.repo, &fixture.repo, Some(&oid)),
    };

    let err = restore_one(&entry, &config()).expect_err("an existing path cannot be overwritten");

    assert!(err.to_string().starts_with("#42: "));
    assert!(err.to_string().contains("already exists"));
    assert!(fixture.repo.join("README.md").is_file());
}

#[test]
fn numbered_log_records_are_newest_first_with_physical_one_based_ids() {
    let _guard = test_lock();
    arrange();
    let fixture = Fixture::new("restore-numbered-log");
    let older_path = fixture.safe_worktree("numbered-older");
    let newer_path = fixture.safe_worktree("numbered-newer");
    let (older, _) = remove_and_find(&fixture, &older_path, "numbered-older-branch");
    let (newer, _) = remove_and_find(&fixture, &newer_path, "numbered-newer-branch");

    let entries = read_log_numbered().expect("read numbered log");
    let found_older = entries
        .iter()
        .find(|entry| entry.record.path == older_path.to_string_lossy())
        .expect("find the older record by path");
    let found_newer = entries
        .iter()
        .find(|entry| entry.record.path == newer_path.to_string_lossy())
        .expect("find the newer record by path");
    assert_eq!(found_older.id, older.id);
    assert_eq!(found_newer.id, newer.id);
    assert!(found_newer.id > found_older.id);
    let older_position = entries
        .iter()
        .position(|entry| entry.record.path == older_path.to_string_lossy())
        .expect("older position");
    let newer_position = entries
        .iter()
        .position(|entry| entry.record.path == newer_path.to_string_lossy())
        .expect("newer position");
    assert!(
        newer_position < older_position,
        "newest records are returned first"
    );

    let raw = std::fs::read_to_string(shear::config::undo_log()).expect("read raw undo log");
    let non_empty_lines = raw.lines().filter(|line| !line.trim().is_empty()).count();
    assert_eq!(
        entries.len(),
        non_empty_lines,
        "every non-empty log line must be readable before physical ids are compared"
    );
    assert_eq!(
        entries.iter().map(|entry| entry.id).max(),
        Some(non_empty_lines),
        "the maximum id is the physical one-based line number"
    );
}
