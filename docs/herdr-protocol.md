# herdr socket notes for shear (verified against herdr 0.8.0, protocol 19)

Working notes for `src/herdr.rs`. Everything here was verified against a live
0.8.0 server and the bundled schema (`herdr api schema --json`), not inferred
from documentation. The transport half is shared with herdr-collide and is
reproduced only in summary; the worktree half is shear's own and is where the
surprises are.

## Transport, in brief

`HERDR_SOCKET_PATH` is injected into every command herdr spawns. Framing is
newline-delimited JSON with no `jsonrpc` field; `id` must be a string and
`params` must be an object (`{}` when empty, never `null`).

**The socket answers one request per connection and then closes.** Every call
must be able to reconnect and retry once. That retry is also what carries the
client across a `herdr update --handoff`.

## `worktree.list` is not the authority on worktrees

`worktree.list` takes `{workspace_id?, cwd?}` and returns
`{source: WorktreeSourceInfo, worktrees: [WorktreeInfo]}`. `cwd` may be any path
inside the repository; **the call has no side effects** — verified by counting
workspaces before and after against a repository the session had never opened
(8 → 8, and `source.source_workspace_id` came back null).

`WorktreeInfo` carries `path`, `branch`, `is_bare`, `is_detached`,
`is_prunable`, `is_linked_worktree`, `label`, `open_workspace_id`.

Three things it does *not* carry, each verified live against a fixture repo
containing the state in question:

1. **There is no locked flag.** A worktree locked with
   `git worktree lock --reason "held for demo"` is reported exactly like an
   unlocked one. A plugin that trusted this list would happily offer somebody's
   deliberately locked worktree for removal.
2. **`label` is the repository name on every row**, not the worktree's. In a
   repo called `repo`, all seven worktrees came back with `"label": "repo"`.
   It is useless as a display name.
3. **`is_prunable` has no reason.** git's own reason ("gitdir file points to
   non-existent location") is the thing a user needs in order to tell a dead
   worktree from one on a filesystem that happens to be unmounted.

So: `git worktree list --porcelain -z` is shear's authority for enumeration and
classification, and `worktree.list` is called for exactly one field,
`open_workspace_id`.

## `worktree.remove` is keyed by workspace, not by path

```
WorktreeRemoveParams { workspace_id: string (required), force: bool = false }
```

This inverts the obvious assumption. herdr can only remove a worktree it has
**open as a workspace**, which in practice is the minority: the worktrees that
pile up are the ones nobody has open. So the routing is:

| worktree | route |
|---|---|
| open in a herdr workspace | `worktree.remove {workspace_id}` |
| not open | `git worktree remove <path>` |

Verified live, on a clean worktree open as workspace `w18`:

```json
{"result":{"type":"worktree_removed","forced":false,
           "path":"…/wt-live","workspace_id":"w18"}}
```

and afterwards: the checkout directory was gone, git's admin entry was pruned
(`git worktree list` no longer showed it), the **workspace was closed** (`w18`
vanished from `session.snapshot`), and the branch `live` still resolved to its
commit. That last point is the entire safety story: removal takes the checkout,
never the work.

Refusals, both verified live:

| situation | envelope |
|---|---|
| uncommitted changes, `force:false` | `{"code":"dirty_worktree_requires_force","message":"fatal: '…' contains modified or untracked files, use --force to delete it"}` |
| locked worktree | `{"code":"worktree_remove_failed","message":"fatal: cannot remove a locked working tree, lock reason: …"}` |

Note the asymmetry: dirty gets its own code, locked does not. Anything matching
on `worktree_remove_failed` alone will lump a locked worktree in with a genuine
failure, so the message text has to be shown to the user verbatim.

Nothing was removed in either refusal case — the directory was still there
afterwards.

## `workspace.close` takes `{workspace_id}`

Used when the user has agreed to close a workspace that holds a selected
worktree open. It closes the workspace and leaves the checkout alone.

## `session.snapshot`

One call; the arrays live under `result.snapshot`, **not** under `result`.
Reading them one level too high yields no workspaces at all, which is
indistinguishable from an idle session — so an absent `snapshot` key is a hard
error in `src/herdr.rs`, not a fallback.

`workspace.worktree` (absent entirely for non-git workspaces) carries
`repo_key`, `repo_name`, `repo_root`, `checkout_path`, `is_linked_worktree`.
Every workspace also carries `agent_status` (`idle`, `working`, `blocked`,
`done`, `unknown`) — herdr's own per-workspace aggregation, which ranks
`working` above `blocked`.

Three traps, the first two present in `tests/capture/session-snapshot.json`:

- **A git repository can arrive with no `worktree` key.** Observed for a
  workspace whose repository had an unborn HEAD (no commits yet). Such a repo is
  invisible to the session scan and has to be reached with `--repo`. Treating a
  missing `worktree` as "not a repo" is right; treating it as "no such
  repository exists" would be wrong.
- **`checkout_path` is echoed back as the user typed it.** A workspace created
  with `--cwd .` arrives as `/home/you/repos/app/.`, which does not string-match
  the absolute path `git worktree list` prints. `herdr::tidy_path` strips `.`
  components before any join.
- **A workspace with no `worktree` key can still hold a checkout open.**
  Verified live against 0.8.2: `worktree.list` reported a checkout's
  `open_workspace_id` while the snapshot carried that workspace with
  `worktree: null`. A path join finds nothing then; the label and agent status
  have to be joined by workspace id, which is why `session_view` summarizes
  *every* workspace, repo or not.

## Plugin execution environment

Commands are argv arrays run with **no shell**, cwd = plugin root, and a minimal
`PATH` — `git` must be resolved explicitly rather than assumed. Plugins run on
the **server** host.

`herdr plugin link .` does **not** run `[[build]]`; `herdr plugin install` does.

`plugin action invoke` resolves its context from the **focused workspace** and
has no workspace selector. shear does not need one: it scans the whole session.

## Gaps

- Whether `worktree.remove` with `force:true` also overrides a *lock* is
  untested. git needs `-f -f` for that, and shear never asks for it: a locked
  worktree is refused outright and the user is told to unlock it themselves.
- `worktree.list` on a repository with an unborn HEAD is untested; shear reaches
  those repos through git only.
