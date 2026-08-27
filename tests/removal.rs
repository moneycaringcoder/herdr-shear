//! The removal path and its guard rails.
//!
//! Two halves, and the first one is the product:
//!
//! 1. Every guard gets a test that proves it **refuses**. These need no
//!    repository at all, because [`shear::remove::check`] is pure — that is what
//!    makes them worth trusting.
//! 2. Real removals against real fixture repositories, which pin the claim the
//!    whole plugin rests on: removing a worktree takes the checkout and never
//!    the work.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use shear::config::Config;
use shear::git;
use shear::model::{
    Candidate, Class, Dirt, Head, LockInfo, Merged, Occupant, OpenWorkspace, PrunableInfo,
    RemovalRoute, RepoKey, Size, Upstream, Verdict, Worktree,
};
use shear::remove::{
    check, git_remove, parse_remove_args, prunable_note, read_log, remove_one,
    remove_one_with_state_dir, restore_command, route_for, Permissions, Refusal,
};

#[path = "fixtures.rs"]
mod fixtures;
use fixtures::Fixture;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// Everything process-global a test in this file depends on, set once.
///
/// The undo log's directory comes from the environment, so it is pointed at a
/// scratch directory before any test can write to it — a suite that appended to
/// the user's real removal log would be a poor advertisement for a plugin about
/// not destroying things. [`fixtures::pin_git_env`] does the same for git's
/// config, which the code under test deliberately does not scrub.
///
/// Every test that builds a [`Fixture`] or can reach
/// [`shear::remove::append_log`] calls this first.
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
        .join("shear-removal")
}

fn config() -> Config {
    Config {
        git_timeout: Duration::from_secs(30),
        ..Config::default()
    }
}

/// A candidate carrying nothing that would block it. Each guard test turns on
/// exactly the one fact it is about, so a refusal can only come from that fact.
fn candidate(path: &Path, repo_root: &Path, branch: Option<&str>, oid: Option<&str>) -> Candidate {
    Candidate {
        worktree: Worktree {
            repo: RepoKey("/fixture/.git".into()),
            repo_root: repo_root.to_path_buf(),
            path: path.to_path_buf(),
            head: match branch {
                Some(branch) => Head::Branch(branch.to_string()),
                None => Head::Detached,
            },
            head_oid: oid.map(str::to_string),
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
        occupants: Vec::new(),
        protected: None,
        classes: BTreeSet::new(),
        verdict: Verdict::Safe,
        size: Size::Pending,
        reason: String::new(),
    }
}

/// A candidate for a guard test, which never touches disk.
fn paper_candidate() -> Candidate {
    candidate(
        Path::new("/nowhere/wt-topic"),
        Path::new("/nowhere/repo"),
        Some("topic"),
        Some("1338d9a1338d9a1338d9a1338d9a1338d9a1338d"),
    )
}

/// Every permission shear has, all granted at once. A guard that still refuses
/// under this is a guard with no override, which is the point of using it.
fn every_permission(acknowledged: usize) -> Permissions {
    Permissions {
        force_dirty: true,
        acknowledged_files: Some(acknowledged),
        close_workspace: true,
    }
}

fn dirty(candidate: &mut Candidate, staged: usize, untracked: usize) {
    candidate.dirt = Dirt {
        paths: staged + untracked,
        staged,
        untracked,
        ..Default::default()
    };
    candidate.classes.insert(Class::Dirty);
}

/// Runs the restore command exactly as a user would: through a shell, with the
/// same scrubbed git environment the fixtures use.
fn run_in_shell(command: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "shear fixtures")
        .env("GIT_AUTHOR_EMAIL", "fixtures@example.invalid")
        .env("GIT_COMMITTER_NAME", "shear fixtures")
        .env("GIT_COMMITTER_EMAIL", "fixtures@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| format!("could not run a shell: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// Guards: each one proves a refusal, and none of them needs a repository
// ---------------------------------------------------------------------------

#[test]
fn the_main_checkout_is_refused_and_no_permission_overrides_it() {
    let mut candidate = paper_candidate();
    candidate.worktree.is_main = true;

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::MainCheckout)
    );
    assert_eq!(
        check(&candidate, every_permission(0)),
        Err(Refusal::MainCheckout),
        "there is no override for the main checkout, and there is no flag to add one"
    );

    // Still refused when it is also dirty and the count is acknowledged
    // correctly: the dirty override is not a general-purpose override.
    dirty(&mut candidate, 2, 1);
    assert_eq!(
        check(&candidate, every_permission(3)),
        Err(Refusal::MainCheckout)
    );

    let sentence = Refusal::MainCheckout.about(candidate.path());
    assert!(
        sentence.contains("main checkout") && sentence.contains("no override"),
        "the refusal has to say that asking again will not help: {sentence}"
    );
}

#[test]
fn a_protected_safe_worktree_is_refused_under_every_permission_combination() {
    let mut candidate = paper_candidate();
    let pattern = "release-*".to_string();
    candidate.protected = Some(pattern.clone());
    candidate.classes.insert(Class::Protected);

    let expected = Err(Refusal::Protected {
        pattern: pattern.clone(),
    });
    assert_eq!(check(&candidate, Permissions::default()), expected);
    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: Some(0),
                close_workspace: false,
            }
        ),
        Err(Refusal::Protected {
            pattern: pattern.clone(),
        })
    );
    assert_eq!(
        check(
            &candidate,
            Permissions {
                close_workspace: true,
                ..Permissions::default()
            }
        ),
        Err(Refusal::Protected {
            pattern: pattern.clone(),
        })
    );
    assert_eq!(
        check(&candidate, every_permission(0)),
        Err(Refusal::Protected {
            pattern: pattern.clone(),
        }),
        "no permission combination may widen the protected set"
    );

    let sentence = Refusal::Protected { pattern }.about(candidate.path());
    assert!(sentence.contains("release-*"), "{sentence}");
    assert!(
        sentence.contains("Edit or remove") && sentence.contains("no flag overrides"),
        "the refusal names both the unblocking action and the absence of an override: {sentence}"
    );
}

#[test]
fn protection_outranks_dirty_even_when_the_dirt_is_correctly_acknowledged() {
    let mut candidate = paper_candidate();
    candidate.protected = Some("/shared/**".into());
    candidate.classes.insert(Class::Protected);
    dirty(&mut candidate, 2, 1);

    assert_eq!(
        check(&candidate, every_permission(3)),
        Err(Refusal::Protected {
            pattern: "/shared/**".into(),
        }),
        "protection is the refusal shear cannot be talked out of"
    );
}

#[test]
fn a_locked_worktree_is_refused_and_the_refusal_names_git_worktree_unlock() {
    let mut candidate = paper_candidate();
    candidate.worktree.locked = Some(LockInfo {
        reason: Some("held for demo".into()),
    });
    candidate.classes.insert(Class::Locked);

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::Locked {
            reason: Some("held for demo".into())
        })
    );
    assert_eq!(
        check(&candidate, every_permission(0)),
        Err(Refusal::Locked {
            reason: Some("held for demo".into())
        }),
        "shear never unlocks on the user's behalf, whatever else they have permitted"
    );

    let sentence = check(&candidate, Permissions::default())
        .unwrap_err()
        .about(candidate.path());
    assert!(
        sentence.contains("held for demo"),
        "whose lock it is has to survive to the user: {sentence}"
    );
    assert!(
        sentence.contains("git worktree unlock /nowhere/wt-topic"),
        "and the refusal names the command that unblocks it, with the real path: {sentence}"
    );
}

#[test]
fn a_lock_with_no_reason_is_still_a_lock_and_is_not_rendered_as_an_empty_one() {
    // `git worktree lock` with no `--reason` gives the bare word `locked`, and
    // "no reason recorded" is not the same claim as "the reason is empty".
    let mut candidate = paper_candidate();
    candidate.worktree.locked = Some(LockInfo { reason: None });

    assert_eq!(
        check(&candidate, every_permission(0)),
        Err(Refusal::Locked { reason: None })
    );

    let sentence = Refusal::Locked { reason: None }.about(candidate.path());
    assert!(
        sentence.contains("no reason recorded"),
        "an absent reason is stated, not printed as `locked ()`: {sentence}"
    );
    assert!(!sentence.contains("()"), "no empty parentheses: {sentence}");
    assert!(sentence.contains("git worktree unlock /nowhere/wt-topic"));
}

#[test]
fn a_lock_outranks_every_other_refusal() {
    // A worktree that is both locked and dirty is refused for the lock: the
    // lock is the refusal the user cannot talk shear out of, so it is the one
    // worth telling them about.
    let mut candidate = paper_candidate();
    candidate.worktree.locked = Some(LockInfo {
        reason: Some("held".into()),
    });
    dirty(&mut candidate, 1, 1);
    candidate.open_workspace = Some(OpenWorkspace {
        workspace_id: "w18".into(),
        label: "live".into(),
        agent_status: None,
    });

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::Locked {
            reason: Some("held".into())
        })
    );
}

#[test]
fn a_worktree_open_in_herdr_is_refused_without_permission_to_close_the_workspace() {
    let mut candidate = paper_candidate();
    candidate.open_workspace = Some(OpenWorkspace {
        workspace_id: "w18".into(),
        label: "media-throughput".into(),
        agent_status: None,
    });
    candidate.classes.insert(Class::OpenInHerdr);

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::OpenInHerdr {
            workspace: "w18".into(),
            label: "media-throughput".into(),
        })
    );

    let sentence = check(&candidate, Permissions::default())
        .unwrap_err()
        .about(candidate.path());
    assert!(
        sentence.contains("w18") && sentence.contains("media-throughput"),
        "the row names which workspace is in the way: {sentence}"
    );
    assert!(
        sentence.contains("--close-workspace"),
        "and the flag that unblocks it: {sentence}"
    );

    assert_eq!(
        check(
            &candidate,
            Permissions {
                close_workspace: true,
                ..Permissions::default()
            }
        ),
        Ok(()),
        "with permission it is allowed — through the herdr route, which closes the workspace"
    );
}

#[test]
fn an_occupied_worktree_is_refused_under_every_permission_combination() {
    let mut candidate = paper_candidate();
    candidate.occupants.push(Occupant {
        pane_id: "w2:p1".into(),
        cwd: PathBuf::from("/nowhere/wt-topic/src"),
    });
    candidate.classes.insert(Class::Occupied);

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::Occupied {
            pane_id: "w2:p1".into(),
            cwd: PathBuf::from("/nowhere/wt-topic/src"),
        })
    );
    assert_eq!(
        check(&candidate, every_permission(0)),
        Err(Refusal::Occupied {
            pane_id: "w2:p1".into(),
            cwd: PathBuf::from("/nowhere/wt-topic/src"),
        }),
        "no flag removes the directory out from under a live pane"
    );

    let sentence = check(&candidate, Permissions::default())
        .unwrap_err()
        .about(candidate.path());
    assert!(
        sentence.contains("w2:p1") && sentence.contains("/nowhere/wt-topic/src"),
        "the refusal names the pane and where it is sitting: {sentence}"
    );
    assert!(
        sentence.contains("close the pane"),
        "and the unblocking action: {sentence}"
    );
}

#[test]
fn a_dirty_worktree_is_refused_without_force() {
    let mut candidate = paper_candidate();
    dirty(&mut candidate, 2, 5);

    assert_eq!(
        check(&candidate, Permissions::default()),
        Err(Refusal::Dirty { files: 7 })
    );

    let sentence = Refusal::Dirty { files: 7 }.about(candidate.path());
    assert!(
        sentence.contains("7 uncommitted files"),
        "the count is the thing the user has to read: {sentence}"
    );
    assert!(
        sentence.contains("--force-dirty") && sentence.contains("--i-understand-7-files"),
        "and both flags are named, including the number: {sentence}"
    );
}

#[test]
fn force_dirty_alone_is_not_a_confirmation() {
    // A confirmation that can be given without reading the number is not a
    // confirmation, so `--force-dirty` with no acknowledgement is the plain
    // dirty refusal, count and all.
    let mut candidate = paper_candidate();
    dirty(&mut candidate, 0, 3);

    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: None,
                close_workspace: false,
            }
        ),
        Err(Refusal::Dirty { files: 3 })
    );
}

#[test]
fn a_dirty_removal_with_the_wrong_acknowledged_count_is_its_own_refusal() {
    // This is what stops a `--force-dirty --i-understand-2-files` typed once
    // from applying to a worktree whose contents changed since the user looked.
    let mut candidate = paper_candidate();
    dirty(&mut candidate, 1, 2);

    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: Some(2),
                close_workspace: false,
            }
        ),
        Err(Refusal::DirtyCountMismatch {
            acknowledged: 2,
            actual: 3,
        })
    );

    let sentence = Refusal::DirtyCountMismatch {
        acknowledged: 2,
        actual: 3,
    }
    .about(candidate.path());
    assert!(
        sentence.contains("--i-understand-3-files"),
        "the refusal names the number that would actually work: {sentence}"
    );
}

#[test]
fn a_dirty_removal_with_the_exact_acknowledged_count_is_permitted() {
    let mut candidate = paper_candidate();
    dirty(&mut candidate, 1, 2);

    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: Some(3),
                close_workspace: false,
            }
        ),
        Ok(())
    );
}

#[test]
fn captured_dirty_status_rejects_the_old_dimension_sum_as_an_acknowledgement() {
    let mut candidate = paper_candidate();
    candidate.dirt =
        git::parse_status(include_bytes!("capture/status-v2.z")).expect("captured status parses");

    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: Some(6),
                close_workspace: false,
            }
        ),
        Err(Refusal::DirtyCountMismatch {
            acknowledged: 6,
            actual: 5,
        })
    );
}

#[test]
fn captured_dirty_status_accepts_its_unique_path_count() {
    let mut candidate = paper_candidate();
    candidate.dirt =
        git::parse_status(include_bytes!("capture/status-v2.z")).expect("captured status parses");

    assert_eq!(
        check(
            &candidate,
            Permissions {
                force_dirty: true,
                acknowledged_files: Some(5),
                close_workspace: false,
            }
        ),
        Ok(())
    );
}

#[test]
fn a_clean_worktree_needs_no_permissions_at_all() {
    assert_eq!(check(&paper_candidate(), Permissions::default()), Ok(()));
}

#[test]
fn the_route_follows_whether_herdr_holds_the_worktree_open() {
    let mut candidate = paper_candidate();
    assert_eq!(route_for(&candidate), RemovalRoute::Git);

    candidate.open_workspace = Some(OpenWorkspace {
        workspace_id: "w18".into(),
        label: "live".into(),
        agent_status: None,
    });
    assert_eq!(
        route_for(&candidate),
        RemovalRoute::Herdr,
        "worktree.remove is the only route that also closes the workspace"
    );
}

#[test]
fn the_restore_command_is_the_one_that_actually_works() {
    let branch = paper_candidate();
    assert_eq!(
        restore_command(&branch),
        "git -C /nowhere/repo worktree add /nowhere/wt-topic topic"
    );

    let detached = candidate(
        Path::new("/nowhere/wt-detached"),
        Path::new("/nowhere/repo"),
        None,
        Some("1338d9a1338d9a1338d9a1338d9a1338d9a1338d"),
    );
    assert_eq!(
        restore_command(&detached),
        "git -C /nowhere/repo worktree add --detach /nowhere/wt-detached \
         1338d9a1338d9a1338d9a1338d9a1338d9a1338d",
        "a detached head has no branch, so the oid is the handle"
    );

    let spaced = candidate(
        Path::new("/nowhere/wt topic"),
        Path::new("/nowhere/repo"),
        Some("topic"),
        None,
    );
    assert_eq!(
        restore_command(&spaced),
        "git -C /nowhere/repo worktree add '/nowhere/wt topic' topic",
        "it is pasted into a shell, so it is quoted for one"
    );
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn remove_takes_repeatable_paths_and_the_three_permission_flags() {
    let (paths, permissions) = parse_remove_args(&args(&[
        "--remove",
        "/tmp/a",
        "--remove=/tmp/b",
        "--force-dirty",
        "--i-understand-7-files",
        "--close-workspace",
    ]))
    .expect("parse");

    assert_eq!(paths, [PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
    assert_eq!(
        permissions,
        Permissions {
            force_dirty: true,
            acknowledged_files: Some(7),
            close_workspace: true,
        }
    );
}

#[test]
fn no_flags_means_no_permissions() {
    let (paths, permissions) = parse_remove_args(&args(&["--remove", "/tmp/a"])).expect("parse");
    assert_eq!(paths, [PathBuf::from("/tmp/a")]);
    assert_eq!(permissions, Permissions::default());
}

#[test]
fn a_malformed_acknowledgement_is_an_error_rather_than_a_silent_miss() {
    // Silently ignoring it would leave the user staring at a refusal for a flag
    // they can see they typed.
    let err = parse_remove_args(&args(&[
        "--remove",
        "/tmp/a",
        "--i-understand-lots-of-files",
    ]))
    .expect_err("a non-numeric count is an error");
    assert!(err.to_string().contains("--i-understand-lots-of-files"));

    let err = parse_remove_args(&args(&[
        "--remove",
        "/tmp/a",
        "--i-understand-3-files",
        "--i-understand-4-files",
    ]))
    .expect_err("two different counts cannot both be the acknowledgement");
    assert!(err.to_string().contains("3") && err.to_string().contains("4"));
}

#[test]
fn remove_without_a_path_is_an_error() {
    assert!(parse_remove_args(&args(&["--force-dirty", "--remove"])).is_err());
}

fn run_remove_cli(fixture: &Fixture, paths: &[&Path]) -> std::process::Output {
    let state = arrange();
    run_remove_cli_with_state(fixture, paths, state)
}

fn run_remove_cli_with_state(
    fixture: &Fixture,
    paths: &[&Path],
    state_dir: &Path,
) -> std::process::Output {
    let config_dir = arrange().join("empty-config");
    std::fs::create_dir_all(&config_dir).expect("create empty config directory");

    let mut command = Command::new(env!("CARGO_BIN_EXE_shear"));
    for path in paths {
        command.arg("--remove").arg(path);
    }
    command
        .arg("--repo")
        .arg(&fixture.repo)
        .arg("--no-size")
        .env("HERDR_PLUGIN_CONFIG_DIR", config_dir)
        .env("HERDR_PLUGIN_STATE_DIR", state_dir)
        .env("HERDR_SOCKET_PATH", arrange().join("missing.sock"))
        .output()
        .expect("run shear removal")
}

#[test]
fn successful_removal_carries_the_exact_warning_when_the_undo_log_is_unwritable() {
    let state = arrange();
    let blocked_state = state.join("outcome-state-path-is-a-file");
    std::fs::write(&blocked_state, b"not a directory").expect("create blocking state file");

    let fixture = Fixture::new("remove-outcome-without-undo-log");
    let path = fixture.safe_worktree("unlogged-outcome");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "unlogged-outcome-branch"]);
    let candidate = candidate(
        &path,
        &fixture.repo,
        Some("unlogged-outcome-branch"),
        Some(&oid),
    );
    let expected_restore = restore_command(&candidate);

    let outcome = remove_one_with_state_dir(
        &candidate,
        Permissions::default(),
        None,
        &config(),
        &blocked_state,
    )
    .expect("an unavailable undo log does not newly block an explicit removal");

    assert!(!path.exists(), "the checkout is removed");
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", "unlogged-outcome-branch"]),
        oid,
        "the branch and commit still survive"
    );
    assert_eq!(outcome.record.restore_command, expected_restore);
    let warning = outcome
        .undo_warning
        .expect("successful unlogged removal carries its warning");
    assert!(
        warning.contains(&format!(
            "shear: WARNING: no undo record could be written for {}",
            path.display()
        )) && warning.contains(&format!("could not create {}", blocked_state.display()))
            && warning.contains(
                "shear: the removal is going ahead because you asked for it, but nothing will \
                 remember it. Keep this:"
            )
            && warning.contains(&expected_restore),
        "the outcome preserves the exact persistent warning and recovery command: {warning}"
    );
}

#[test]
fn an_unwritable_undo_log_keeps_the_cli_warning_loud_during_success() {
    let state = arrange();
    let blocked_state = state.join("state-path-is-a-file");
    std::fs::write(&blocked_state, b"not a directory").expect("create blocking state file");

    let fixture = Fixture::new("remove-without-undo-log");
    let path = fixture.safe_worktree("unlogged");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "unlogged-branch"]);
    let expected_restore = format!(
        "git -C {} worktree add {} unlogged-branch",
        fixture.repo.display(),
        path.display()
    );

    let output = run_remove_cli_with_state(&fixture, &[&path], &blocked_state);
    assert!(output.status.success(), "{output:?}");
    assert!(!path.exists(), "best-effort logging does not block removal");
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", "unlogged-branch"]),
        oid,
        "the branch and commit still survive"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "shear: WARNING: no undo record could be written for {}",
            path.display()
        )) && stderr.contains(
            "shear: the removal is going ahead because you asked for it, but nothing will \
             remember it. Keep this:"
        ) && stderr.contains(&expected_restore),
        "the CLI keeps the warning and full recovery command loud: {stderr}"
    );
}

#[test]
fn repeated_identical_paths_are_planned_removed_and_logged_once() {
    arrange();
    let fixture = Fixture::new("remove-identical-repeat");
    let path = fixture.safe_worktree("same");

    let output = run_remove_cli(&fixture, &[&path, &path]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("shear: removing 1 worktree:"),
        "the plan counts the unique target: {stdout}"
    );
    assert!(!path.exists(), "the checkout is removed");

    let logged = read_log()
        .expect("read the undo log")
        .into_iter()
        .filter(|record| record.path == path.to_string_lossy())
        .count();
    assert_eq!(logged, 1, "the repeated target has one undo record");
}

#[test]
fn equivalent_canonical_spellings_are_removed_and_logged_once() {
    arrange();
    let fixture = Fixture::new("remove-canonical-repeat");
    let path = fixture.safe_worktree("same");
    let equivalent = path.join(".");

    let output = run_remove_cli(&fixture, &[&path, &equivalent]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("shear: removing 1 worktree:"),
        "both spellings resolve to one planned target: {stdout}"
    );
    assert!(!path.exists(), "the checkout is removed");

    let logged = read_log()
        .expect("read the undo log")
        .into_iter()
        .filter(|record| record.path == path.to_string_lossy())
        .count();
    assert_eq!(logged, 1, "the resolved target has one undo record");
}

#[test]
fn repeated_unknown_paths_produce_one_refusal_and_no_mutation() {
    arrange();
    let fixture = Fixture::new("remove-unknown-repeat");
    let path = fixture.safe_worktree("bystander");
    let unknown = fixture
        .repo
        .parent()
        .expect("fixture repo has a parent")
        .join("unknown-worktree");

    let output = run_remove_cli(&fixture, &[&path, &unknown, &unknown]);
    assert!(!output.status.success(), "an unknown target is refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let unknown_text = unknown.to_string_lossy();
    assert_eq!(
        stderr.matches(unknown_text.as_ref()).count(),
        1,
        "the repeated raw unknown has one clear refusal: {stderr}"
    );
    assert!(
        stderr.contains("1 of 2 selected worktrees refused; nothing was removed"),
        "the failure count uses unique targets: {stderr}"
    );
    assert!(path.exists(), "batch preflight prevents the known removal");

    let logged = read_log()
        .expect("read the undo log")
        .into_iter()
        .any(|record| record.path == path.to_string_lossy());
    assert!(
        !logged,
        "a batch refused in preflight writes no undo record"
    );
}

#[test]
fn distinct_targets_keep_the_first_requested_order() {
    arrange();
    let fixture = Fixture::new("remove-distinct-order");
    let first = fixture.safe_worktree("first");
    let second = fixture.safe_worktree("second");

    let output = run_remove_cli(&fixture, &[&second, &first]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let second_position = stdout
        .find(second.to_string_lossy().as_ref())
        .expect("second target is in the plan");
    let first_position = stdout
        .find(first.to_string_lossy().as_ref())
        .expect("first target is in the plan");
    assert!(
        second_position < first_position,
        "the plan preserves first-request order: {stdout}"
    );
    assert!(
        !first.exists() && !second.exists(),
        "both checkouts are removed"
    );

    let paths: Vec<String> = read_log()
        .expect("read the undo log")
        .into_iter()
        .map(|record| record.path)
        .filter(|path| {
            path == first.to_string_lossy().as_ref() || path == second.to_string_lossy().as_ref()
        })
        .collect();
    assert_eq!(
        paths,
        [
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ],
        "newest-first log order reflects execution in first-request order"
    );
}

// ---------------------------------------------------------------------------
// Real removals, against real repositories
// ---------------------------------------------------------------------------

#[test]
fn a_clean_worktree_goes_and_its_branch_and_commit_still_resolve() {
    // This is the safety claim the whole plugin rests on, so it is pinned here
    // rather than left to the interface to promise.
    arrange();
    let fixture = Fixture::new("remove-clean");
    let path = fixture.safe_worktree("safe");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "safe-branch"]);
    let tree = fixture.git(&fixture.repo, &["rev-parse", "safe-branch^{tree}"]);

    let mut candidate = candidate(&path, &fixture.repo, Some("safe-branch"), Some(&oid));
    candidate.size = Size::Skipped;
    let outcome = remove_one(&candidate, Permissions::default(), None, &config())
        .expect("a clean worktree needs no permissions");
    assert!(
        outcome.undo_warning.is_none(),
        "a written undo record produces no warning"
    );
    let record = &outcome.record;

    assert!(!path.exists(), "the checkout is gone");
    assert!(
        !fixture
            .git(&fixture.repo, &["worktree", "list"])
            .contains("wt-safe"),
        "and git's admin entry with it"
    );
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", "safe-branch"]),
        oid,
        "the branch still resolves: removal takes the checkout, never the work"
    );
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", &format!("{oid}^{{tree}}")]),
        tree,
        "and the commit's tree is still in the object store"
    );
    assert_eq!(record.route, "git");
    assert_eq!(record.head_oid.as_deref(), Some(oid.as_str()));
    assert_eq!(
        record.bytes_reclaimed, None,
        "deliberately skipped measurement must not be logged as zero bytes"
    );
}

#[test]
fn a_prunable_worktree_is_removable_through_git_although_its_directory_is_gone() {
    // Verified in docs/git-plumbing.md: `git worktree remove` exits 0 on a
    // prunable worktree, so there is no need for `git worktree prune` — which
    // would be wrong anyway, since it prunes every prunable worktree in the repo
    // rather than the one the user selected.
    arrange();
    let fixture = Fixture::new("remove-prunable");
    let alive = fixture.stale_worktree("bystander", 30);
    let path = fixture.prunable_worktree("goner");
    assert!(!path.exists(), "the fixture deleted it behind git's back");

    let listed = fixture.git(&fixture.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains("wt-goner"),
        "git still carries the admin entry: {listed}"
    );

    git_remove(&fixture.repo, &path, false, &config()).expect("git removes a prunable worktree");

    let listed = fixture.git(&fixture.repo, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains("wt-goner"), "the admin entry is pruned");
    assert!(
        listed.contains("wt-bystander") && alive.exists(),
        "and only the selected one: the other prunable-adjacent worktree is untouched"
    );
}

#[test]
fn a_prunable_candidate_goes_through_remove_one_and_is_logged() {
    // The same removal as above, but driven through the guarded path with a
    // candidate that carries git's `prunable` flag, so the whole route is
    // exercised against the real attribute rather than only `git_remove`.
    arrange();
    let fixture = Fixture::new("prunable-remove-one");
    let path = fixture.prunable_worktree("vanished");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "vanished-branch"]);

    let mut candidate = candidate(&path, &fixture.repo, Some("vanished-branch"), Some(&oid));
    candidate.worktree.prunable = Some(PrunableInfo {
        reason: Some("gitdir file points to non-existent location".into()),
    });
    candidate.classes.insert(Class::Prunable);
    candidate.size = Size::Gone;

    let outcome = remove_one(&candidate, Permissions::default(), None, &config())
        .expect("a prunable worktree needs no permissions: there is nothing left to lose");
    assert!(outcome.undo_warning.is_none());
    let record = &outcome.record;

    assert!(!fixture
        .git(&fixture.repo, &["worktree", "list", "--porcelain"])
        .contains("wt-vanished"));
    assert_eq!(
        fixture.git(&fixture.repo, &["rev-parse", "vanished-branch"]),
        oid,
        "the branch survives a prunable removal too"
    );
    assert_eq!(
        record.bytes_reclaimed,
        Some(0),
        "a directory that is already gone reclaims nothing, which is a measurement \
         rather than a missing one"
    );
    assert!(record.classes.iter().any(|class| class == "prunable"));

    let records = read_log().expect("read the undo log");
    assert!(records
        .iter()
        .any(|logged| logged.path == path.to_string_lossy()));
}

#[test]
fn the_prunable_note_reads_correctly_with_and_without_a_reason() {
    // git reports `prunable` with a reason but is not obliged to, and the two
    // states are two sentences: an absent reason must never render as an empty
    // parenthesis, for exactly the reason an absent lock reason must not.
    let mut candidate = paper_candidate();
    assert_eq!(
        prunable_note(&candidate),
        "",
        "a worktree that is not prunable says nothing about prunability"
    );

    candidate.worktree.prunable = Some(PrunableInfo {
        reason: Some("gitdir file points to non-existent location".into()),
    });
    assert_eq!(
        prunable_note(&candidate),
        "; the checkout is already gone (gitdir file points to non-existent location)"
    );

    candidate.worktree.prunable = Some(PrunableInfo { reason: None });
    let note = prunable_note(&candidate);
    assert_eq!(
        note,
        "; the checkout is already gone, and git gave no reason"
    );
    assert!(!note.contains("()"), "no empty parentheses: {note}");
}

#[test]
fn git_itself_refuses_a_dirty_worktree() {
    // The second guard, behind check(). If the two ever disagree, this is the
    // one that is right.
    arrange();
    let fixture = Fixture::new("remove-dirty");
    let path = fixture.dirty_worktree("messy");

    let err = git_remove(&fixture.repo, &path, false, &config())
        .expect_err("git refuses a dirty worktree on its own");
    assert!(
        err.to_string()
            .contains("contains modified or untracked files"),
        "git's own message reaches the user: {err}"
    );
    assert!(path.exists(), "and nothing was removed");
    assert!(
        path.join("scratch.txt").exists(),
        "including the untracked file that caused the refusal"
    );
}

#[test]
fn git_itself_refuses_a_locked_worktree_even_with_force() {
    // shear passes a single `--force`, never `-f -f`. That is the difference
    // between "permit uncommitted changes" and "override somebody's lock", and
    // this test is what keeps them different.
    arrange();
    let fixture = Fixture::new("remove-locked");
    let path = fixture.locked_worktree("held", "held for demo");

    for force in [false, true] {
        let err = git_remove(&fixture.repo, &path, force, &config())
            .expect_err("git refuses a locked worktree whatever a single --force says");
        assert!(
            err.to_string()
                .contains("cannot remove a locked working tree"),
            "with force={force}, git's own message reaches the user: {err}"
        );
        assert!(
            err.to_string().contains("held for demo"),
            "including whose lock it is: {err}"
        );
        assert!(path.exists(), "and nothing was removed");
    }
}

#[test]
fn the_undo_log_round_trips_and_its_restore_command_restores_the_checkout() {
    arrange();
    let fixture = Fixture::new("undo-log");
    let path = fixture.safe_worktree("recoverable");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "recoverable-branch"]);

    let candidate = candidate(&path, &fixture.repo, Some("recoverable-branch"), Some(&oid));
    let written = remove_one(&candidate, Permissions::default(), None, &config()).expect("remove");
    assert!(written.undo_warning.is_none());
    assert!(!path.exists());

    let records = read_log().expect("read the undo log");
    let record = records
        .iter()
        .find(|record| record.path == path.to_string_lossy())
        .expect("the removal is in the log");
    assert_eq!(
        record, &written.record,
        "what was returned is what was written"
    );
    assert_eq!(record.branch.as_deref(), Some("recoverable-branch"));
    assert_eq!(record.head_oid.as_deref(), Some(oid.as_str()));
    assert_eq!(record.route, "git");
    assert!(
        record.at.ends_with('Z') && record.at.contains('T'),
        "the timestamp is RFC 3339 in UTC: {}",
        record.at
    );

    // The claim under test: this command is one that actually works.
    run_in_shell(&record.restore_command)
        .unwrap_or_else(|err| panic!("restore command failed: {}: {err}", record.restore_command));

    assert!(path.exists(), "the checkout is back");
    assert!(
        path.join("README.md").exists(),
        "with its content, from the branch that was never touched"
    );
    assert_eq!(
        fixture.git(&path, &["rev-parse", "HEAD"]),
        oid,
        "at the very commit the log recorded"
    );
    assert!(fixture
        .git(&fixture.repo, &["worktree", "list"])
        .contains("wt-recoverable"));
}

#[test]
fn the_undo_record_is_written_before_the_removal_is_attempted() {
    // A removal that half-succeeds has to still be recoverable, so the note goes
    // down first. The proof is a removal that fails outright: the record is
    // there anyway.
    arrange();
    let fixture = Fixture::new("log-first");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "HEAD"]);
    let bogus = fixture.root().join("not-a-worktree");
    std::fs::create_dir_all(&bogus).expect("create a directory git knows nothing about");

    let candidate = candidate(&bogus, &fixture.repo, None, Some(&oid));
    let err = remove_one(&candidate, Permissions::default(), None, &config())
        .expect_err("git cannot remove something that is not a worktree");
    assert!(
        err.to_string().contains("not a working tree"),
        "git's own message reaches the user: {err}"
    );

    let records = read_log().expect("read the undo log");
    assert!(
        records
            .iter()
            .any(|record| record.path == bogus.to_string_lossy()),
        "the record was appended before the attempt, and survives its failure"
    );
    assert!(bogus.exists(), "and the failed removal changed nothing");
}

#[test]
fn a_failed_route_carries_the_undo_warning_emitted_before_the_attempt() {
    let state = arrange();
    let blocked_state = state.join("failed-route-state-path-is-a-file");
    std::fs::write(&blocked_state, b"not a directory").expect("create blocking state file");

    let fixture = Fixture::new("failed-route-without-undo-log");
    let oid = fixture.git(&fixture.repo, &["rev-parse", "HEAD"]);
    let bogus = fixture.root().join("not-a-worktree");
    std::fs::create_dir_all(&bogus).expect("create a directory git knows nothing about");
    let candidate = candidate(&bogus, &fixture.repo, None, Some(&oid));
    let restore = restore_command(&candidate);

    let failure = remove_one_with_state_dir(
        &candidate,
        Permissions::default(),
        None,
        &config(),
        &blocked_state,
    )
    .expect_err("the git route still fails");

    assert!(failure.message.contains("not a working tree"));
    let warning = failure
        .undo_warning
        .expect("the route failure retains the earlier warning");
    assert!(
        warning.contains("WARNING")
            && warning.contains(blocked_state.to_string_lossy().as_ref())
            && warning.contains(&restore),
        "the exact persistent recovery warning survives the route error: {warning}"
    );
    assert!(bogus.exists(), "the failed route changed nothing");
}

#[test]
fn the_undo_log_is_newest_first() {
    arrange();
    let fixture = Fixture::new("log-order");
    let first = fixture.safe_worktree("older");
    let second = fixture.safe_worktree("newer");

    for (path, branch) in [(&first, "older-branch"), (&second, "newer-branch")] {
        let oid = fixture.git(&fixture.repo, &["rev-parse", branch]);
        let candidate = candidate(path, &fixture.repo, Some(branch), Some(&oid));
        remove_one(&candidate, Permissions::default(), None, &config()).expect("remove");
    }

    let records = read_log().expect("read the undo log");
    let ours: Vec<&str> = records
        .iter()
        .map(|record| record.path.as_str())
        .filter(|path| path.contains("wt-older") || path.contains("wt-newer"))
        .collect();
    assert_eq!(ours.len(), 2);
    assert!(ours[0].contains("wt-newer"), "newest first: {ours:?}");
}

#[test]
fn a_worktree_herdr_holds_open_is_refused_when_there_is_no_socket() {
    // The failure this prevents: falling back to git, removing the directory,
    // and leaving herdr showing a workspace whose checkout has vanished.
    arrange();
    let fixture = Fixture::new("no-socket");
    let path = fixture.active_worktree("live");

    let oid = fixture.git(&fixture.repo, &["rev-parse", "live-branch"]);
    let mut candidate = candidate(&path, &fixture.repo, Some("live-branch"), Some(&oid));
    candidate.open_workspace = Some(OpenWorkspace {
        workspace_id: "w18".into(),
        label: "live".into(),
        agent_status: None,
    });
    candidate.classes.insert(Class::OpenInHerdr);

    let permissions = Permissions {
        close_workspace: true,
        ..Permissions::default()
    };
    // check() permits it; the socket is what is missing.
    assert_eq!(check(&candidate, permissions), Ok(()));

    let err = remove_one(&candidate, permissions, None, &config())
        .expect_err("the herdr route is unavailable, so this cannot happen at all");
    let message = err.to_string();
    assert!(
        message.contains("w18") && message.contains("socket"),
        "the refusal says which workspace and why: {message}"
    );
    assert!(
        message.contains("will not remove it with git"),
        "and that it deliberately did not fall back: {message}"
    );

    assert!(path.exists(), "nothing was removed");
    let records = read_log().expect("read the undo log");
    assert!(
        !records
            .iter()
            .any(|record| record.path == path.to_string_lossy()),
        "and a removal that never started is not in the undo log"
    );
}
