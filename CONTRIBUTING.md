# Contributing to shear

Contributions are genuinely welcome — bug reports, questions, documentation
fixes, and code. This document exists so you know what to expect before you
spend time on something, not to put obstacles in front of you.

The project is maintained by one person. Review is attentive but not instant,
and every change is read carefully before it lands. Please don't take questions
on a pull request as resistance; they are how the maintainer stays confident in
code that deletes things on other people's machines.

## The two rules that matter

**1. A scan never writes.** Everything outside `src/remove.rs` is read-only
against a user's repository. Scans run on machines full of in-flight,
uncommitted, unpushed agent work.

So any change touching `src/git.rs` must keep these true:

- every `status` invocation passes `--no-optional-locks`, or git takes
  `index.lock` to write back its stat cache
- nothing is staged, nothing is written to the object store, no ref moves
- every invocation is bounded by a timeout and runs with an explicitly resolved
  `git`, because herdr runs plugin commands with a minimal `PATH`

`tests/read_only.rs` enforces this by fingerprinting index bytes and mtimes, the
working tree, refs, reflogs and the object store before and after a full scan,
including while another process holds `index.lock`. If your change makes that
test fail, the test is right and the change is wrong.

**2. Removal is only ever what the user picked.** `src/remove.rs` is the only
module that may change anything, and every path into it starts from an explicit
selection. Each guard — main checkout, locked, open in herdr, dirty — has a test
that proves it *refuses*. Adding a capability that can widen a selection, or
that acts on an inventory taken before the user looked at it, is the kind of
change that will be sent back.

Removing a worktree must leave its branch and every commit on it untouched. That
claim is pinned by a test that removes a worktree and then asserts the branch
still resolves. Do not weaken it.

## Getting set up

```sh
git clone https://github.com/moneycaringcoder/herdr-shear
cd herdr-shear
cargo build --release
herdr plugin link .          # note: `link` does NOT run the build step
```

Rebuild by hand after every change, since `herdr plugin link` deliberately skips
the `[[build]]` hook.

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --all
```

CI runs exactly these on Linux and macOS with the current stable toolchain. If
your local Rust is older than CI's, clippy will pass locally and fail there —
`rustup update stable` first if in doubt.

No test requires a running herdr. The fixtures build throwaway git repositories
in a temp directory and clean up after themselves, and the socket tests stand up
their own Unix socket server.

## What makes a change easy to merge

**A test that fails before your fix and passes after it.** This matters more
here than in most projects, because the bugs a tool like this attracts are
*invisible* ones: a wrong answer with no error, which looks exactly like a right
one. A worktree misclassified as safe is not a cosmetic defect.

**Tests built from observed behaviour, not assumed behaviour.** `tests/capture/`
holds real output — from real git, from a real herdr server — captured before
the code that parses it was written. A fake that replies in the shape the parser
wants will pass an entire suite while the parser is wrong. If you add a parser,
add a capture with it and say in the commit message how you produced it.

**A loud failure over a quiet fallback.** "No worktrees found" and "I could not
read the response" must never look the same on screen. Where a question cannot
be answered — no integration ref resolves, a status cannot be read — say so.
Never render an unanswerable question as a negative answer, and never let a
failed measurement render as a plausible zero.

**Small, focused pull requests.** One behaviour change per pull request, with
the reasoning in the commit message rather than in a comment on the diff.

## What the project will probably say no to

- **Deleting branches.** shear removes checkouts. The commits are the safety
  net that makes a removal feel reversible, and deleting the branch removes it.
- **Network calls of any kind.** Classification is entirely local git state, on
  purpose: it works offline, on a plane, behind a proxy, with no token.
- **An auto-prune mode with no confirmation.** The product is confidence about
  what is safe to delete. A mode that deletes without asking is a different,
  worse product.
- **New dependencies**, unless there is no reasonable alternative. The crate
  currently has two, plus `libc` and `signal-hook` on unix.

## Style

Match the code around you. Comments explain *why*, especially where the code
looks odd because of something git or herdr actually does; the interesting ones
in this codebase all cite something that was verified rather than assumed.

Commit messages are plain prose in the imperative, wrapped at 72 columns, and
say what changed and why.

## Code of conduct

By participating you agree to abide by the [Code of Conduct](CODE_OF_CONDUCT.md).
