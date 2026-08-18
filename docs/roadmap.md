# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Two decisions are settled and nothing here reopens them: **shear never deletes
branches, only checkouts**, and **there is no bulk mode that removes without a
selection**. Both are what make the tool safe to point at a live session, and an
idea that needs either to change belongs in a different tool with a different
name.

## Explaining itself

### Branch fate

Whether a branch is merged, gone from the remote, or has an open pull request is
often the deciding fact, and it is the one thing the current offline
classification cannot know.

This has to stay opt-in and clearly labelled, because it is the one feature that
would make a deliberately network-free tool make network calls. Offline must
remain the default and must remain fully useful.

Deferred for 0.1.1: herdr 0.8.0 proxies no such call, so every implementation
available today is either a new HTTP-and-TLS dependency or a shell-out standing
in for an API shear does not have. If it is taken up, the shape is already
decided: opt-in flag only, never reachable from a default `--list`, `--json` or
`--report` run; an unanswerable question is never a negative answer; no new
dependency; and a network answer may only ever *demote* a verdict, because
`safe` must keep requiring positive local evidence.

## Scale

### Cache disk measurements

The pane draws before the walk finishes, which is correct, but reopening the
review pane re-walks trees that have not changed. Caching by mtime would make the
second open immediate without ever showing a stale figure as though it were
fresh — a cached size that might be wrong must render like a pending one, not
like a measured one.

Attempted for 0.1.1 and stopped, on evidence. Validating a cached size by mtime
does not work: an in-place write took a fixture from 12 blocks to 5128 while
every directory mtime stayed byte-identical, because a directory's mtime records
changes to its entries and not to the contents of a file already in it. The
measurement *is* the stat walk that would have to prove freshness, so a cache
that validates itself saves nothing, and nothing cheaper is sound — recursive
directory mtimes miss in-place writes for the same reason, filesystem generation
counters are absent on ext4 and APFS, and `fanotify`/FSEvents need a process
alive between two pane opens.

What is left is worth doing but is a smaller, different thing, and should be
written down as such rather than under this heading: draw the previous figure on
the first frame, marked provisional and counted in no total, while the walk runs
anyway. Avoiding the re-walk needs a soundness proof no portable filesystem
interface can give.

## Interfaces

### Prefer herdr's `worktree.remove`

Some removal paths shell out to git directly. Where herdr's own API covers the
operation, using it keeps the session's view of its worktrees consistent with what
was actually removed, rather than relying on the session noticing afterwards.

Already true wherever the API can express it, as of 0.1.1: every worktree herdr
holds open goes through the socket, and a removal that cannot take that route is
refused rather than quietly falling back to git. The remainder is upstream: herdr
0.8.0's `worktree.remove` is keyed by `workspace_id`, not by path, so a worktree
nobody has open has no herdr call to make — and those are exactly the ones that
pile up. It needs a path-keyed removal upstream. Opening a workspace on a
worktree in order to remove it is not an acceptable substitute: that is a side
effect on a live session, taken during what the user asked to be the removal of
one checkout.

## Platforms

### Windows

The manifest declares Linux and macOS. Windows would need the terminal discipline
reworked — raw mode is entered once and restored from `Drop`, from a panic hook,
and from SIGINT/SIGTERM, and that guarantee is what keeps a janitor from leaving
somebody's pane unusable. It is worth doing only with the same guarantee intact.

Two further surfaces change meaning at the same time: disk sizing measures
apparent size off unix, so the disk column would stop being the number a user
checks against `du`, and path handling is byte-exact only on unix. It is its own
piece of work, not an item in a patch release.
