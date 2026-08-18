//! Staleness policy resolution and its end-to-end safety invariant.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use shear::config::{self, Config, StaleRule, StaleWhen};
use shear::model::{Merged, Upstream, Verdict};
use shear::shear as pipeline;

use fixtures::Fixture;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ConfigDir {
    path: PathBuf,
    previous: Option<OsString>,
}

impl ConfigDir {
    fn with_file(tag: &str, contents: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("SHEAR_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "shear-policy-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create isolated config directory");
        std::fs::write(path.join("config.json"), contents).expect("write config.json");
        let previous = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR");
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", &path);
        Self { path, previous }
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", previous),
            None => std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn load_config(tag: &str, contents: &str) -> (ConfigDir, Config) {
    let dir = ConfigDir::with_file(tag, contents);
    let config = config::load().expect("load config");
    (dir, config)
}

fn duration(days: u64) -> Duration {
    Duration::from_secs(days * 86_400)
}

fn upstream(gone: bool) -> Upstream {
    Upstream {
        gone,
        ..Upstream::default()
    }
}

fn rule(when: StaleWhen, days: u64) -> StaleRule {
    StaleRule { when, days }
}

#[test]
fn two_rule_file_resolves_merged_and_unmerged_thresholds() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let (_dir, config) = load_config(
        "two-rules",
        r#"{
            "stale_rules": [
                {"when": "merged", "days": 30},
                {"when": "unmerged", "days": 90}
            ]
        }"#,
    );

    assert_eq!(
        config.stale_after_for(&Merged::Into("main".into()), &upstream(false)),
        duration(30)
    );
    assert_eq!(
        config.stale_after_for(&Merged::No("main".into()), &upstream(false)),
        duration(90)
    );
}

#[test]
fn no_rules_use_stale_after_for_every_fact_combination() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let config = Config {
        stale_days: 37,
        ..Config::default()
    };
    let merged_states = [
        Merged::Into("main".into()),
        Merged::No("main".into()),
        Merged::Unknown,
    ];

    for merged in &merged_states {
        for gone in [false, true] {
            assert_eq!(
                config.stale_after_for(merged, &upstream(gone)),
                config.stale_after()
            );
        }
    }
}

#[test]
fn unknown_merge_state_matches_only_any() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let facts = Merged::Unknown;
    let upstream = upstream(false);

    for when in [StaleWhen::Merged, StaleWhen::Unmerged] {
        let config = Config {
            stale_days: 14,
            stale_rules: vec![rule(when, 3)],
            ..Config::default()
        };
        assert_eq!(
            config.stale_after_for(&facts, &upstream),
            config.stale_after()
        );
    }

    let config = Config {
        stale_rules: vec![rule(StaleWhen::Any, 3)],
        ..Config::default()
    };
    assert_eq!(config.stale_after_for(&facts, &upstream), duration(3));
}

#[test]
fn gone_matches_only_an_upstream_reported_gone() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let config = Config {
        stale_rules: vec![rule(StaleWhen::Gone, 5)],
        ..Config::default()
    };
    let merged = Merged::No("main".into());

    assert_eq!(
        config.stale_after_for(&merged, &upstream(false)),
        config.stale_after()
    );
    assert_eq!(
        config.stale_after_for(&merged, &upstream(true)),
        duration(5)
    );
}

#[test]
fn first_matching_rule_wins() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let config = Config {
        stale_rules: vec![rule(StaleWhen::Any, 2), rule(StaleWhen::Merged, 40)],
        ..Config::default()
    };

    assert_eq!(
        config.stale_after_for(&Merged::Into("main".into()), &upstream(false)),
        duration(2)
    );
}

#[test]
fn zero_day_rule_is_ignored_and_fallback_applies() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let (_dir, config) = load_config(
        "zero-days",
        r#"{
            "stale_days": 23,
            "stale_rules": [{"when": "merged", "days": 0}]
        }"#,
    );

    assert!(config.stale_rules.is_empty());
    assert_eq!(
        config.stale_after_for(&Merged::No("main".into()), &upstream(false)),
        config.stale_after()
    );
    assert_eq!(config.stale_after(), duration(23));
}

#[test]
fn malformed_stale_rules_use_every_default_without_failing() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let (_dir, config) = load_config(
        "malformed",
        r#"{
            "integration_ref": "main",
            "stale_days": 99,
            "measure_disk": false,
            "stale_rules": [{"when": 7, "days": 1}]
        }"#,
    );

    assert_eq!(config, Config::default());
}

#[test]
fn unknown_key_does_not_discard_valid_rules() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    let (_dir, config) = load_config(
        "unknown-key",
        r#"{
            "future_setting": {"enabled": true},
            "stale_rules": [{"when": "gone", "days": 11}]
        }"#,
    );

    assert_eq!(config.stale_rules, vec![rule(StaleWhen::Gone, 11)]);
    assert_eq!(
        config.stale_after_for(&Merged::Unknown, &upstream(true)),
        duration(11)
    );
}

fn verdict_rows(inventory: &shear::model::Inventory) -> BTreeSet<(PathBuf, Verdict)> {
    inventory
        .candidates
        .iter()
        .map(|candidate| (candidate.path().to_path_buf(), candidate.verdict))
        .collect()
}

fn assert_present(inventory: &shear::model::Inventory, path: &Path) {
    assert!(
        inventory.find(path).is_some(),
        "{} is missing from the inventory",
        path.display()
    );
}

#[test]
fn aggressive_policy_preserves_safe_set_but_moves_keep_to_review() {
    let _guard = env_lock();
    fixtures::pin_git_env();
    std::env::set_var(
        "HERDR_SOCKET_PATH",
        "/nonexistent/shear-policy-invariant.sock",
    );

    let fixture = Fixture::new("policy-invariant");
    let safe = fixture.safe_worktree("safe");
    let merged = fixture.merged_worktree("merged");
    let stale = fixture.stale_worktree("stale", 90);
    let active = fixture.active_worktree("active");
    let dirty = fixture.dirty_worktree("dirty");

    let baseline_config = Config {
        only_repos: vec![fixture.repo.clone()],
        integration_ref: Some("main".into()),
        // Keep the 90-day-old row below the fallback so the policy is the only
        // difference that can move it from keep to review.
        stale_days: 365,
        ..Config::default()
    };
    let baseline = pipeline::scan(&baseline_config).expect("scan without rules");
    let aggressive_config = Config {
        stale_rules: vec![rule(StaleWhen::Any, 1)],
        ..baseline_config.clone()
    };
    let aggressive = pipeline::scan(&aggressive_config).expect("scan with aggressive policy");

    for path in [&safe, &merged, &stale, &active, &dirty] {
        assert_present(&baseline, path);
        assert_present(&aggressive, path);
    }
    let baseline_rows = verdict_rows(&baseline);
    let aggressive_rows = verdict_rows(&aggressive);
    assert!(
        baseline_rows
            .iter()
            .any(|(_, verdict)| *verdict == Verdict::Safe),
        "the baseline fixture must contain a safe row"
    );
    assert!(
        baseline_rows
            .iter()
            .any(|(_, verdict)| *verdict == Verdict::Keep),
        "the baseline fixture must contain a keep row"
    );

    assert_eq!(
        baseline.find(&safe).expect("safe row").verdict,
        Verdict::Safe
    );
    assert_eq!(
        baseline.find(&merged).expect("merged row").verdict,
        Verdict::Review
    );
    assert_eq!(
        baseline.find(&stale).expect("baseline stale row").verdict,
        Verdict::Keep
    );
    assert_eq!(
        baseline.find(&active).expect("active row").verdict,
        Verdict::Keep
    );
    assert_eq!(
        baseline.find(&dirty).expect("dirty row").verdict,
        Verdict::Review
    );

    let removed: BTreeSet<_> = baseline_rows
        .difference(&aggressive_rows)
        .cloned()
        .collect();
    let added: BTreeSet<_> = aggressive_rows
        .difference(&baseline_rows)
        .cloned()
        .collect();
    assert!(
        !removed.is_empty(),
        "the aggressive policy must change at least one row"
    );
    for (path, verdict) in &removed {
        assert_eq!(
            *verdict,
            Verdict::Keep,
            "{} moved out of a verdict other than keep",
            path.display()
        );
    }
    let expected_added: BTreeSet<_> = removed
        .iter()
        .map(|(path, _)| (path.clone(), Verdict::Review))
        .collect();
    assert_eq!(
        added, expected_added,
        "policy may only move rows from keep to review"
    );

    assert_eq!(
        aggressive
            .find(&stale)
            .expect("aggressive stale row")
            .verdict,
        Verdict::Review
    );
}
