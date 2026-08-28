//! The gathering pipeline: herdr and git in, one [`Inventory`] out.
//!
//! Every verb goes through [`scan`], so the review pane, the one-shot table
//! and the JSON snapshot cannot drift into three subtly different answers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::classify::{self, Facts, HerdrVisibility};
use crate::config::{self, Config};
use crate::disk;
use crate::git;
use crate::herdr::{self, Herdr};
use crate::model::{Inventory, Merged, OpenWorkspace, Repo, RepoKey, Size};
use crate::Result;

/// Scans every repository in scope and classifies every worktree in each.
///
/// Disk sizes start [`crate::model::Size::Pending`] when measurement is enabled
/// and settle to [`crate::model::Size::Skipped`] during the scan when it is
/// disabled. Enabled callers may use [`crate::disk::measure_all`] or let the
/// review pane fill pending sizes in behind the rendering.
pub fn scan(config: &Config) -> Result<Inventory> {
    let mut inventory = Inventory::default();
    let herdr_expected = [
        "HERDR_PLUGIN_ID",
        "HERDR_SOCKET_PATH",
        "HERDR_PLUGIN_CONTEXT_JSON",
        "HERDR_PLUGIN_ROOT",
    ]
    .iter()
    .any(|key| config::non_empty_env(key).is_some());

    // The herdr side is optional. Running the binary from a plain shell with
    // `--repo` is a supported way to use it, and a missing socket must degrade
    // to "no workspace information" with a note, never to an empty scan.
    let (mut client, initial_herdr_visibility) = match Herdr::connect() {
        Ok(client) => (Some(client), HerdrVisibility::Complete),
        Err(err) => {
            let visibility = if herdr_expected {
                HerdrVisibility::Incomplete
            } else {
                HerdrVisibility::Standalone
            };
            let consequence = if herdr_expected {
                "herdr workspace and pane visibility is incomplete, so no affected worktree \
                 will be classified safe"
            } else {
                "this is a standalone scan, so the existing git-only safety rule remains in use"
            };
            inventory.notes.push(format!(
                "herdr is not reachable ({err}); worktrees held open by a workspace and panes \
                 working inside a checkout cannot be identified; {consequence}, and none will \
                 be offered for removal through herdr"
            ));
            (None, visibility)
        }
    };

    let discovery = discover(
        config,
        client.as_mut(),
        initial_herdr_visibility,
        &mut inventory.notes,
    )?;
    let repos = discovery.repos;
    let mut open = discovery.open;
    let panes = discovery.panes;
    let workspaces = discovery.workspaces;
    let herdr_visibility = discovery.herdr_visibility;
    let mut incomplete_herdr_repos = BTreeSet::new();
    if repos.is_empty() {
        inventory.notes.push(
            "no git repositories in scope. Pass --repo <PATH> to scan one explicitly, or open \
             a repository as a herdr workspace."
                .into(),
        );
        return Ok(inventory);
    }

    // herdr's own view of which checkouts are open. Preferred over the snapshot
    // join because herdr does the path matching itself, and the snapshot echoes
    // back whatever path a workspace was created with.
    if let Some(client) = client.as_mut() {
        for repo in &repos {
            match client.open_workspaces(&repo.root) {
                Ok(pairs) => {
                    for (path, workspace_id) in pairs {
                        // The snapshot carries what `worktree.list` does not:
                        // the workspace's label and what its agents are doing.
                        // Joined by workspace id, because a workspace can
                        // arrive in the snapshot with no `worktree` key while
                        // holding this checkout open — verified live — and the
                        // id is then the only join that still names it.
                        let summary = workspaces.get(&workspace_id);
                        let label = summary
                            .map(|w| w.label.clone())
                            .unwrap_or_else(|| workspace_id.clone());
                        let agent_status = summary.and_then(|w| w.agent_status);
                        open.insert(
                            path,
                            OpenWorkspace {
                                workspace_id,
                                label,
                                agent_status,
                            },
                        );
                    }
                }
                Err(err) => {
                    // `not_git_worktree` is data, not failed visibility: Herdr
                    // positively answered that this root has no worktree view.
                    if herdr::error_code(&*err) != Some(herdr::ERR_NOT_GIT) {
                        incomplete_herdr_repos.insert(repo.key.clone());
                        inventory.notes.push(format!(
                            "{}: could not complete herdr worktree.list ({err}); workspace visibility \
                             is unknown for this repository, so its worktrees cannot be classified safe",
                            repo.root.display()
                        ));
                    }
                }
            }
        }
    }

    let now = SystemTime::now();

    for repo in &repos {
        let worktrees = match git::worktrees(&repo.root, config.git_timeout) {
            Ok(worktrees) => worktrees,
            Err(err) => {
                inventory
                    .notes
                    .push(format!("skipping {}: {err}", repo.root.display()));
                continue;
            }
        };

        let branches = match git::branches(&repo.root, config.git_timeout) {
            Ok(rows) => rows,
            Err(err) => {
                inventory.notes.push(format!(
                    "{}: could not read branch state ({err}); upstream and staleness are unknown \
                     for every worktree in this repo",
                    repo.root.display()
                ));
                Vec::new()
            }
        };
        let branch_by_name: BTreeMap<&str, &git::BranchRow> =
            branches.iter().map(|b| (b.name.as_str(), b)).collect();

        let integration = match git::integration_ref(
            &repo.root,
            config.integration_ref.as_deref(),
            config.git_timeout,
        ) {
            Ok(reference) => reference,
            Err(err) => {
                inventory.notes.push(format!(
                    "{}: could not resolve an integration ref ({err}); no worktree here can \
                         be called merged",
                    repo.root.display()
                ));
                None
            }
        };
        if integration.is_none() {
            inventory.notes.push(format!(
                "{}: no integration ref resolved ({}), so `merged` is unknown here rather than \
                 false, and nothing in this repo can be classified safe",
                repo.root.display(),
                git::NOTE_NO_INTEGRATION_REF
            ));
        }

        let merged: Vec<String> = match integration.as_deref() {
            Some(reference) => {
                match git::merged_branches(&repo.root, reference, config.git_timeout) {
                    Ok(names) => names,
                    Err(err) => {
                        inventory.notes.push(format!(
                            "{}: could not list branches merged into {reference} ({err})",
                            repo.root.display()
                        ));
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };

        for worktree in worktrees {
            let dirt = match git::dirt(&worktree.path, config.git_timeout) {
                Ok(dirt) => dirt,
                Err(err) => {
                    inventory.notes.push(format!(
                        "{}: could not read working-tree status ({err}); treating it as dirty, \
                         because an unreadable worktree is not a safe one",
                        worktree.path.display()
                    ));
                    // Deliberately conservative: one unreadable file is enough
                    // to keep it out of every automatic selection.
                    crate::model::Dirt {
                        paths: 1,
                        unstaged: 1,
                        ..Default::default()
                    }
                }
            };

            let branch_row = worktree
                .head
                .branch()
                .and_then(|name| branch_by_name.get(name).copied());

            // Three states, deliberately. `Merged::No` means the test ran and
            // said no; `Merged::Unknown` means it could not run. Collapsing them
            // is how a tool like this starts recommending the wrong thing.
            let merged_state = match (&integration, worktree.head.branch()) {
                (None, _) => Merged::Unknown,
                (Some(reference), Some(branch)) => {
                    if merged.iter().any(|m| m == branch) {
                        Merged::Into(reference.clone())
                    } else {
                        Merged::No(reference.clone())
                    }
                }
                // No branch: a detached HEAD can still be tested by commit; an
                // unborn or bare one has no commit to test at all.
                (Some(reference), None) => match (&worktree.head, worktree.head_oid.as_deref()) {
                    (crate::model::Head::Detached, Some(oid)) => {
                        match git::is_ancestor(&repo.root, oid, reference, config.git_timeout) {
                            Ok(true) => Merged::Into(reference.clone()),
                            Ok(false) => Merged::No(reference.clone()),
                            Err(err) => {
                                inventory.notes.push(format!(
                                    "{}: could not test whether the detached HEAD is merged \
                                     ({err})",
                                    worktree.path.display()
                                ));
                                Merged::Unknown
                            }
                        }
                    }
                    _ => Merged::Unknown,
                },
            };

            let path = worktree.path.to_string_lossy();
            let branch = worktree.head.branch();
            let protected = config
                .protect
                .iter()
                .find(|pattern| {
                    config::pattern_matches(pattern, &path)
                        || branch.is_some_and(|name| config::branch_pattern_matches(pattern, name))
                })
                .cloned();

            // A pane sitting inside a checkout is the occupancy
            // `open_workspace_id` cannot see — a shell that merely `cd`-ed in
            // from another workspace. Only the holding workspace's own panes
            // are excepted: removing its checkout through herdr closes them
            // with it, while a foreign pane would be left standing in a
            // directory that no longer exists.
            let open_workspace = open.get(&worktree.path).cloned();
            let occupants = occupants_of(
                &worktree.path,
                &panes,
                open_workspace.as_ref().map(|w| w.workspace_id.as_str()),
            );

            let facts = Facts {
                herdr_visibility: if herdr_visibility == HerdrVisibility::Incomplete
                    || incomplete_herdr_repos.contains(&repo.key)
                {
                    HerdrVisibility::Incomplete
                } else {
                    herdr_visibility
                },
                upstream: branch_row.map(|b| b.upstream.clone()).unwrap_or_default(),
                last_commit: branch_row.and_then(|b| b.tip),
                open_workspace,
                occupants,
                protected,
                merged: merged_state,
                dirt,
                worktree,
            };
            // Policy resolution belongs here because this caller already knows
            // the merge and upstream facts; the classifier stays a pure
            // function of one threshold.
            let stale_after = config.stale_after_for(&facts.merged, &facts.upstream);
            let mut candidate = classify::classify(facts, stale_after, now);
            if !config.measure_disk {
                candidate.size = Size::Skipped;
            }
            inventory.candidates.push(candidate);
        }
    }
    let protected = inventory
        .candidates
        .iter()
        .filter(|candidate| candidate.protected.is_some())
        .count();
    if protected == 1 {
        inventory.notes.push(
            "1 protected worktree remains visible and blocked by a pattern in config.json; \
             edit or remove that pattern to unblock it"
                .into(),
        );
    } else if protected > 1 {
        inventory.notes.push(format!(
            "{protected} protected worktrees remain visible and blocked by patterns in \
             config.json; edit or remove those patterns to unblock them"
        ));
    }

    inventory.repos = repos;
    Ok(inventory)
}

/// What discovery found: the repositories in scope, the checkouts the session
/// holds open (keyed by path), where every pane's processes are sitting, and
/// every workspace's label and agent status (keyed by workspace id) — the
/// latter carried separately because a workspace can hold a checkout open
/// while arriving in the snapshot with no `worktree` key, verified live, and
/// the id is then the only join that still names it.
struct Discovery {
    repos: Vec<Repo>,
    open: BTreeMap<PathBuf, OpenWorkspace>,
    panes: Vec<herdr::PaneCwd>,
    workspaces: BTreeMap<String, herdr::WorkspaceSummary>,
    herdr_visibility: HerdrVisibility,
}

/// The repositories in scope, and the checkouts the session holds open.
///
/// `--repo` replaces discovery entirely. Otherwise the session and configured
/// repositories are supplemented from Herdr's invocation context: focused pane
/// cwd first, then workspace cwd. Process cwd is a direct-CLI fallback only;
/// Herdr runs installed plugins from their own root, which is never a target.
fn discover(
    config: &Config,
    client: Option<&mut Herdr>,
    initial_herdr_visibility: HerdrVisibility,
    notes: &mut Vec<String>,
) -> Result<Discovery> {
    let mut repos: Vec<Repo> = Vec::new();
    let mut open: BTreeMap<PathBuf, OpenWorkspace> = BTreeMap::new();
    let mut herdr_visibility = initial_herdr_visibility;
    let plugin_root = config::plugin_env().plugin_root().map(Path::to_path_buf);
    let (plugin_context, malformed_plugin_context) = match herdr::plugin_context() {
        Ok(context) => (context, false),
        Err(err) => {
            herdr_visibility = HerdrVisibility::Incomplete;
            notes.push(format!(
                "could not read HERDR_PLUGIN_CONTEXT_JSON ({err}); repository discovery from \
                 Herdr's focused pane and workspace is unavailable, so affected worktrees \
                 cannot be classified safe; the plugin process directory was not used"
            ));
            (None, true)
        }
    };

    // `--repo` narrows which repositories are scanned. It does not make the
    // session's workspaces stop existing, so the snapshot is still read for
    // them: `worktree.list` knows *that* a checkout is open but not what the
    // workspace is called, and a refusal that says "workspace w1X (w1X)" because
    // the label fell back to the id is a worse sentence than one that just says
    // "workspace w1X".
    let session = match client {
        Some(client) => match client.session_view() {
            Ok(session) => session,
            Err(err) => {
                herdr_visibility = HerdrVisibility::Incomplete;
                notes.push(format!(
                    "could not read the herdr session.snapshot ({err}); repository discovery, \
                     workspace names, agent activity and pane occupancy are unavailable, so \
                     affected worktrees cannot be classified safe; without --repo only \
                     explicitly named repositories will be scanned"
                ));
                herdr::SessionView {
                    repos: Vec::new(),
                    workspaces: Vec::new(),
                    panes: Vec::new(),
                }
            }
        },
        None => herdr::SessionView {
            repos: Vec::new(),
            workspaces: Vec::new(),
            panes: Vec::new(),
        },
    };
    let panes = session.panes;
    let workspaces: BTreeMap<String, herdr::WorkspaceSummary> = session
        .workspaces
        .into_iter()
        .map(|w| (w.workspace_id.clone(), w))
        .collect();
    let session = session.repos;
    for repo in &session {
        for (path, workspace) in &repo.open {
            open.insert(path.clone(), workspace.clone());
        }
    }

    if !config.only_repos.is_empty() {
        for path in &config.only_repos {
            push_repo_at(path, config, &mut repos, notes);
        }
        return Ok(Discovery {
            repos,
            open,
            panes,
            workspaces,
            herdr_visibility,
        });
    }

    for repo in session {
        push_repo(
            Repo {
                key: repo.key,
                root: repo.root,
                name: repo.name,
            },
            &mut repos,
        );
    }

    for path in &config.extra_repos {
        push_repo_at(path, config, &mut repos, notes);
    }

    match plugin_context {
        Some(context) => {
            let focused = context
                .focused_pane_cwd()
                .map(|path| ("focused pane cwd", path));
            let workspace = context.workspace_cwd().map(|path| ("workspace cwd", path));
            if focused.is_none() && workspace.is_none() {
                herdr_visibility = HerdrVisibility::Incomplete;
                notes.push(
                    "HERDR_PLUGIN_CONTEXT_JSON names neither a focused pane cwd nor a \
                     workspace cwd; repository discovery is incomplete, so affected \
                     worktrees cannot be classified safe; the plugin process directory \
                     was not used"
                        .into(),
                );
            } else {
                for (source, path) in [focused, workspace].into_iter().flatten() {
                    if source == "workspace cwd"
                        && context
                            .focused_pane_cwd()
                            .is_some_and(|focused| focused == path)
                    {
                        continue;
                    }
                    if push_context_repo_at(
                        path,
                        source,
                        plugin_root.as_deref(),
                        config,
                        &mut repos,
                        notes,
                    ) {
                        break;
                    }
                    herdr_visibility = HerdrVisibility::Incomplete;
                }
            }
        }
        None if malformed_plugin_context => {}
        None => {
            let invoked_by_herdr =
                plugin_root.is_some() || config::non_empty_env("HERDR_PLUGIN_ID").is_some();
            if invoked_by_herdr {
                herdr_visibility = HerdrVisibility::Incomplete;
                notes.push(
                    "Herdr invoked the installed plugin without HERDR_PLUGIN_CONTEXT_JSON; \
                     repository discovery is incomplete, so affected worktrees cannot be \
                     classified safe; the plugin process directory was not used"
                        .into(),
                );
            } else if let Ok(cwd) = std::env::current_dir() {
                push_repo_at(&cwd, config, &mut repos, notes);
            }
        }
    }

    Ok(Discovery {
        repos,
        open,
        panes,
        workspaces,
        herdr_visibility,
    })
}

fn push_repo_at(path: &Path, config: &Config, repos: &mut Vec<Repo>, notes: &mut Vec<String>) {
    match git::repo_at(path, config.git_timeout) {
        Ok(Some(repo)) => push_repo(repo, repos),
        // Not a repository is ordinary: the current directory usually is not
        // one, and saying so on every run would be noise. An explicit `--repo`
        // that is not a repository is different, and is reported.
        Ok(None) => {
            if config.only_repos.iter().any(|p| p == path) {
                notes.push(format!("--repo {}: not a git repository", path.display()));
            }
        }
        Err(err) => notes.push(format!("{}: {err}", path.display())),
    }
}

/// Adds the repository selected by Herdr's invocation context.
///
/// Failure is deliberately distinct from ordinary direct-CLI discovery: an
/// installed action has named the repository it means, so falling back to the
/// plugin's process cwd would scan the wrong checkout and could widen safety.
fn push_context_repo_at(
    path: &Path,
    source: &str,
    plugin_root: Option<&Path>,
    config: &Config,
    repos: &mut Vec<Repo>,
    notes: &mut Vec<String>,
) -> bool {
    if plugin_root.is_some_and(|root| same_or_below(path, root)) {
        notes.push(format!(
            "Herdr {source} {} points at the installed plugin root; repository discovery is \
             incomplete, so affected worktrees cannot be classified safe",
            path.display()
        ));
        return false;
    }

    match git::repo_at(path, config.git_timeout) {
        Ok(Some(repo)) => {
            if plugin_root.is_some_and(|root| repo_is_plugin_root(&repo, root, config)) {
                notes.push(format!(
                    "Herdr {source} {} resolves to the installed plugin's repository; \
                     repository discovery is incomplete, so affected worktrees cannot be \
                     classified safe",
                    path.display()
                ));
                false
            } else {
                push_repo(repo, repos);
                true
            }
        }
        Ok(None) => {
            notes.push(format!(
                "Herdr {source} {} is not inside a readable git checkout; repository discovery \
                 is incomplete, so affected worktrees cannot be classified safe",
                path.display()
            ));
            false
        }
        Err(err) => {
            notes.push(format!(
                "could not discover a repository from Herdr {source} {} ({err}); repository \
                 discovery is incomplete, so affected worktrees cannot be classified safe",
                path.display()
            ));
            false
        }
    }
}

fn same_or_below(path: &Path, root: &Path) -> bool {
    path == root
        || path.starts_with(root)
        || match (path.canonicalize(), root.canonicalize()) {
            (Ok(path), Ok(root)) => path == root || path.starts_with(root),
            _ => false,
        }
}

fn repo_is_plugin_root(repo: &Repo, plugin_root: &Path, config: &Config) -> bool {
    same_or_below(plugin_root, &repo.root)
        || git::repo_at(plugin_root, config.git_timeout)
            .ok()
            .flatten()
            .is_some_and(|plugin_repo| plugin_repo.key == repo.key)
}

fn push_repo(repo: Repo, repos: &mut Vec<Repo>) {
    if !repos.iter().any(|r| r.key == repo.key) {
        repos.push(repo);
    }
}

/// The panes whose working directory is inside `path`, excepting those that
/// belong to `own_workspace` — the workspace holding `path` open, whose panes
/// are closed with it when the checkout is removed through herdr. Byte-exact
/// and component-wise, like every other path comparison in the crate: `path`
/// itself or anything below it counts, `path`-as-a-prefix-of-a-sibling-name
/// does not.
pub fn occupants_of(
    path: &Path,
    panes: &[herdr::PaneCwd],
    own_workspace: Option<&str>,
) -> Vec<crate::model::Occupant> {
    let mut occupants = Vec::new();
    for pane in panes {
        if own_workspace.is_some() && pane.workspace_id.as_deref() == own_workspace {
            continue;
        }
        // The foreground process's cwd is preferred when both match: it is the
        // one that names what is actually running there.
        let hit = [pane.foreground_cwd.as_ref(), pane.cwd.as_ref()]
            .into_iter()
            .flatten()
            .find(|cwd| cwd.starts_with(path));
        if let Some(cwd) = hit {
            occupants.push(crate::model::Occupant {
                pane_id: pane.pane_id.clone(),
                cwd: cwd.clone(),
            });
        }
    }
    occupants
}

/// `--json`: the same inventory the table renders, machine-readable.
///
/// Sizes are measured here rather than left pending: a script reading JSON has
/// no way to ask for them later.
pub fn run_json(config: &Config) -> Result<()> {
    let mut inventory = scan(config)?;
    if config.measure_disk {
        disk::measure_all(&mut inventory, &AtomicBool::new(false));
    }
    println!("{}", serde_json::to_string_pretty(&to_json(&inventory))?);
    Ok(())
}

/// JSON projection of an inventory. Written by hand rather than derived, so the
/// wire shape is a deliberate choice and adding a field to a model struct cannot
/// silently change a public interface.
pub fn to_json(inventory: &Inventory) -> serde_json::Value {
    use crate::model::{Head, Size};
    use serde_json::json;

    json!({
        "repos": inventory
            .repos
            .iter()
            .map(|repo| json!({
                "key": repo.key.0,
                "root": repo.root.to_string_lossy(),
                "name": repo.name,
            }))
            .collect::<Vec<_>>(),
        "worktrees": inventory
            .candidates
            .iter()
            .map(|candidate| json!({
                "path": candidate.worktree.path.to_string_lossy(),
                "repo": candidate.worktree.repo.0,
                "repo_root": candidate.worktree.repo_root.to_string_lossy(),
                "is_main": candidate.worktree.is_main,
                "head": match &candidate.worktree.head {
                    Head::Branch(name) => json!({"kind": "branch", "branch": name}),
                    Head::Detached => json!({"kind": "detached"}),
                    Head::Unborn => json!({"kind": "unborn"}),
                    Head::Bare => json!({"kind": "bare"}),
                },
                "head_oid": candidate.worktree.head_oid,
                "verdict": candidate.verdict.label(),
                "classes": candidate.classes.iter().map(|c| c.label()).collect::<Vec<_>>(),
                "reason": candidate.reason,
                "dirty_files": candidate.dirt.total(),
                "dirt": {
                    "staged": candidate.dirt.staged,
                    "unstaged": candidate.dirt.unstaged,
                    "untracked": candidate.dirt.untracked,
                    "unmerged": candidate.dirt.unmerged,
                },
                "upstream": candidate.upstream.name,
                "upstream_gone": candidate.upstream.gone,
                // `null` means the question could not be asked, which is not
                // the same as `false` and must not be conflated by a consumer.
                "merged": candidate.merged.as_bool(),
                "merged_against": candidate.merged.against(),
                "last_commit_unix": candidate.last_commit.and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs())
                }),
                "locked": candidate.worktree.locked.as_ref().map(|l| json!({"reason": l.reason})),
                // Projected by hand, like `locked` above and everything else
                // here. Deriving `Serialize` on the model type instead would
                // make the wire shape a side effect of a struct definition,
                // which is the thing this function exists to prevent.
                "prunable": candidate.worktree.prunable.as_ref().map(|p| json!({"reason": p.reason})),
                "open_workspace": candidate.open_workspace.as_ref().map(|w| json!({
                    "workspace_id": w.workspace_id,
                    "label": w.label,
                    // `null` means herdr did not say, or said something this
                    // build does not know; either way it is not `unknown`,
                    // which is a state herdr reports.
                    "agent_status": w.agent_status.map(|s| s.label()),
                })),
                "occupants": candidate.occupants.iter().map(|o| json!({
                    "pane_id": o.pane_id,
                    "cwd": o.cwd.to_string_lossy(),
                })).collect::<Vec<_>>(),
                "bytes": match candidate.size {
                    Size::Bytes(bytes) => json!(bytes),
                    Size::Gone => json!(0),
                    // Neither a remembered claim nor an unfinished, skipped, or
                    // failed walk is a measurement a script may trust.
                    Size::Pending
                    | Size::Skipped
                    | Size::Provisional(_)
                    | Size::Failed => serde_json::Value::Null,
                },
                "size_state": match candidate.size {
                    Size::Bytes(_) => "measured",
                    Size::Gone => "gone",
                    Size::Pending => "pending",
                    Size::Skipped => "skipped",
                    Size::Provisional(_) => "provisional",
                    Size::Failed => "failed",
                },
                "notes": candidate.worktree.notes,
            }))
            .collect::<Vec<_>>(),
        "notes": inventory.notes,
    })
}

/// Total bytes a set of candidates would reclaim. Only `Size::Bytes` counts:
/// pending, skipped, provisional, and failed sizing contributes nothing rather
/// than a plausible zero, and the caller is expected to distinguish why rows
/// were not counted when presenting the result.
pub fn reclaimable<'a>(
    candidates: impl Iterator<Item = &'a crate::model::Candidate>,
) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut unknown = 0usize;
    for candidate in candidates {
        match candidate.size {
            crate::model::Size::Bytes(n) => bytes = bytes.saturating_add(n),
            crate::model::Size::Gone => {}
            // Counted as unknown, never as a figure. Presentation distinguishes
            // deliberate skipping from an unfinished or failed measurement.
            crate::model::Size::Pending
            | crate::model::Size::Skipped
            | crate::model::Size::Provisional(_)
            | crate::model::Size::Failed => unknown += 1,
        }
    }
    (bytes, unknown)
}

/// Resolves a user-supplied path to the candidate it names, accepting either the
/// exact path git reports or any path that canonicalizes to it.
pub fn resolve<'a>(inventory: &'a Inventory, path: &Path) -> Option<&'a crate::model::Candidate> {
    if let Some(found) = inventory.find(path) {
        return Some(found);
    }
    let canonical = path.canonicalize().ok()?;
    inventory.candidates.iter().find(|candidate| {
        candidate.worktree.path == canonical
            || candidate
                .worktree
                .path
                .canonicalize()
                .map(|p| p == canonical)
                .unwrap_or(false)
    })
}

/// Marker used by [`RepoKey`] consumers that need a stable placeholder in tests.
pub fn unknown_repo() -> RepoKey {
    RepoKey(String::new())
}
