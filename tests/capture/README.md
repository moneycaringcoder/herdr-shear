# Captured output

Every file here is **real output**, captured before the code that parses it was
written. Build fakes from these, never from what the code expects to see. A fake that
replies in the shape the parser wants will pass a whole suite while the parser
is wrong.

| file | command |
|---|---|
| `worktree-list.z` | `git worktree list --porcelain -z` on a fixture repo carrying every class |
| `for-each-ref.txt` | `git for-each-ref --format='%(refname:short)\t%(objectname)\t%(upstream)\t%(upstream:track)\t%(committerdate:unix)' refs/heads/` |
| `for-each-ref-merged.txt` | `git for-each-ref --merged=main --format='%(refname:short)' refs/heads/` |
| `status-v2.z` | `git --no-optional-locks status --porcelain=v2 -z --untracked-files=all --renames` in a worktree with a staged rename, a staged add, and untracked files named with a space, a quote and a **literal newline** |
| `session-snapshot.json` | `herdr api snapshot` against a live 0.8.0 server, paths and titles redacted, structure untouched |
| `worktree-list.json` | the `worktree.list` reply for a fixture repo, with one worktree open in a workspace |

Captured on git 2.53.0 and herdr 0.8.0 / protocol 19.

Things in these files that a hand-written fake would have got wrong:

- `worktree-list.z` separates records with an **empty NUL field**, and a
  `locked` line with no reason is the bare word `locked` with nothing after it.
- The **broken-head** worktree prints `HEAD 0000…0000` *and* a
  `branch refs/heads/broken-branch` line, even though that ref no longer exists.
  The branch line is not the discriminator; the worktree's own `logs/HEAD` is.
- `status-v2.z` contains a `2` (rename) record, which consumes **two**
  NUL-terminated fields — the new path, then the original path.
- One untracked path in `status-v2.z` contains a real newline byte, so anything
  that splits the output on lines rather than NUL will parse it as two entries.
- `session-snapshot.json` contains a workspace whose `checkout_path` ends in
  `/.`, because herdr echoes back whatever path the workspace was created with.
  Joining that against git's absolute paths without tidying it finds nothing.
- `session-snapshot.json` also contains git repositories that arrive with **no
  `worktree` key at all** — a workspace can be a repository and still be
  reported without one.
