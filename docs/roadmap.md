# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Two decisions are settled and nothing here reopens them: **shear never deletes
branches, only checkouts**, and **there is no bulk mode that removes without a
selection**. Both are what make the tool safe to point at a live session, and an
idea that needs either to change belongs in a different tool with a different
name.

## Finishing what the undo log started

### `--restore <id>`

The undo log already records every worktree shear has removed *and the command
that restores it*. Making the user copy that command out of a log and paste it
back is one step short of the feature. Running it directly closes the loop, and
turns "the worst case is `git worktree add`" from a claim in the README into a
key you can press.

## Explaining itself

### Explain a verdict

A row says `review`. It does not say which of the several possible reasons put it
there, and the classification is the whole product. Showing why a particular
worktree is `review` rather than `safe` — the specific signal, not the category —
is what lets someone disagree with the tool rather than obey it.

### Branch fate

Whether a branch is merged, gone from the remote, or has an open pull request is
often the deciding fact, and it is the one thing the current offline
classification cannot know.

This has to stay opt-in and clearly labelled, because it is the one feature that
would make a deliberately network-free tool make network calls. Offline must
remain the default and must remain fully useful.

## Policy

### Staleness rules

"Merged and untouched for thirty days" is a policy many people already apply by
eye. Letting it be written down turns a judgement made repeatedly into one made
once. It changes what is *offered*, never what is removed without a selection.

### A protect list

Some checkouts should never appear in the review pane at all — a long-lived
release branch, a worktree someone else owns. A pattern-based protect list is
simpler and safer than expecting the user to skip the same row every time.

## Scale

### Cache disk measurements

The pane draws before the walk finishes, which is correct, but reopening the
review pane re-walks trees that have not changed. Caching by mtime would make the
second open immediate without ever showing a stale figure as though it were
fresh — a cached size that might be wrong must render like a pending one, not
like a measured one.

### A multi-repository summary

Grouped by repository, with per-repository totals, for the case that actually
motivates running a janitor: not one worktree, but forty across six checkouts.

## Interfaces

### A JSON report mode

`--json` exists for the inventory. A report shaped for CI — what is stale, per
repository, with no removal path reachable — would let a team see the drift
accumulating without anyone opening a pane.

### Prefer herdr's `worktree.remove`

Some removal paths shell out to git directly. Where herdr's own API covers the
operation, using it keeps the session's view of its worktrees consistent with what
was actually removed, rather than relying on the session noticing afterwards.

## Platforms

### Windows

The manifest declares Linux and macOS. Windows would need the terminal discipline
reworked — raw mode is entered once and restored from `Drop`, from a panic hook,
and from SIGINT/SIGTERM, and that guarantee is what keeps a janitor from leaving
somebody's pane unusable. It is worth doing only with the same guarantee intact.
