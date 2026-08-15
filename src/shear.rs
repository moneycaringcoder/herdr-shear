//! The gathering pipeline: herdr and git in, one [`Inventory`] out.
//!
//! Every verb goes through [`scan`], so the review pane, the one-shot table
//! and the JSON snapshot cannot drift into three subtly different answers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::classify::{self, Facts};
use crate::config::Config;
use crate::disk;
use crate::git;
use crate::herdr::{self, Herdr};
use crate::model::{Inventory, OpenWorkspace, Repo, RepoKey};
use crate::Result;

/// Scans every repository in scope and classifies every worktree in each.
///
/// Disk sizes are left [`crate::model::Size::Pending`]; call
/// [`crate::disk::measure_all`] or let the review pane fill them in behind the
/// rendering.
pub fn scan(config: &Config) -> Result<Inventory> {
    let mut inventory = Inventory::default();

    // The herdr side is optional. Running the binary from a plain shell with
    // `--repo` is a supported way to use it, and a missing socket must degrade
    // to "no workspace information" with a note, never to an empty scan.
    let mut client = match Herdr::connect() {
        Ok(client) => Some(client),
        Err(err) => {
            inventory.notes.push(format!(
                "herdr is not reachable ({err}); worktrees held open by a workspace \
                 cannot be identified, and none will be offered for removal through herdr"
            ));
            None
        }
    };

    let (repos, mut open) = discover(config, client.as_mut(), &mut inventory.notes)?;
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
                        let label = open
                            .get(&path)
                            .map(|w| w.label.clone())
                            .unwrap_or_else(|| workspace_id.clone());
                        open.insert(
                            path,
                            OpenWorkspace {
                                workspace_id,
                                label,
                            },
                        );
                    }
                }
                Err(err) => {
                    // `not_git_worktree` is data, not a failure: a repo can be
                    // removed from disk between the snapshot and this call.
                    if herdr::error_code(&*err) != Some(herdr::ERR_NOT_GIT) {
                        inventory.notes.push(format!(
                            "could not ask herdr which checkouts are open in {}: {err}",
                            repo.root.display()
                        ));
                    }
                }
            }
        }
    }

    let now = SystemTime::now();
    let stale_after = config.stale_after();

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
                        unstaged: 1,
                        ..Default::default()
                    }
                }
            };

            let branch_row = worktree
                .head
                .branch()
                .and_then(|name| branch_by_name.get(name).copied());

            let merged_into = match (&integration, worktree.head.branch(), &worktree.head) {
                (Some(reference), Some(branch), _) if merged.iter().any(|m| m == branch) => {
                    Some(reference.clone())
                }
                (Some(reference), None, crate::model::Head::Detached) => {
                    match worktree.head_oid.as_deref() {
                        Some(oid) => {
                            match git::is_ancestor(&repo.root, oid, reference, config.git_timeout) {
                                Ok(true) => Some(reference.clone()),
                                Ok(false) => None,
                                Err(err) => {
                                    inventory.notes.push(format!(
                                        "{}: could not test whether the detached HEAD is merged \
                                         ({err})",
                                        worktree.path.display()
                                    ));
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                }
                _ => None,
            };

            let facts = Facts {
                upstream: branch_row.map(|b| b.upstream.clone()).unwrap_or_default(),
                last_commit: branch_row.and_then(|b| b.tip),
                open_workspace: open.get(&worktree.path).cloned(),
                merged_into,
                dirt,
                worktree,
            };
            inventory
                .candidates
                .push(classify::classify(facts, stale_after, now));
        }
    }

    inventory.repos = repos;
    Ok(inventory)
}

/// The repositories in scope, and the checkouts the session holds open.
///
/// `--repo` replaces the session entirely; otherwise the session's repositories
/// plus any `extra_repos` from the config file are scanned, plus the current
/// directory's repository when there is one.
fn discover(
    config: &Config,
    client: Option<&mut Herdr>,
    notes: &mut Vec<String>,
) -> Result<(Vec<Repo>, BTreeMap<PathBuf, OpenWorkspace>)> {
    let mut repos: Vec<Repo> = Vec::new();
    let mut open: BTreeMap<PathBuf, OpenWorkspace> = BTreeMap::new();

    if !config.only_repos.is_empty() {
        for path in &config.only_repos {
            push_repo_at(path, config, &mut repos, notes);
        }
        return Ok((repos, open));
    }

    if let Some(client) = client {
        match client.session_repos() {
            Ok(session) => {
                for repo in session {
                    for (path, workspace) in repo.open {
                        open.insert(path, workspace);
                    }
                    push_repo(
                        Repo {
                            key: repo.key,
                            root: repo.root,
                            name: repo.name,
                        },
                        &mut repos,
                    );
                }
            }
            Err(err) => notes.push(format!(
                "could not read the herdr session ({err}); only explicitly named repositories \
                 will be scanned"
            )),
        }
    }

    for path in &config.extra_repos {
        push_repo_at(path, config, &mut repos, notes);
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_repo_at(&cwd, config, &mut repos, notes);
    }

    Ok((repos, open))
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

fn push_repo(repo: Repo, repos: &mut Vec<Repo>) {
    if !repos.iter().any(|r| r.key == repo.key) {
        repos.push(repo);
    }
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
                // `null` means the question could not be asked, which is not the
                // same as `false` and must not be conflated by a consumer.
                "merged_into": candidate.merged_into,
                "last_commit_unix": candidate.last_commit.and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs())
                }),
                "locked": candidate.worktree.locked.as_ref().map(|l| json!({"reason": l.reason})),
                "prunable_reason": candidate.worktree.prunable,
                "open_workspace": candidate.open_workspace.as_ref().map(|w| json!({
                    "workspace_id": w.workspace_id,
                    "label": w.label,
                })),
                "bytes": match candidate.size {
                    Size::Bytes(bytes) => json!(bytes),
                    Size::Gone => json!(0),
                    Size::Pending | Size::Failed => serde_json::Value::Null,
                },
                "size_state": match candidate.size {
                    Size::Bytes(_) => "measured",
                    Size::Gone => "gone",
                    Size::Pending => "pending",
                    Size::Failed => "failed",
                },
                "notes": candidate.worktree.notes,
            }))
            .collect::<Vec<_>>(),
        "notes": inventory.notes,
    })
}

/// Total bytes a set of candidates would reclaim. Only `Size::Bytes` counts:
/// a pending or failed measurement contributes nothing rather than a plausible
/// zero, and the caller is expected to say how many rows were not counted.
pub fn reclaimable<'a>(
    candidates: impl Iterator<Item = &'a crate::model::Candidate>,
) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut unknown = 0usize;
    for candidate in candidates {
        match candidate.size {
            crate::model::Size::Bytes(n) => bytes = bytes.saturating_add(n),
            crate::model::Size::Gone => {}
            crate::model::Size::Pending | crate::model::Size::Failed => unknown += 1,
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
