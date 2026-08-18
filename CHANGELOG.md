# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tag-triggered release automation. Pushing `vX.Y.Z` runs the full suite on
  Linux and macOS and publishes the GitHub release with notes taken from that
  version's changelog section — but only after an identity gate has confirmed
  that the tag, `Cargo.toml`, `Cargo.lock` and `herdr-plugin.toml` all name the
  same version and that the changelog section for it exists and is not empty.
  The manifest version is the one the marketplace displays and the one easiest
  to forget, so it is checked explicitly.
- An advisory upstream canary. Once a day it resolves one exact herdr `master`
  commit, fetches the API schema herdr generates from its own types at that
  revision, and checks that the five methods shear calls, the parameters it
  sends, and the fields it reads back are all still there — including the ones
  a removal returns, which are how shear reports what it destroyed. It is
  scheduled and manual only, it is not a required check, and a red canary is a
  signal to read herdr's recent changes rather than a reason to hold a pull
  request.
- `--restore <id>`, which puts a removed checkout back instead of printing a
  command for someone to paste. The ids are the `#N` numbers `--undo-log` now
  prints, and they are each record's line number in an append-only log, so an
  id never comes to mean a different removal. The recorded restore command is
  not executed: the argv is rebuilt from the record's own fields, and the branch
  form of `git worktree add` is used only when that branch still points at the
  commit the checkout was removed at. Otherwise the checkout comes back
  detached at exactly that commit, because creating or moving a branch to make
  a checkout reappear would put work at a commit nobody chose — worse than a
  refusal. A checkout that was removed through herdr comes back without its
  workspace, and the restore says so: herdr 0.8.0 has no call that opens a
  workspace at a path, and implying the session is as it was would be a quiet
  fallback.
- The review pane now says *why* the row under the cursor got its verdict, one
  line per signal, each quoting the value it was computed from: the counts of
  what is uncommitted, git's own lock and prunable reasons verbatim, the
  upstream ref that has gone, the integration ref a branch is contained in, and
  how old the tip is. A row that is not `safe` ends with the first condition
  that failed, because `safe` is the verdict that requires every question to
  have been answered positively and the useful sentence is which one was not.
  A worktree whose merge question could not be asked says exactly that, and
  never that it is not merged. The block is bounded: it keeps whole signals and
  says how many it could not show rather than truncating one mid-sentence,
  since half an explanation reads as a different explanation.

## [0.1.0] - 2026-08-16

First release.

### Added

- Inventory of every git worktree the herdr session knows about, grouped by
  repository, with a verdict per worktree: `safe`, `review`, `keep` or
  `blocked`.
- Offline classification from local git state only — no network calls at all:
  gone upstream, merged into the integration ref, prunable, stale, dirty,
  locked, and open in a herdr workspace.
- Lazy per-row disk sizing, reported both per worktree and as a reclaimable
  total.
- An interactive review pane (`--review`) with per-item selection, a bulk
  select limited to `safe` rows, and a second, differently-worded confirmation
  for anything with uncommitted changes.
- `--list` and `--json` for a dry-run report and a machine-readable one.
- `--remove <PATH>` for non-interactive removal, with the same guards.
- An undo log recording the path, branch, commit and timestamp of every
  removal, and the command that restores the checkout.

### Notes

- Removing a worktree never removes a branch or a commit. Only the checkout
  goes, and the undo log records how to get it back.
- The main checkout is never a removal candidate, and there is no override.
