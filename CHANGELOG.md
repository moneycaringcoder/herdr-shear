# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
