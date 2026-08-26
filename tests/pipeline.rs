//! End-to-end: a real repository in, a classified inventory out.
//!
//! The other test files each prove one layer. This one proves the seams between
//! them, which is where a split like this actually breaks: a parser that is
//! right and a classifier that is right can still disagree about whether a
//! branch name carries its `refs/heads/` prefix, and no single-layer test can
//! see that.
//!
//! No running herdr is required. `scan` degrades to "no workspace information"
//! with a note when the socket is unreachable, and these tests assert that it
//! says so rather than silently reporting an empty session.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, OnceLock};

use shear::config::Config;
use shear::model::{Class, Merged, Size, Verdict};
use shear::{disk, shear as pipeline};

use fixtures::Fixture;

/// `HERDR_SOCKET_PATH` is process-global, so the tests that set it run one at a
/// time even though cargo runs them on separate threads.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Points the socket client at nothing, so the scan runs with no herdr.
fn no_herdr() {
    std::env::set_var("HERDR_SOCKET_PATH", "/nonexistent/shear-pipeline-test.sock");
}

fn config_for(repo: &Path) -> Config {
    Config {
        only_repos: vec![repo.to_path_buf()],
        // Pinned rather than inherited: `origin/HEAD` is not set in a fixture,
        // and a test that silently fell back to a guess would not be testing
        // what it claims to.
        integration_ref: Some("main".into()),
        ..Config::default()
    }
}

fn verdict_at(inventory: &shear::model::Inventory, path: &Path) -> Verdict {
    inventory
        .find(path)
        .unwrap_or_else(|| panic!("{} is not in the inventory", path.display()))
        .verdict
}

fn classes_at(inventory: &shear::model::Inventory, path: &Path) -> BTreeSet<Class> {
    inventory
        .find(path)
        .unwrap_or_else(|| panic!("{} is not in the inventory", path.display()))
        .classes
        .clone()
}

#[test]
fn every_class_survives_the_whole_pipeline() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("pipeline");
    let safe = fixture.safe_worktree("safe");
    let merged = fixture.merged_worktree("merged");
    let active = fixture.active_worktree("active");
    let dirty = fixture.dirty_worktree("dirty");
    let stale = fixture.stale_worktree("stale", 90);
    let locked = fixture.locked_worktree("locked", "held for a postmortem");
    let detached = fixture.detached_worktree("detached");
    let prunable = fixture.prunable_worktree("prunable");

    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");

    // The main checkout is present and blocked, always.
    assert_eq!(verdict_at(&inventory, &fixture.repo), Verdict::Blocked);
    assert!(
        inventory.find(&fixture.repo).map(|c| c.worktree.is_main) == Some(true),
        "the repo root must be recognised as the main checkout"
    );

    assert_eq!(verdict_at(&inventory, &safe), Verdict::Safe);
    assert_eq!(verdict_at(&inventory, &merged), Verdict::Review);
    // Tested and found unmerged is a positive answer, not an absent one.
    assert_eq!(
        inventory.find(&active).expect("active").merged,
        Merged::No("main".into())
    );
    assert_eq!(
        inventory.find(&safe).expect("safe").merged,
        Merged::Into("main".into())
    );
    assert_eq!(verdict_at(&inventory, &active), Verdict::Keep);
    assert_eq!(verdict_at(&inventory, &stale), Verdict::Review);
    assert_eq!(verdict_at(&inventory, &locked), Verdict::Blocked);
    assert_eq!(verdict_at(&inventory, &prunable), Verdict::Review);

    // A dirty worktree is never safe, whatever else is true of it.
    assert_ne!(verdict_at(&inventory, &dirty), Verdict::Safe);
    assert!(classes_at(&inventory, &dirty).contains(&Class::Dirty));

    // A detached HEAD has no branch, so it can be neither merged-by-name nor
    // gone-upstream. It is here to prove the pipeline does not panic or invent
    // a branch for it.
    let detached_row = inventory.find(&detached).expect("detached worktree");
    assert_eq!(detached_row.branch(), None);
    assert_ne!(detached_row.verdict, Verdict::Safe);

    assert_eq!(
        inventory.safe().count(),
        1,
        "exactly one worktree in this fixture satisfies every safe condition, got: {:?}",
        inventory
            .safe()
            .map(|c| c.path().display().to_string())
            .collect::<Vec<_>>()
    );

    // Precisely the class combination the safe rule requires, and nothing else
    // standing in for it.
    let safe_classes = classes_at(&inventory, &safe);
    assert!(safe_classes.contains(&Class::Merged));
    assert!(safe_classes.contains(&Class::GoneUpstream));
    assert!(!safe_classes.contains(&Class::Dirty));
    assert!(!safe_classes.contains(&Class::Locked));
}

#[test]
fn a_repo_with_no_integration_ref_has_no_safe_worktrees() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("no-integration");
    let safe = fixture.safe_worktree("safe");

    // A ref that does not resolve anywhere. The merged question becomes
    // unanswerable, which is not the same as answering it "no".
    let config = Config {
        only_repos: vec![fixture.repo.clone()],
        integration_ref: Some("refs/heads/does-not-exist".into()),
        ..Config::default()
    };
    let inventory = pipeline::scan(&config).expect("scan");

    let row = inventory.find(&safe).expect("worktree");
    assert_eq!(
        row.merged,
        Merged::Unknown,
        "an unresolvable integration ref must leave merged unknown, not `No`"
    );
    assert_eq!(row.merged.as_bool(), None);
    assert!(!row.classes.contains(&Class::Merged));
    assert_eq!(row.verdict, Verdict::Review);
    assert_eq!(inventory.safe().count(), 0);
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains("integration ref")),
        "the user must be told the question could not be asked: {:?}",
        inventory.notes
    );
}

#[test]
fn an_unreachable_herdr_is_a_note_and_not_an_empty_session() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("no-herdr");
    fixture.safe_worktree("safe");

    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");

    assert!(
        !inventory.candidates.is_empty(),
        "a missing socket must not empty the scan"
    );
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains("herdr is not reachable")),
        "blindness must be reported, not swallowed: {:?}",
        inventory.notes
    );
    // With no herdr, nothing can be known to be open, so nothing is blocked for
    // that reason.
    assert!(inventory
        .candidates
        .iter()
        .all(|c| !c.classes.contains(&Class::OpenInHerdr)));
}

#[test]
fn worktrees_of_different_repositories_are_never_mixed() {
    let _guard = env_lock();
    no_herdr();

    let a = Fixture::new("repo-a");
    let a_safe = a.safe_worktree("safe");
    let b = Fixture::new("repo-b");
    let b_active = b.active_worktree("active");

    let config = Config {
        only_repos: vec![a.repo.clone(), b.repo.clone()],
        integration_ref: Some("main".into()),
        ..Config::default()
    };
    let inventory = pipeline::scan(&config).expect("scan");

    assert_eq!(inventory.repos.len(), 2, "two distinct repositories");
    let a_key = &inventory.find(&a_safe).expect("a").worktree.repo;
    let b_key = &inventory.find(&b_active).expect("b").worktree.repo;
    assert_ne!(a_key, b_key);
    assert!(
        inventory.repos.iter().all(|r| inventory
            .candidates
            .iter()
            .filter(|c| c.worktree.repo == r.key)
            .all(|c| c.worktree.repo_root == r.root)),
        "every candidate must sit under the repo root it was found in"
    );
}

#[test]
fn a_measured_size_is_bytes_a_missing_one_is_gone_and_neither_is_a_plausible_zero() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("sizes");
    let dirty = fixture.dirty_worktree("dirty");
    let prunable = fixture.prunable_worktree("prunable");

    let mut inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    assert!(
        inventory.candidates.iter().all(|c| c.size == Size::Pending),
        "a scan must not measure eagerly"
    );

    disk::measure_all(&mut inventory, &AtomicBool::new(false));

    match inventory.find(&dirty).expect("dirty").size {
        Size::Bytes(bytes) => assert!(bytes > 0, "a real checkout occupies something"),
        other => panic!("expected a measurement, got {other:?}"),
    }
    assert_eq!(
        inventory.find(&prunable).expect("prunable").size,
        Size::Gone,
        "a worktree whose directory is gone reclaims nothing, which is not the same as zero bytes"
    );
}

#[test]
fn json_reports_unknown_as_null_rather_than_as_false() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("json");
    let safe = fixture.safe_worktree("safe");

    let config = Config {
        only_repos: vec![fixture.repo.clone()],
        integration_ref: Some("refs/heads/does-not-exist".into()),
        ..Config::default()
    };
    let inventory = pipeline::scan(&config).expect("scan");
    let json = pipeline::to_json(&inventory);

    let row = json["worktrees"]
        .as_array()
        .expect("worktrees array")
        .iter()
        .find(|row| row["path"] == safe.to_string_lossy().as_ref())
        .expect("the safe worktree is in the JSON");
    assert!(
        row["merged"].is_null(),
        "an unanswerable question is null, so a consumer cannot read it as `not merged`"
    );
    assert!(row["merged_against"].is_null());
    assert_eq!(row["verdict"], "review");
}

#[test]
fn a_user_supplied_path_resolves_through_a_symlink_and_a_trailing_dot() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("resolve");
    let safe = fixture.safe_worktree("safe");

    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");

    assert!(pipeline::resolve(&inventory, &safe).is_some());
    let with_dot = PathBuf::from(format!("{}/.", safe.display()));
    assert!(
        pipeline::resolve(&inventory, &with_dot).is_some(),
        "a path the user typed with a trailing `.` names the same worktree"
    );
    assert!(
        pipeline::resolve(&inventory, Path::new("/nonexistent/nowhere")).is_none(),
        "an unknown path must not resolve to something"
    );
}

// ---------------------------------------------------------------------------
// The size cache: last run's figures, provisional next time
// ---------------------------------------------------------------------------

#[test]
fn a_measured_size_comes_back_provisional_and_the_walk_still_replaces_it() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("size-cache");
    let worktree = fixture.active_worktree("cached");
    let cache = fixture.root().join("sizes.jsonl");

    // First run: measure and remember.
    let mut inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    disk::measure_all(&mut inventory, &AtomicBool::new(false));
    let Size::Bytes(measured) = inventory.find(&worktree).expect("row").size else {
        panic!("the walk must have measured the worktree");
    };
    disk::remember(&inventory, &cache);

    // Second run: the remembered figure arrives provisional on pending rows,
    // marked as a claim rather than a measurement, and rows the cache does not
    // know stay pending.
    let mut second = pipeline::scan(&config_for(&fixture.repo)).expect("rescan");
    let unknown = fixture.active_worktree("uncached");
    let mut third = pipeline::scan(&config_for(&fixture.repo)).expect("third scan");
    disk::recall(&mut third, &cache);
    assert_eq!(
        third.find(&worktree).expect("row").size,
        Size::Provisional(measured)
    );
    assert_eq!(third.find(&unknown).expect("new row").size, Size::Pending);

    // The walk still runs and replaces the claim with a measurement.
    disk::measure_all(&mut third, &AtomicBool::new(false));
    assert!(matches!(
        third.find(&worktree).expect("row").size,
        Size::Bytes(_)
    ));

    // And a provisional figure counts in no total: it goes to the unknown
    // bucket, exactly like pending.
    disk::recall(&mut second, &cache);
    let provisional_row = second.find(&worktree).expect("row");
    let (bytes, unknown_count) = pipeline::reclaimable(std::iter::once(provisional_row));
    assert_eq!(bytes, 0);
    assert_eq!(unknown_count, 1);
}

#[test]
fn the_cache_survives_corruption_and_sheds_paths_that_are_gone() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("size-cache-hygiene");
    let kept = fixture.active_worktree("kept");
    let doomed = fixture.active_worktree("doomed");
    let cache = fixture.root().join("sizes.jsonl");

    let mut inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    disk::measure_all(&mut inventory, &AtomicBool::new(false));
    disk::remember(&inventory, &cache);

    // A corrupt line loses one figure, never the file.
    let mut raw = std::fs::read_to_string(&cache).expect("read the cache");
    raw.insert_str(0, "not json at all\n");
    std::fs::write(&cache, raw).expect("corrupt the cache");
    let mut recalled = pipeline::scan(&config_for(&fixture.repo)).expect("rescan");
    disk::recall(&mut recalled, &cache);
    assert!(matches!(
        recalled.find(&kept).expect("kept").size,
        Size::Provisional(_)
    ));

    // A remembered path that no longer exists on disk is dropped on the next
    // write, which is what keeps the file from growing forever.
    fixture.git(
        &fixture.repo,
        &[
            "worktree",
            "remove",
            "--force",
            doomed.to_str().expect("utf-8 fixture path"),
        ],
    );
    let mut after = pipeline::scan(&config_for(&fixture.repo)).expect("scan after removal");
    disk::measure_all(&mut after, &AtomicBool::new(false));
    disk::remember(&after, &cache);
    let written = std::fs::read_to_string(&cache).expect("read the cache back");
    assert!(
        written.contains(kept.to_str().expect("utf-8")),
        "the surviving checkout keeps its figure: {written}"
    );
    assert!(
        !written.contains(doomed.to_str().expect("utf-8")),
        "the removed checkout is shed: {written}"
    );
    assert!(
        !written.contains("not json"),
        "the corrupt line is not carried forward: {written}"
    );
}
