# git plumbing notes for shear (verified on git 2.53.0, Linux)

Working notes for `src/git.rs` and `src/remove.rs`. Every command here was run
against a purpose-built fixture repository containing one worktree of every
class shear recognises, plus the degenerate states. The raw output is committed
under `tests/capture/`.

## Hard rules

1. Always pass `--no-optional-locks` to `status`. Plain `status` takes
   `<gitdir>/index.lock` to write back its stat cache.
2. Never touch a worktree's real index, and never write an object. shear has no
   reason to stage anything, so unlike collide it needs no temp index and no
   `GIT_OBJECT_DIRECTORY` redirection — but `tests/read_only.rs` fingerprints the
   object store anyway, because "we never write objects" is a claim, and claims
   rot.
3. Resolve `git` explicitly. herdr runs plugin commands with a minimal `PATH`.
4. Bound every invocation with a timeout.
5. Scrub the environment. `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` are left
   alone in production — a user's `init.defaultBranch` is a fact about their
   repo, not interference — but the fixtures pin them to `/dev/null`, or a green
   suite on a developer's machine says nothing about CI.

## Repo identity

```sh
git -C <path> rev-parse --path-format=absolute --git-common-dir
```

All worktrees of one repo share it; each has its own `--git-dir`. Canonicalize
before comparing. herdr reports the same value as `repo_key`.

## Enumerating worktrees

```sh
git -C <repo> worktree list --porcelain -z
```

Records are separated by an **empty NUL field**. `worktree <abs-path>` is always
first; after that assume no ordering. The first record is the main checkout.

From the captured fixture (`tests/capture/worktree-list.z`), one record of each
interesting shape, NULs shown as `|`:

```
worktree /…/wt-locked|HEAD 1338d9a…|branch refs/heads/locked-branch|locked held for demo||
worktree /…/wt-locked2|HEAD 1338d9a…|branch refs/heads/locked2-branch|locked||
worktree /…/wt-prunable|HEAD 1338d9a…|branch refs/heads/goner-branch|prunable gitdir file points to non-existent location||
worktree /…/wt-detached|HEAD 1338d9a…|detached||
worktree /…/wt-broken|HEAD 0000000000000000000000000000000000000000|branch refs/heads/broken-branch||
```

Points that a hand-written parser gets wrong:

- `locked` with no reason is the **bare word**, with nothing after it. Splitting
  on the first space and taking the remainder yields an empty-string reason that
  is not the same as "no reason given".
- The **broken-head** record still carries `branch refs/heads/broken-branch`
  even though that ref was deleted. The branch line is not the discriminator.
  What distinguishes an unborn worktree from one whose branch was deleted
  underneath it is the worktree's own HEAD reflog: a worktree that ever had a
  commit checked out has `logs/HEAD`, a freshly initialised one does not.
  (`symbolic-ref -q HEAD` does **not** distinguish them: it exits 0 and prints
  the same ref name in both cases.)
- A `bare` record has no `HEAD` and no `branch`.

## Upstream and staleness, in one call

```sh
git -C <repo> for-each-ref \
  --format='%(refname:short)%09%(objectname)%09%(upstream)%09%(upstream:track)%09%(committerdate:unix)' \
  refs/heads/
```

Captured output (`tests/capture/for-each-ref.txt`), tabs shown as `→`:

```
active-branch  → 7a221ac… → refs/remotes/origin/active-branch →           → 1786830847
main           → 1338d9a… → refs/remotes/origin/main          → [ahead 2] → 1786830847
safe-branch    → 98c9ee0… → refs/remotes/origin/safe-branch   → [gone]    → 1786830847
stale-branch   → 301ef44… →                                   →           → 1704067200
```

`%(upstream:track)` reports the literal `[gone]` for a branch tracking a ref
that no longer exists. That string is the detection. Do **not** try to infer it
from a missing remote ref: a remote that has simply never been fetched looks
identical.

A branch with no upstream configured at all yields two empty fields, which is a
third state — "never pushed" — and not the same as `[gone]`. Only `[gone]` is
evidence that work has landed somewhere.

## Merged detection

```sh
git -C <repo> for-each-ref --merged=<integration-ref> --format='%(refname:short)' refs/heads/
```

For a detached HEAD there is no branch to look up, so the question becomes
`git merge-base --is-ancestor <oid> <integration-ref>` (exit 0 = contained).

The integration ref itself is resolved in this order:

1. the user's `--integration-ref` / config value,
2. `origin/HEAD`, which is the only authoritative answer,
3. `origin/main`, `origin/master`, `main`, `master`.

`origin/HEAD` is frequently **not set** — it was unset in a freshly cloned
fixture and had to be created by hand — so the guesses matter in practice.

When none of them resolves, the merged question **cannot be asked**. That is
reported as unknown, never as "not merged", and it means nothing in that
repository can be classified safe. Rendering an unanswerable question as a
negative answer is precisely the silent degradation this plugin exists to avoid.

## Dirty detection

```sh
git -C <wt> --no-optional-locks status --porcelain=v2 -z --untracked-files=all --renames
```

`-z` disables path quoting, so paths are **raw bytes**. Record grammar, split on
NUL then on ASCII space with a bounded `splitn`:

| kind | layout | fields before path |
|---|---|---|
| header | `# <key> <value…>` | — |
| ordinary | `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` | 8 |
| rename/copy | `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>` NUL `<origPath>` | 9 |
| unmerged | `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` | 10 |
| untracked | `? <path>` | 1 |
| ignored | `! <path>` | 1 |

**The framing rule naive parsers get wrong:** a `2` record consumes **two**
NUL-terminated fields — the new path, then the original path as the very next
field. `tests/capture/status-v2.z` contains one, plus an untracked file whose
name holds a literal newline, so anything splitting on lines parses it as two
entries and reports a file that does not exist.

`X` is the index status, `Y` the worktree status. shear counts them separately:
`X` non-`.` is staged, `Y` non-`.` is unstaged, `u` is unmerged, `?` is
untracked. The second confirmation for a dirty removal names these numbers, and
"3 untracked files" and "3 unmerged paths" are very different sentences.

Ignored files (`!`) are **not** counted as dirt — `--untracked-files=all` does
not list them unless `--ignored` is also given, and a `target/` directory is not
work at risk.

## Removal, and what git refuses on its own

Verified, exit codes and all:

| command | result |
|---|---|
| `git worktree remove <dirty>` | `fatal: '…' contains modified or untracked files, use --force to delete it`, exit 128, nothing removed |
| `git worktree remove <locked>` | `fatal: cannot remove a locked working tree, lock reason: …` / `use 'remove -f -f' to override or unlock first`, exit 128, nothing removed |
| `git worktree remove <prunable>` | **exit 0** — it works even though the directory is already gone |
| `git worktree remove <clean>` | exit 0; the directory goes, the branch and its commits remain |

The third row is the useful one: there is no need for `git worktree prune`,
which would be wrong anyway because it prunes *every* prunable worktree in the
repo rather than the one the user selected.

The fourth row is the safety story, and it is worth stating in the interface:
after removing the worktree for `merged-gone`, `git rev-parse merged-gone` still
printed the commit. The checkout goes; the work does not.

## Disk sizing

Measured as `st_blocks * 512` per file, hardlinks counted once per `(dev, ino)`,
symlinks never followed. That is what `du` reports, and `du` is the command a
user will check the number against. Apparent size (`st_size`) would overstate a
sparse file and understate the block padding across thousands of small source
files.

A prunable worktree's directory does not exist, so it reclaims nothing. Saying
`0 B` and saying "gone" are different claims and the table distinguishes them.

## Degenerate cases

| case | detection | shear's behaviour |
|---|---|---|
| detached HEAD | `worktree list` → `detached` | classified on the commit, never on a branch |
| unborn branch | `HEAD 0000…` and **no** `logs/HEAD` | never a candidate; the row says why |
| branch deleted underneath | `HEAD 0000…` and a non-empty `logs/HEAD` | reported as broken, offered only under `review` |
| bare | `bare` record | skipped entirely |
| foreign repo | differing `--git-common-dir` | never grouped with another repo's worktrees |
| main checkout | first record of the repo | never a candidate, no override |

## Unverified

Windows; case-insensitive and NFD filesystems; submodules; worktrees relocated
across mounts; `core.fsmonitor` interaction with `--no-optional-locks`.
