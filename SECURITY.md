# Security policy

## Reporting a vulnerability

Please report security issues privately, through GitHub's
[private vulnerability reporting](https://github.com/moneycaringcoder/herdr-shear/security/advisories/new)
rather than as a public issue.

You can expect an acknowledgement within a few days. Since this is a
single-maintainer project, please don't read silence as dismissal — follow up if
you have heard nothing after a week.

If you would rather not use GitHub's reporting flow, open a public issue saying
only that you have found a security problem and would like a private channel,
with no details, and one will be arranged.

## What counts as a security issue here

shear deletes things. That is unusual for a plugin, and it sets the bar for what
counts as urgent.

- **Anything that removes a worktree the user did not select.** Every removal
  goes through an explicit selection. A path that widens a selection, that acts
  on a stale inventory, or that resolves a user-supplied path to the wrong
  candidate is a serious bug.
- **Anything that destroys committed work.** Removing a checkout must never
  remove a branch, a ref, or a commit. If a change can make `git rev-parse
  <branch>` stop resolving after a removal, that is the most serious bug this
  project can have.
- **Any guard that can be bypassed.** The main checkout, a locked worktree, a
  worktree open in another session, and a dirty worktree each have a refusal
  with a test that proves it. A way past one of them without the matching
  explicit permission is in scope.
- **Any write to a repository during a scan.** Everything outside `src/remove.rs`
  is read-only, proven by `tests/read_only.rs`. A scan that mutates an index,
  working tree, ref, or object store is a bug even if it looks harmless, because
  scans run against in-flight agent work.
- **Leaking repository contents** — file contents, branch names, or paths —
  anywhere they should not go. shear makes no network calls at all, so any
  outbound traffic is a bug by definition.
- **Command injection through a branch name, path, or config value.** Git
  invocations pass arguments as argv arrays rather than through a shell, and no
  path is ever interpolated into a shell string, so this should not be
  reachable — a way around that is worth reporting.

## What is out of scope

- A worktree classified `review` that you consider `safe`, or the other way
  round. Classification is deliberately conservative; disagreements are ordinary
  issues.
- Wrong disk numbers, wrong ages, misaligned columns. Ordinary bugs.
- shear executing a config file you wrote yourself.
- Issues in herdr itself, or in git. Those belong upstream, though a report here
  is welcome if shear could work around one.

## Supported versions

The most recent release is supported. Given the size of the project, fixes are
made on `main` and released rather than backported.
