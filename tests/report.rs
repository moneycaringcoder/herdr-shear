//! End-to-end coverage for the versioned CI report.

#[path = "fixtures.rs"]
mod fixtures;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use shear::config::{self, Config, StaleRule, StaleWhen};
use shear::model::{Class, Size};
use shear::{disk, report, shear as pipeline};

use fixtures::Fixture;

/// Herdr context and the plugin directories are process-global, so tests that
/// set them run one at a time even though cargo runs them on separate threads.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Leaves no Herdr context marker and makes the ordinary fallback socket miss.
fn no_herdr() {
    std::env::remove_var("HERDR_SOCKET_PATH");
    std::env::remove_var("HERDR_PLUGIN_ID");
    std::env::set_var(
        "XDG_CONFIG_HOME",
        "/nonexistent/shear-report-standalone-xdg",
    );
}

fn config_for(repo: &Path) -> Config {
    Config {
        only_repos: vec![repo.to_path_buf()],
        integration_ref: Some("main".into()),
        ..Config::default()
    }
}

fn document(config: &Config) -> serde_json::Value {
    let mut inventory = pipeline::scan(config).expect("scan");
    if config.measure_disk {
        disk::measure_all(&mut inventory, &AtomicBool::new(false));
    }
    report::to_json(&inventory)
}

fn repository_at<'a>(document: &'a serde_json::Value, root: &Path) -> &'a serde_json::Value {
    document["repositories"]
        .as_array()
        .expect("repositories array")
        .iter()
        .find(|repository| repository["root"] == root.to_string_lossy().as_ref())
        .unwrap_or_else(|| panic!("{} is not in the report", root.display()))
}

fn stale_row_at<'a>(document: &'a serde_json::Value, path: &Path) -> &'a serde_json::Value {
    document["repositories"]
        .as_array()
        .expect("repositories array")
        .iter()
        .flat_map(|repository| repository["stale"].as_array().expect("stale array").iter())
        .find(|row| row["path"] == path.to_string_lossy().as_ref())
        .unwrap_or_else(|| panic!("{} is not in the stale report", path.display()))
}

struct IsolatedDir {
    path: PathBuf,
    variable: &'static str,
    previous: Option<OsString>,
}

impl IsolatedDir {
    fn new(tag: &str, variable: &'static str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("SHEAR_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "shear-report-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create isolated plugin directory");
        let previous = std::env::var_os(variable);
        std::env::set_var(variable, &path);
        Self {
            path,
            variable,
            previous,
        }
    }
}

impl Drop for IsolatedDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var(self.variable, previous),
            None => std::env::remove_var(self.variable),
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn document_is_one_versioned_json_object() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-schema");
    let encoded = serde_json::to_string(&document(&config_for(&fixture.repo))).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&encoded).expect("parse report");

    assert!(parsed.is_object(), "the report is one JSON object");
    assert_eq!(parsed["schema_version"], 2);
    assert!(parsed["generated_at"]
        .as_str()
        .is_some_and(|timestamp| timestamp.ends_with('Z')));
}

#[test]
fn repositories_keep_their_own_counts_and_stale_rows() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-two-repos");
    let _safe = fixture.safe_worktree("safe");
    let stale = fixture.stale_worktree("stale", 45);
    let _dirty = fixture.dirty_worktree("dirty");
    let foreign = fixture.foreign_repo("foreign");
    let config = Config {
        only_repos: vec![fixture.repo.clone(), foreign.clone()],
        integration_ref: Some("main".into()),
        measure_disk: false,
        ..Config::default()
    };
    let report = document(&config);

    assert_eq!(
        report["repositories"]
            .as_array()
            .expect("repositories")
            .len(),
        2
    );
    let primary = repository_at(&report, &fixture.repo);
    assert_eq!(primary["worktree_count"], 4);
    assert_eq!(primary["verdicts"]["safe"], 1);
    assert_eq!(primary["verdicts"]["review"], 2);
    assert_eq!(primary["verdicts"]["keep"], 0);
    assert_eq!(primary["verdicts"]["blocked"], 1);

    let other = repository_at(&report, &foreign);
    assert_eq!(other["worktree_count"], 1);
    assert_eq!(other["verdicts"]["safe"], 0);
    assert_eq!(other["verdicts"]["review"], 0);
    assert_eq!(other["verdicts"]["keep"], 0);
    assert_eq!(other["verdicts"]["blocked"], 1);

    let occurrences = report["repositories"]
        .as_array()
        .expect("repositories")
        .iter()
        .flat_map(|repository| repository["stale"].as_array().expect("stale rows"))
        .filter(|row| row["path"] == stale.to_string_lossy().as_ref())
        .count();
    assert_eq!(occurrences, 1, "a row belongs to exactly one repository");
}

#[test]
fn stale_rows_carry_age_and_fresh_rows_are_absent() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-age");
    let stale = fixture.stale_worktree("old", 45);
    let fresh = fixture.safe_worktree("fresh");
    let report = document(&Config {
        measure_disk: false,
        ..config_for(&fixture.repo)
    });

    assert_eq!(stale_row_at(&report, &stale)["age_days"], 45);
    assert!(report["repositories"]
        .as_array()
        .expect("repositories")
        .iter()
        .flat_map(|repository| repository["stale"].as_array().expect("stale rows"))
        .all(|row| row["path"] != fresh.to_string_lossy().as_ref()));
}

#[test]
fn unknown_tip_time_is_null_and_never_a_plausible_zero() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-unknown-age");
    let path = fixture.broken_head_worktree("unknown");
    let mut inventory = pipeline::scan(&Config {
        measure_disk: false,
        ..config_for(&fixture.repo)
    })
    .expect("scan");
    let candidate = inventory
        .candidates
        .iter_mut()
        .find(|candidate| candidate.path() == path)
        .expect("unknown-tip worktree");
    assert!(
        candidate.last_commit.is_none(),
        "the tip time is genuinely unknown"
    );
    // The classifier correctly refuses to call an unknown tip stale. Marking the
    // row stale here isolates the projection's null contract from that policy.
    candidate.classes.insert(Class::Stale);

    let report = report::to_json(&inventory);
    let age = &stale_row_at(&report, &path)["age_days"];
    assert!(age.is_null());
    assert_ne!(age, 0);
}

#[test]
fn skipped_size_is_null_without_being_reported_as_pending_or_failed() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-skipped");
    let stale = fixture.stale_worktree("stale", 45);
    let report = document(&Config {
        measure_disk: false,
        ..config_for(&fixture.repo)
    });

    let row = stale_row_at(&report, &stale);
    assert!(row["bytes"].is_null());
    assert_ne!(row["bytes"], 0);
    assert_eq!(row["size_state"], "skipped");
    let repository = repository_at(&report, &fixture.repo);
    assert_eq!(repository["total_unmeasured"], 0);
    assert_eq!(repository["total_skipped"], repository["worktree_count"]);
}

#[test]
fn stale_rows_project_every_size_state_explicitly() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-size-states");
    let stale = fixture.stale_worktree("stale", 45);
    let mut inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    let cases = [
        (Size::Pending, "pending", serde_json::Value::Null),
        (Size::Skipped, "skipped", serde_json::Value::Null),
        (
            Size::Provisional(41),
            "provisional",
            serde_json::Value::Null,
        ),
        (Size::Bytes(42), "measured", serde_json::json!(42)),
        (Size::Gone, "gone", serde_json::json!(0)),
        (Size::Failed, "failed", serde_json::Value::Null),
    ];

    for (size, state, bytes) in cases {
        inventory
            .candidates
            .iter_mut()
            .find(|candidate| candidate.path() == stale)
            .expect("stale row")
            .size = size;
        let report = report::to_json(&inventory);
        let row = stale_row_at(&report, &stale);
        assert_eq!(row["size_state"], state, "{size:?}");
        assert_eq!(row["bytes"], bytes, "{size:?}");
    }
}

#[test]
fn protected_stale_worktree_is_reported_as_protected() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-protected");
    let protected = fixture.stale_worktree("protected", 45);
    let config_dir = IsolatedDir::new("protected-config", "HERDR_PLUGIN_CONFIG_DIR");
    std::fs::write(
        config_dir.path.join("config.json"),
        r#"{"protect":["protected-branch"],"measure_disk":false}"#,
    )
    .expect("write config.json");
    let args = vec![
        "--repo".to_string(),
        fixture.repo.to_string_lossy().into_owned(),
        "--integration-ref".to_string(),
        "main".to_string(),
    ];
    let config = config::load_with_args(&args).expect("load protected config");

    let row = stale_row_at(&document(&config), &protected).clone();
    assert_eq!(row["protected"], true);
    assert!(row["classes"]
        .as_array()
        .expect("classes")
        .iter()
        .any(|class| class == "protected"));
}

#[test]
fn merged_stale_row_reports_positive_merge_answer() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-stale-rule");
    let stale = fixture.stale_worktree("merged-old", 10);
    fixture.git(
        &fixture.repo,
        &[
            "merge",
            "-q",
            "--no-ff",
            "merged-old-branch",
            "-m",
            "merge old",
        ],
    );
    let config = Config {
        only_repos: vec![fixture.repo.clone()],
        integration_ref: Some("main".into()),
        stale_days: 30,
        stale_rules: vec![StaleRule {
            when: StaleWhen::Merged,
            days: 2,
        }],
        measure_disk: false,
        ..Config::default()
    };

    let row = stale_row_at(&document(&config), &stale).clone();
    assert_eq!(row["path"], stale.to_string_lossy().as_ref());
    assert_eq!(row["merged"], true);
    assert_eq!(row["merged_against"], "main");
}

#[test]
fn unmerged_stale_row_reports_negative_merge_answer() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-unmerged-answer");
    let stale = fixture.stale_worktree("unmerged-old", 45);
    let report = document(&Config {
        measure_disk: false,
        ..config_for(&fixture.repo)
    });

    let row = stale_row_at(&report, &stale);
    assert_eq!(row["merged"], false);
    assert_eq!(row["merged_against"], "main");
}

#[test]
fn unanswerable_merge_question_is_null_and_never_false() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-unknown-merge");
    let repo = fixture.no_integration_repo("orphaned");
    let report = document(&Config {
        only_repos: vec![repo.clone()],
        stale_days: 0,
        measure_disk: false,
        ..Config::default()
    });

    let row = stale_row_at(&report, &repo);
    let merged = &row["merged"];
    assert!(merged.is_null(), "an unasked merge question is JSON null");
    assert_ne!(merged, false, "an unasked merge question is never false");
    assert!(row["merged_against"].is_null());
}

#[test]
fn running_report_changes_neither_undo_log_nor_checkouts() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-read-only");
    let safe = fixture.safe_worktree("safe");
    let stale = fixture.stale_worktree("stale", 45);
    let dirty = fixture.dirty_worktree("dirty");
    let state_dir = IsolatedDir::new("state", "HERDR_PLUGIN_STATE_DIR");
    let undo_log = state_dir.path.join("removed.jsonl");
    let before = b"pre-existing undo log bytes\n";
    std::fs::write(&undo_log, before).expect("seed undo log");

    report::run_report(&config_for(&fixture.repo)).expect("run report");

    assert_eq!(std::fs::read(&undo_log).expect("read undo log"), before);
    for checkout in [&fixture.repo, &safe, &stale, &dirty] {
        assert!(checkout.exists(), "{} still exists", checkout.display());
    }
}

#[test]
fn report_without_herdr_still_names_every_repository() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-no-herdr");
    let foreign = fixture.foreign_repo("foreign");
    let report = document(&Config {
        only_repos: vec![fixture.repo.clone(), foreign.clone()],
        integration_ref: Some("main".into()),
        measure_disk: false,
        ..Config::default()
    });

    assert!(report["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .any(|note| note
            .as_str()
            .is_some_and(|note| note.contains("herdr is not reachable"))));
    assert_eq!(repository_at(&report, &fixture.repo)["name"], "repo");
    assert_eq!(repository_at(&report, &foreign)["name"], "foreign");
}

#[test]
fn non_repository_scope_is_a_well_formed_empty_report_with_a_note() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    no_herdr();

    let fixture = Fixture::new("report-empty");
    let plain = fixture.origin.parent().expect("fixture root").join("plain");
    std::fs::create_dir_all(&plain).expect("create non-repository directory");
    let config = Config {
        only_repos: vec![plain.clone()],
        measure_disk: false,
        ..Config::default()
    };

    report::run_report(&config).expect("an empty report is successful");
    let report = document(&config);
    assert_eq!(report["schema_version"], 2);
    assert!(report["repositories"]
        .as_array()
        .expect("repositories")
        .is_empty());
    assert!(report["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .any(|note| note.as_str().is_some_and(|note| {
            note.contains("not a git repository") && note.contains(plain.to_string_lossy().as_ref())
        })));
}
