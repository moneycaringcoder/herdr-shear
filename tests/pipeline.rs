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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

use shear::config::Config;
use shear::model::{Class, Merged, Size, Verdict};
use shear::{disk, shear as pipeline};

use fixtures::Fixture;

/// Herdr-related environment is process-global, so these tests run one at a
/// time even though cargo runs them on separate threads.
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
    std::env::remove_var("HERDR_PLUGIN_CONTEXT_JSON");
    std::env::remove_var("HERDR_PLUGIN_ROOT");
    std::env::set_var(
        "XDG_CONFIG_HOME",
        "/nonexistent/shear-pipeline-standalone-xdg",
    );
}

#[derive(Clone)]
enum InjectedFailure {
    None,
    SessionSnapshot,
    WorktreeList(PathBuf),
    WorktreeListNotGit(PathBuf),
}

struct FakeHerdr {
    dir: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeHerdr {
    fn new(failure: InjectedFailure) -> Self {
        Self::with_snapshot(
            failure,
            serde_json::json!({
                "workspaces": [],
                "panes": []
            }),
        )
    }

    fn with_snapshot(failure: InjectedFailure, snapshot: serde_json::Value) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join("shr-pipeline").join(format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fake herdr socket directory");
        let socket = dir.join("s");
        let listener = UnixListener::bind(&socket).expect("bind fake herdr socket");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    break;
                }
                if line.trim().is_empty() {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                let request: serde_json::Value =
                    serde_json::from_str(line.trim_end()).expect("parse fake herdr request");
                let id = request["id"].clone();
                let method = request["method"].as_str().unwrap_or_default();
                let failure_code = match &failure {
                    InjectedFailure::None => None,
                    InjectedFailure::SessionSnapshot if method == "session.snapshot" => {
                        Some("injected_visibility_failure")
                    }
                    InjectedFailure::WorktreeList(repo)
                        if method == "worktree.list"
                            && request["params"]["cwd"].as_str()
                                == Some(repo.to_string_lossy().as_ref()) =>
                    {
                        Some("injected_visibility_failure")
                    }
                    InjectedFailure::WorktreeListNotGit(repo)
                        if method == "worktree.list"
                            && request["params"]["cwd"].as_str()
                                == Some(repo.to_string_lossy().as_ref()) =>
                    {
                        Some(shear::herdr::ERR_NOT_GIT)
                    }
                    _ => None,
                };
                let response = if let Some(code) = failure_code {
                    serde_json::json!({
                        "id": id,
                        "error": {
                            "code": code,
                            "message": format!("injected {method} failure")
                        }
                    })
                } else if method == "session.snapshot" {
                    serde_json::json!({
                        "id": id,
                        "result": {
                            "type": "session_snapshot",
                            "snapshot": snapshot.clone()
                        }
                    })
                } else {
                    serde_json::json!({
                        "id": id,
                        "result": {
                            "type": "worktree_list",
                            "worktrees": []
                        }
                    })
                };
                let mut encoded = serde_json::to_vec(&response).expect("encode fake reply");
                encoded.push(b'\n');
                (&stream).write_all(&encoded).expect("write fake reply");
            }
        });
        Self {
            dir,
            socket,
            stop,
            handle: Some(handle),
        }
    }

    fn select(&self) {
        std::env::set_var("HERDR_SOCKET_PATH", &self.socket);
        std::env::remove_var("HERDR_PLUGIN_ID");
        std::env::remove_var("HERDR_PLUGIN_CONTEXT_JSON");
        std::env::remove_var("HERDR_PLUGIN_ROOT");
    }

    fn select_plugin(&self, plugin_root: &Path, context: &serde_json::Value) {
        self.select_plugin_raw(plugin_root, &context.to_string());
    }

    fn select_plugin_raw(&self, plugin_root: &Path, context: &str) {
        std::env::set_var("HERDR_SOCKET_PATH", &self.socket);
        std::env::set_var("HERDR_PLUGIN_ID", "moneycaringcoder.shear");
        std::env::set_var("HERDR_PLUGIN_ROOT", plugin_root);
        std::env::set_var("HERDR_PLUGIN_CONTEXT_JSON", context);
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
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

/// Herdr 0.8.2 can identify the active workspace without attaching worktree
/// metadata to it. The invocation context is then the only repository seed.
fn snapshot_without_worktree() -> serde_json::Value {
    serde_json::json!({
        "workspaces": [{
            "workspace_id": "w-context",
            "label": "user-repository"
        }],
        "panes": []
    })
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
    let safe = fixture.safe_worktree("safe");

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
    let safe_row = inventory.find(&safe).expect("standalone safe row");
    assert_eq!(
        safe_row.verdict,
        Verdict::Safe,
        "a hand-run scan with no Herdr expected preserves the documented standalone rule"
    );
    assert!(shear::tui::preselectable(safe_row));
}

#[test]
fn either_herdr_context_marker_makes_a_connection_failure_demote_safe_rows() {
    let _guard = env_lock();
    no_herdr();
    std::env::set_var(
        "HERDR_SOCKET_PATH",
        "/nonexistent/shear-pipeline-expected.sock",
    );

    let fixture = Fixture::new("expected-no-herdr");
    let safe = fixture.safe_worktree("safe");
    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    std::env::remove_var("HERDR_SOCKET_PATH");

    let row = inventory.find(&safe).expect("safe-shaped row");
    assert_eq!(row.verdict, Verdict::Review);
    assert!(!shear::tui::preselectable(row));
    assert_eq!(inventory.safe().count(), 0);
    assert!(
        inventory.notes.iter().any(|note| {
            note.contains("herdr is not reachable") && note.contains("cannot be identified")
        }),
        "{:?}",
        inventory.notes
    );
    std::env::set_var("HERDR_PLUGIN_ID", "shear");
    let plugin_inventory = pipeline::scan(&config_for(&fixture.repo)).expect("plugin scan");
    std::env::remove_var("HERDR_PLUGIN_ID");
    assert_eq!(
        plugin_inventory
            .find(&safe)
            .expect("plugin safe-shaped row")
            .verdict,
        Verdict::Review,
        "HERDR_PLUGIN_ID independently marks Herdr visibility as expected"
    );
}

#[test]
fn complete_herdr_visibility_preserves_safe_classification() {
    let _guard = env_lock();
    let server = FakeHerdr::new(InjectedFailure::None);
    server.select();

    let fixture = Fixture::new("complete-herdr");
    let safe = fixture.safe_worktree("safe");
    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");

    let row = inventory.find(&safe).expect("safe row");
    assert_eq!(row.verdict, Verdict::Safe);
    assert!(shear::tui::preselectable(row));
}

#[test]
fn herdr_0_8_2_context_discovers_the_user_repo_without_snapshot_worktree_metadata() {
    let _guard = env_lock();
    let server = FakeHerdr::with_snapshot(InjectedFailure::None, snapshot_without_worktree());
    let installed_plugin = Fixture::new("installed-plugin");
    let plugin_candidate = installed_plugin.safe_worktree("plugin-candidate");
    let user_repo = Fixture::new("context-user-repo");
    let user_candidate = user_repo.safe_worktree("user-candidate");
    let unusable_focus = server.dir.join("focused-outside-repo");
    std::fs::create_dir_all(&unusable_focus).expect("create unusable focused cwd");

    let contexts = [
        (
            "focused pane cwd",
            Verdict::Safe,
            serde_json::json!({
                "workspace_id": "w-context",
                "workspace_cwd": "/outside/fallback-must-not-win",
                "focused_pane_id": "p-context",
                "focused_pane_cwd": user_candidate
            }),
        ),
        (
            "workspace cwd",
            Verdict::Safe,
            serde_json::json!({
                "workspace_id": "w-context",
                "workspace_cwd": user_candidate,
                "focused_pane_id": "p-context",
                "focused_pane_cwd": null
            }),
        ),
        (
            "workspace fallback after unusable focused pane cwd",
            Verdict::Review,
            serde_json::json!({
                "workspace_id": "w-context",
                "workspace_cwd": user_candidate,
                "focused_pane_id": "p-context",
                "focused_pane_cwd": unusable_focus
            }),
        ),
    ];

    for (source, expected_verdict, context) in contexts {
        server.select_plugin(&installed_plugin.repo, &context);
        let inventory = pipeline::scan(&Config {
            integration_ref: Some("main".into()),
            ..Config::default()
        })
        .unwrap_or_else(|err| panic!("scan from {source}: {err}"));

        assert_eq!(
            inventory
                .repos
                .iter()
                .map(|repo| repo.root.as_path())
                .collect::<Vec<_>>(),
            vec![user_repo.repo.as_path()],
            "{source} must seed only the user repository"
        );
        assert!(
            inventory.find(&user_candidate).is_some(),
            "{source} did not expose the user's worktree"
        );
        assert_eq!(
            inventory
                .find(&user_candidate)
                .expect("user candidate")
                .verdict,
            expected_verdict,
            "{source} visibility"
        );
        assert!(
            inventory.find(&plugin_candidate).is_none(),
            "{source} exposed the installed plugin repository"
        );
    }
}

#[test]
fn explicit_repo_replaces_installed_plugin_context_scope() {
    let _guard = env_lock();
    let server = FakeHerdr::with_snapshot(InjectedFailure::None, snapshot_without_worktree());
    let installed_plugin = Fixture::new("explicit-plugin-root");
    let contextual = Fixture::new("explicit-contextual");
    let contextual_candidate = contextual.safe_worktree("contextual-candidate");
    let explicit = Fixture::new("explicit-target");
    let explicit_candidate = explicit.safe_worktree("explicit-candidate");
    server.select_plugin(
        &installed_plugin.repo,
        &serde_json::json!({
            "workspace_id": "w-context",
            "workspace_cwd": contextual.repo,
            "focused_pane_id": "p-context",
            "focused_pane_cwd": contextual.repo
        }),
    );

    let inventory = pipeline::scan(&config_for(&explicit.repo)).expect("explicit scan");

    assert!(inventory.find(&explicit_candidate).is_some());
    assert!(inventory.find(&contextual_candidate).is_none());
    assert_eq!(
        inventory
            .repos
            .iter()
            .map(|repo| repo.root.as_path())
            .collect::<Vec<_>>(),
        vec![explicit.repo.as_path()]
    );
}

#[test]
fn malformed_plugin_context_keeps_explicit_rows_fail_closed() {
    let _guard = env_lock();
    let server = FakeHerdr::new(InjectedFailure::None);
    let installed_plugin = Fixture::new("malformed-plugin-root");
    let explicit = Fixture::new("malformed-explicit");
    let safe = explicit.safe_worktree("safe");
    server.select_plugin_raw(&installed_plugin.repo, r#"{"focused_pane_cwd":42}"#);

    let inventory = pipeline::scan(&config_for(&explicit.repo)).expect("explicit scan");
    let row = inventory.find(&safe).expect("explicit row");

    assert_eq!(row.verdict, Verdict::Review);
    assert!(!shear::tui::preselectable(row));
    assert_eq!(inventory.safe().count(), 0);
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains("HERDR_PLUGIN_CONTEXT_JSON")
                && note.contains("cannot be classified safe")),
        "{:?}",
        inventory.notes
    );
}

#[test]
fn context_outside_a_git_checkout_does_not_fall_back_to_plugin_cwd() {
    let _guard = env_lock();
    let server = FakeHerdr::with_snapshot(InjectedFailure::None, snapshot_without_worktree());
    let outside = server.dir.join("not-a-repository");
    std::fs::create_dir_all(&outside).expect("create non-repository context cwd");
    let installed_plugin = Fixture::new("outside-plugin-root");
    let plugin_candidate = installed_plugin.safe_worktree("plugin-candidate");
    let configured = Fixture::new("outside-configured");
    let safe = configured.safe_worktree("safe");
    server.select_plugin(
        &installed_plugin.repo,
        &serde_json::json!({
            "workspace_id": "w-context",
            "workspace_cwd": outside,
            "focused_pane_id": "p-context",
            "focused_pane_cwd": outside
        }),
    );

    let inventory = pipeline::scan(&Config {
        extra_repos: vec![configured.repo.clone()],
        integration_ref: Some("main".into()),
        ..Config::default()
    })
    .expect("scan with unusable context");
    let row = inventory.find(&safe).expect("configured safe-shaped row");

    assert_eq!(row.verdict, Verdict::Review);
    assert!(!shear::tui::preselectable(row));
    assert_eq!(inventory.safe().count(), 0);
    assert!(inventory.find(&plugin_candidate).is_none());
    assert!(
        inventory.notes.iter().any(|note| {
            note.contains("not inside a readable git checkout")
                && note.contains("cannot be classified safe")
        }),
        "{:?}",
        inventory.notes
    );
}
#[test]
fn context_cannot_select_the_installed_plugin_repository() {
    let _guard = env_lock();
    let server = FakeHerdr::with_snapshot(InjectedFailure::None, snapshot_without_worktree());
    let installed_plugin = Fixture::new("self-context-plugin-root");
    let plugin_candidate = installed_plugin.safe_worktree("plugin-candidate");
    let configured = Fixture::new("self-context-configured");
    let safe = configured.safe_worktree("safe");
    server.select_plugin(
        &installed_plugin.repo,
        &serde_json::json!({
            "workspace_id": "w-context",
            "workspace_cwd": installed_plugin.repo,
            "focused_pane_id": "p-context",
            "focused_pane_cwd": installed_plugin.repo
        }),
    );

    let inventory = pipeline::scan(&Config {
        extra_repos: vec![configured.repo.clone()],
        integration_ref: Some("main".into()),
        ..Config::default()
    })
    .expect("scan with plugin root context");
    let row = inventory.find(&safe).expect("configured safe-shaped row");

    assert_eq!(row.verdict, Verdict::Review);
    assert!(!shear::tui::preselectable(row));
    assert_eq!(inventory.safe().count(), 0);
    assert!(inventory.find(&plugin_candidate).is_none());
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains("installed plugin root")
                && note.contains("cannot be classified safe")),
        "{:?}",
        inventory.notes
    );
}

#[test]
fn failed_session_snapshot_demotes_every_safe_shape_to_review() {
    let _guard = env_lock();
    let server = FakeHerdr::new(InjectedFailure::SessionSnapshot);
    server.select();

    let fixture = Fixture::new("failed-snapshot");
    let safe = fixture.safe_worktree("safe");
    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");

    let row = inventory.find(&safe).expect("safe-shaped row");
    assert_eq!(
        row.verdict,
        Verdict::Review,
        "the death signals remain reviewable so explicit removal stays available"
    );
    assert!(!shear::tui::preselectable(row));
    assert_eq!(inventory.safe().count(), 0);
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains("session.snapshot")
                && note.contains("cannot be classified safe")),
        "{:?}",
        inventory.notes
    );
}

#[test]
fn failed_worktree_list_demotes_only_its_repository() {
    let _guard = env_lock();
    let blind = Fixture::new("blind-repo");
    let blind_safe = blind.safe_worktree("safe");
    let visible = Fixture::new("visible-repo");
    let visible_safe = visible.safe_worktree("safe");
    let server = FakeHerdr::new(InjectedFailure::WorktreeList(blind.repo.clone()));
    server.select();

    let inventory = pipeline::scan(&Config {
        only_repos: vec![blind.repo.clone(), visible.repo.clone()],
        integration_ref: Some("main".into()),
        ..Config::default()
    })
    .expect("scan");

    let blind_row = inventory.find(&blind_safe).expect("blind row");
    let visible_row = inventory.find(&visible_safe).expect("visible row");
    assert_eq!(blind_row.verdict, Verdict::Review);
    assert_eq!(visible_row.verdict, Verdict::Safe);
    assert!(!shear::tui::preselectable(blind_row));
    assert!(shear::tui::preselectable(visible_row));
    assert_eq!(
        inventory
            .safe()
            .map(|candidate| candidate.path())
            .collect::<Vec<_>>(),
        vec![visible_safe.as_path()],
        "bulk safety retains only repositories whose Herdr joins completed"
    );
    let blindness: Vec<&String> = inventory
        .notes
        .iter()
        .filter(|note| note.contains("worktree.list"))
        .collect();
    assert_eq!(blindness.len(), 1, "{:?}", inventory.notes);
    assert!(blindness[0].contains(blind.repo.to_string_lossy().as_ref()));
}

#[test]
fn not_git_worktree_is_a_complete_answer_not_visibility_failure() {
    let _guard = env_lock();
    let fixture = Fixture::new("herdr-not-git");
    let safe = fixture.safe_worktree("safe");
    let server = FakeHerdr::new(InjectedFailure::WorktreeListNotGit(fixture.repo.clone()));
    server.select();

    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    let row = inventory.find(&safe).expect("safe row");
    assert_eq!(
        row.verdict,
        Verdict::Safe,
        "not_git_worktree is Herdr data and must not demote an otherwise safe row"
    );
    assert!(shear::tui::preselectable(row));
    assert!(
        inventory.notes.iter().all(|note| {
            !note.contains("worktree.list") && !note.contains("visibility is unknown")
        }),
        "not_git_worktree must not report incomplete visibility: {:?}",
        inventory.notes
    );
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
fn disabled_measurement_settles_every_row_as_skipped_and_never_starts_a_walk() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("sizes-skipped");
    fixture.safe_worktree("safe");
    let mut inventory = pipeline::scan(&Config {
        measure_disk: false,
        ..config_for(&fixture.repo)
    })
    .expect("scan");

    assert!(
        inventory
            .candidates
            .iter()
            .all(|candidate| candidate.size == Size::Skipped),
        "disabled sizing must settle during the scan, not remain pending"
    );

    disk::measure_all(&mut inventory, &AtomicBool::new(false));
    assert!(
        inventory
            .candidates
            .iter()
            .all(|candidate| candidate.size == Size::Skipped),
        "a generic sizing helper must not turn skipped rows into background work"
    );
}

#[test]
fn inventory_json_projects_every_size_state_without_inventing_bytes() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("json-size-states");
    let path = fixture.safe_worktree("safe");
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
            .find(|candidate| candidate.path() == path)
            .expect("safe row")
            .size = size;
        let json = pipeline::to_json(&inventory);
        let row = json["worktrees"]
            .as_array()
            .expect("worktrees")
            .iter()
            .find(|row| row["path"] == path.to_string_lossy().as_ref())
            .expect("safe row projection");
        assert_eq!(row["size_state"], state, "{size:?}");
        assert_eq!(row["bytes"], bytes, "{size:?}");
    }
}

#[test]
fn json_dirty_files_counts_an_mm_path_once_without_losing_dimensions() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("json-dirty-path-count");
    let path = fixture.add_worktree("double-state", &["-b", "double-state-branch"]);
    fixture.append(&path, "README.md", "staged change\n");
    fixture.git(&path, &["add", "README.md"]);
    fixture.append(&path, "README.md", "unstaged change\n");

    let inventory = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    let candidate = inventory.find(&path).expect("double-state worktree");
    assert_eq!(candidate.dirt.paths, 1);
    assert_eq!(candidate.dirt.staged, 1);
    assert_eq!(candidate.dirt.unstaged, 1);

    let json = pipeline::to_json(&inventory);
    let row = json["worktrees"]
        .as_array()
        .expect("worktrees")
        .iter()
        .find(|row| row["path"] == path.to_string_lossy().as_ref())
        .expect("double-state row projection");
    assert_eq!(row["dirty_files"], 1);
    assert_eq!(row["dirt"]["staged"], 1);
    assert_eq!(row["dirt"]["unstaged"], 1);
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

#[test]
fn a_run_that_never_finished_measuring_keeps_the_older_figure() {
    let _guard = env_lock();
    no_herdr();

    let fixture = Fixture::new("size-cache-quick-quit");
    let worktree = fixture.active_worktree("kept-figure");
    let cache = fixture.root().join("sizes.jsonl");

    // A finished run remembers its figure.
    let mut measured = pipeline::scan(&config_for(&fixture.repo)).expect("scan");
    disk::measure_all(&mut measured, &AtomicBool::new(false));
    let Size::Bytes(bytes) = measured.find(&worktree).expect("row").size else {
        panic!("the walk must have measured the worktree");
    };
    disk::remember(&measured, &cache);

    // A quick open-and-quit: every row still pending or provisional, nothing
    // measured. Writing the cache back must not lose the older figure.
    let mut unfinished = pipeline::scan(&config_for(&fixture.repo)).expect("rescan");
    disk::recall(&mut unfinished, &cache);
    disk::remember(&unfinished, &cache);

    let mut third = pipeline::scan(&config_for(&fixture.repo)).expect("third scan");
    disk::recall(&mut third, &cache);
    assert_eq!(
        third.find(&worktree).expect("row").size,
        Size::Provisional(bytes),
        "the figure survives a run that never finished measuring"
    );
}

// ---------------------------------------------------------------------------
// The occupancy join
// ---------------------------------------------------------------------------

fn pane(pane_id: &str, cwd: Option<&str>, foreground_cwd: Option<&str>) -> shear::herdr::PaneCwd {
    shear::herdr::PaneCwd {
        pane_id: pane_id.into(),
        workspace_id: pane_id.split(':').next().map(str::to_string),
        cwd: cwd.map(PathBuf::from),
        foreground_cwd: foreground_cwd.map(PathBuf::from),
    }
}

#[test]
fn occupancy_is_component_wise_and_never_matches_a_sibling_prefix() {
    let panes = [
        // Inside: the checkout itself, and a directory below it.
        pane("w1:p1", Some("/scratch/wt"), None),
        pane("w1:p2", Some("/scratch/wt/src/deep"), None),
        // Outside: a sibling whose name merely starts with the checkout's, the
        // parent, and an unrelated tree. `/scratch/wt-2` is the case a string
        // prefix match would get wrong.
        pane("w2:p1", Some("/scratch/wt-2"), None),
        pane("w2:p2", Some("/scratch"), None),
        pane("w2:p3", Some("/elsewhere/wt"), None),
    ];
    let occupants = pipeline::occupants_of(Path::new("/scratch/wt"), &panes, None);
    let ids: Vec<&str> = occupants.iter().map(|o| o.pane_id.as_str()).collect();
    assert_eq!(ids, ["w1:p1", "w1:p2"]);
}

#[test]
fn the_foreground_cwd_is_preferred_and_either_cwd_counts() {
    // The shell sits at the repo root while its foreground process works inside
    // the checkout — the case `cwd` alone would miss.
    let foreground_only = [pane("w1:p1", Some("/scratch"), Some("/scratch/wt/build"))];
    let occupants = pipeline::occupants_of(Path::new("/scratch/wt"), &foreground_only, None);
    assert_eq!(occupants.len(), 1);
    assert_eq!(occupants[0].cwd, PathBuf::from("/scratch/wt/build"));

    // Both inside: the foreground process's cwd is the one recorded, because it
    // names what is actually running there.
    let both = [pane("w1:p1", Some("/scratch/wt"), Some("/scratch/wt/src"))];
    let occupants = pipeline::occupants_of(Path::new("/scratch/wt"), &both, None);
    assert_eq!(occupants[0].cwd, PathBuf::from("/scratch/wt/src"));
}

#[test]
fn the_holding_workspaces_own_panes_are_excepted_and_nobody_elses_are() {
    // w1 holds the checkout open; removing it through herdr closes w1's panes
    // with it. The pane from w2 would be left standing in a deleted directory,
    // so it occupies even though a workspace holds the checkout open.
    let panes = [
        pane("w1:p1", Some("/scratch/wt"), None),
        pane("w1:p2", Some("/scratch/wt/src"), None),
        pane("w2:p1", Some("/scratch/wt/deep"), None),
    ];
    let occupants = pipeline::occupants_of(Path::new("/scratch/wt"), &panes, Some("w1"));
    let ids: Vec<&str> = occupants.iter().map(|o| o.pane_id.as_str()).collect();
    assert_eq!(ids, ["w2:p1"]);

    // A pane herdr could not attribute to a workspace is never excepted: an
    // unattributable pane still breaks when the directory goes.
    let unattributed = [shear::herdr::PaneCwd {
        pane_id: "p9".into(),
        workspace_id: None,
        cwd: Some(PathBuf::from("/scratch/wt")),
        foreground_cwd: None,
    }];
    let occupants = pipeline::occupants_of(Path::new("/scratch/wt"), &unattributed, Some("w1"));
    assert_eq!(occupants.len(), 1);
}
