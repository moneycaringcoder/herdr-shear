//! Thin shear-specific wrapper around Crook's herdr socket client.

use std::fmt;
use std::path::{Path, PathBuf};

use crook::client::{Client, Error as CrookError, RetrySafety};
use crook::env::PluginEnv;
use serde_json::{json, Value};

use crate::config;
use crate::model::{OpenWorkspace, RepoKey};
use crate::Result;

/// A herdr error envelope, carried as a real error type so callers can tell
/// `dirty_worktree_requires_force` (a guard doing its job) from a transport
/// failure (we are blind and should say so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

/// Error code from a herdr error envelope, or `None` for a transport failure.
pub fn error_code<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    err.downcast_ref::<HerdrError>().map(|e| e.code.as_str())
}

/// herdr returns this when `worktree.remove` is asked to remove a checkout with
/// uncommitted changes. Verified live against 0.8.0.
pub const ERR_DIRTY: &str = "dirty_worktree_requires_force";
/// Everything else `worktree.remove` can refuse with, including a locked
/// worktree. Verified live: a locked worktree comes back as
/// `worktree_remove_failed` carrying git's own "cannot remove a locked working
/// tree" text, *not* as a distinct code.
pub const ERR_REMOVE_FAILED: &str = "worktree_remove_failed";
/// `worktree.list` returns this for a path that is not inside a git work tree.
/// It is data ("not a repo"), never a transport failure.
pub const ERR_NOT_GIT: &str = "not_git_worktree";
pub const ERR_WORKSPACE_NOT_FOUND: &str = "workspace_not_found";

#[derive(Debug)]
pub struct Herdr {
    client: Client,
}

/// One repository the session has a workspace in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRepo {
    pub key: RepoKey,
    pub root: PathBuf,
    pub name: String,
    /// Checkout paths the session currently holds open in this repo, with the
    /// workspace that holds each.
    pub open: Vec<(PathBuf, OpenWorkspace)>,
}

/// What one `session.snapshot` says: the repositories the session knows about,
/// and where every pane's processes are sitting, and every workspace's label
/// and agent status — including workspaces that arrive with no `worktree`
/// key, which still hold checkouts open that `worktree.list` can see. Read in
/// one call because all of it comes from the same snapshot, and asking twice
/// could describe two different sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub repos: Vec<SessionRepo>,
    pub panes: Vec<PaneCwd>,
    pub workspaces: Vec<WorkspaceSummary>,
}

/// One workspace's identity, from `session.snapshot.workspaces`. Carried for
/// every workspace, repo or not: a workspace herdr reports with no `worktree`
/// key can still hold a checkout open — verified live — and joining by
/// workspace id is what still names it then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSummary {
    pub workspace_id: String,
    pub label: String,
    pub agent_status: Option<crate::model::AgentStatus>,
}

/// One pane's working directories, from `session.snapshot.panes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCwd {
    /// The pane's id, or a placeholder when herdr reported none. A pane with a
    /// usable cwd but no id still occupies: dropping it would widen what can be
    /// removed for a reporting gap.
    pub pane_id: String,
    /// The workspace the pane belongs to, when herdr reported one. Used to
    /// except a workspace's own panes from occupying its own checkout —
    /// removing that checkout through herdr closes those panes with it.
    pub workspace_id: Option<String>,
    /// The pane's shell cwd, when herdr knows it.
    pub cwd: Option<PathBuf>,
    /// The pane's foreground process's cwd, when herdr knows it. This is the
    /// one that moves when a program inside the shell changes directory.
    pub foreground_cwd: Option<PathBuf>,
}

/// Result of `worktree.remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub path: PathBuf,
    pub workspace_id: String,
    pub forced: bool,
}

impl Herdr {
    pub fn connect() -> Result<Self> {
        let environment = PluginEnv::resolve(config::PLUGIN_ID);
        let client =
            Client::connect(environment.socket_path(), "shear").map_err(into_local_error)?;
        Ok(Self { client })
    }

    /// The repositories the session knows about, every pane's working
    /// directories, and every workspace's label and agent status, reduced from
    /// one `session.snapshot`.
    ///
    /// Workspaces with no `worktree` key are not repos and are skipped from
    /// `repos` — that is data, not an error — but they still appear in
    /// `workspaces`: verified live, a workspace can arrive with no `worktree`
    /// key while `worktree.list` reports it holding a checkout open. A
    /// repository with an **unborn HEAD** also arrives with no `worktree` key,
    /// so a brand-new repo is invisible in `repos` and has to be reached with
    /// `--repo`.
    pub fn session_view(&mut self) -> Result<SessionView> {
        let result = self.call("session.snapshot", json!({}), RetrySafety::Idempotent)?;
        // The payload is `{"type":"session_snapshot","snapshot":{...}}`; the
        // arrays live one level down, under `snapshot`. Reading them off the
        // result object silently yields no repos at all, which looks exactly
        // like an idle session — so an absent `snapshot` is an error, never a
        // fallback.
        let snapshot = result.get("snapshot").ok_or_else(|| {
            format!(
                "session.snapshot returned no `snapshot` object (result type `{}`)",
                text(&result, "type").unwrap_or("missing")
            )
        })?;
        Ok(SessionView {
            repos: reduce_snapshot(snapshot),
            panes: pane_cwds(snapshot),
            workspaces: workspace_summaries(snapshot),
        })
    }

    /// herdr's own view of one repository's worktrees, used **only** for
    /// `open_workspace_id`.
    ///
    /// Verified live against 0.8.0, and the reason `git.rs` is the authority for
    /// everything else:
    ///
    /// - `label` is the *repository* name on every row, not the worktree's.
    /// - There is no locked flag at all. A locked worktree is reported exactly
    ///   like an unlocked one.
    /// - `is_prunable` is reported, but without git's reason.
    ///
    /// A path that is not in a git work tree comes back as an error envelope
    /// with code [`ERR_NOT_GIT`]; callers treat that as "not a repo".
    pub fn open_workspaces(&mut self, repo_root: &Path) -> Result<Vec<(PathBuf, String)>> {
        let result = self.call(
            "worktree.list",
            json!({ "cwd": repo_root.to_string_lossy() }),
            RetrySafety::Idempotent,
        )?;
        let worktrees = result
            .get("worktrees")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "worktree.list returned no `worktrees` array (result type `{}`)",
                    text(&result, "type").unwrap_or("missing")
                )
            })?;
        Ok(worktrees
            .iter()
            .filter_map(|worktree| {
                let path = text(worktree, "path")?;
                let workspace_id = text(worktree, "open_workspace_id")?;
                Some((PathBuf::from(path), workspace_id.to_string()))
            })
            .collect())
    }

    /// Removes a worktree that herdr holds open, via the workspace that holds
    /// it.
    ///
    /// This is the only route that keeps herdr's bookkeeping consistent: it
    /// removes the checkout, prunes the git admin entry, **and closes the
    /// workspace**, all in one call. Verified live.
    ///
    /// `force` is what git's `--force` is: it permits removing a checkout with
    /// uncommitted changes. Without it a dirty worktree comes back as
    /// [`ERR_DIRTY`] and nothing is touched.
    pub fn remove_worktree(&mut self, workspace_id: &str, force: bool) -> Result<Removed> {
        let result = self.call(
            "worktree.remove",
            json!({ "workspace_id": workspace_id, "force": force }),
            RetrySafety::Never,
        )?;
        Ok(Removed {
            path: PathBuf::from(text(&result, "path").ok_or_else(|| {
                format!(
                    "worktree.remove returned no `path` (result type `{}`)",
                    text(&result, "type").unwrap_or("missing")
                )
            })?),
            workspace_id: text(&result, "workspace_id")
                .unwrap_or(workspace_id)
                .to_string(),
            forced: result
                .get("forced")
                .and_then(Value::as_bool)
                .unwrap_or(force),
        })
    }

    /// Closes a workspace without touching its checkout. Used when a worktree
    /// the user selected is held open by a workspace and they have agreed to
    /// close it.
    pub fn close_workspace(&mut self, workspace_id: &str) -> Result<()> {
        self.call(
            "workspace.close",
            json!({ "workspace_id": workspace_id }),
            RetrySafety::Never,
        )?;
        Ok(())
    }

    pub fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        self.call(
            "notification.show",
            json!({ "title": title, "body": body }),
            RetrySafety::Never,
        )?;
        Ok(())
    }

    fn call(&self, method: &str, params: Value, retry_safety: RetrySafety) -> Result<Value> {
        self.client
            .request(method, params, retry_safety)
            .map_err(into_local_error)
    }
}

fn into_local_error(error: CrookError) -> Box<dyn std::error::Error> {
    match error {
        CrookError::Protocol { code, message } => Box::new(HerdrError { code, message }),
        error => Box::new(error),
    }
}

/// Non-empty string field, since herdr reports absent context as an empty string
/// rather than as a missing key.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], |a| a)
}

/// Drops `.` components from a path herdr reports.
///
/// herdr echoes back whatever path a workspace was created with, so one made
/// with `--cwd .` arrives as `/home/you/repos/app/.`. That would not match the
/// absolute path `git worktree list` prints, so the join against git's
/// enumeration would silently find no open workspaces at all.
fn tidy_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let tidied: PathBuf = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if tidied.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        tidied
    }
}

/// Reduces a `session.snapshot` result to one entry per distinct repository,
/// carrying the checkouts the session holds open in each.
fn reduce_snapshot(snapshot: &Value) -> Vec<SessionRepo> {
    let mut repos: Vec<SessionRepo> = Vec::new();

    for workspace in array(snapshot, "workspaces") {
        // No `worktree` key means the workspace is not a repo — or is a repo
        // with an unborn HEAD, which herdr reports the same way.
        let Some(worktree) = workspace.get("worktree").filter(|w| w.is_object()) else {
            continue;
        };
        let (Some(workspace_id), Some(repo_key), Some(checkout_path)) = (
            text(workspace, "workspace_id"),
            text(worktree, "repo_key"),
            text(worktree, "checkout_path"),
        ) else {
            continue;
        };
        let repo_root = text(worktree, "repo_root").unwrap_or(checkout_path);
        let name = text(worktree, "repo_name")
            .map(str::to_string)
            .unwrap_or_else(|| {
                Path::new(repo_root)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| repo_root.to_string())
            });
        let open = OpenWorkspace {
            workspace_id: workspace_id.to_string(),
            label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
            agent_status: text(workspace, "agent_status")
                .and_then(crate::model::AgentStatus::parse),
        };

        let key = RepoKey(repo_key.to_string());
        match repos.iter_mut().find(|r| r.key == key) {
            Some(repo) => repo.open.push((tidy_path(checkout_path), open)),
            None => repos.push(SessionRepo {
                key,
                root: tidy_path(repo_root),
                name,
                open: vec![(tidy_path(checkout_path), open)],
            }),
        }
    }

    repos
}

/// Every pane's working directories from a `session.snapshot`, for the
/// occupancy join. A pane with neither cwd is dropped — it can occupy nothing —
/// but a pane with a cwd and no id is kept under a placeholder, because an
/// unnameable occupant is still an occupant.
fn pane_cwds(snapshot: &Value) -> Vec<PaneCwd> {
    array(snapshot, "panes")
        .iter()
        .filter_map(|pane| {
            let cwd = text(pane, "cwd").map(tidy_path);
            let foreground_cwd = text(pane, "foreground_cwd").map(tidy_path);
            if cwd.is_none() && foreground_cwd.is_none() {
                return None;
            }
            Some(PaneCwd {
                pane_id: text(pane, "pane_id")
                    .unwrap_or("(pane with no id)")
                    .to_string(),
                workspace_id: text(pane, "workspace_id").map(str::to_string),
                cwd,
                foreground_cwd,
            })
        })
        .collect()
}

/// Every workspace's label and agent status from a `session.snapshot`,
/// including the ones with no `worktree` key.
fn workspace_summaries(snapshot: &Value) -> Vec<WorkspaceSummary> {
    array(snapshot, "workspaces")
        .iter()
        .filter_map(|workspace| {
            let workspace_id = text(workspace, "workspace_id")?;
            Some(WorkspaceSummary {
                workspace_id: workspace_id.to_string(),
                label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
                agent_status: text(workspace, "agent_status")
                    .and_then(crate::model::AgentStatus::parse),
            })
        })
        .collect()
}
