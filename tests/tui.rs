//! The review pane, driven entirely without a terminal.
//!
//! [`shear::tui::apply`] is a total function from state and key to state, which
//! is the whole reason a destructive interface can be tested at all: every
//! assertion below is a session someone could sit through, including both
//! confirmations for a dirty removal and the one that ends in `q`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use shear::model::{
    Candidate, Class, Dirt, Head, Inventory, LockInfo, Merged, OpenWorkspace, Repo, RepoKey, Size,
    Upstream, Verdict, Worktree,
};
use shear::tui::{apply, decode, for_raw_terminal, frame, Key, Mode, Review};

// ---------------------------------------------------------------------------
// Hand-built inventory
// ---------------------------------------------------------------------------

const REPO: &str = "/home/dev/repos/app";

struct Row(Candidate);

impl Row {
    fn new(path: &str, branch: &str) -> Self {
        Self(Candidate {
            worktree: Worktree {
                repo: RepoKey(format!("{REPO}/.git")),
                repo_root: PathBuf::from(REPO),
                path: PathBuf::from(path),
                head: Head::Branch(branch.to_string()),
                head_oid: Some("c0ffee".repeat(6) + "abcd"),
                is_main: false,
                locked: None,
                prunable: None,
                notes: Vec::new(),
            },
            dirt: Dirt::default(),
            upstream: Upstream::default(),
            merged: Merged::No("origin/main".into()),
            last_commit: Some(SystemTime::now() - Duration::from_secs(21 * 86_400 + 43_200)),
            open_workspace: None,
            occupants: Vec::new(),
            protected: None,
            classes: BTreeSet::new(),
            verdict: Verdict::Keep,
            size: Size::Pending,
            reason: String::new(),
        })
    }

    fn verdict(mut self, verdict: Verdict) -> Self {
        self.0.verdict = verdict;
        self
    }

    fn classes(mut self, classes: &[Class]) -> Self {
        self.0.classes = classes.iter().copied().collect();
        self
    }

    fn merged(mut self, merged: Merged) -> Self {
        self.0.merged = merged;
        self
    }

    fn upstream_gone(mut self, name: &str) -> Self {
        self.0.upstream = Upstream {
            name: Some(name.to_string()),
            gone: true,
            ahead: 0,
            behind: 0,
        };
        self
    }

    fn size(mut self, size: Size) -> Self {
        self.0.size = size;
        self
    }

    fn dirt(mut self, staged: usize, unstaged: usize, untracked: usize) -> Self {
        self.0.dirt = Dirt {
            staged,
            unstaged,
            untracked,
            unmerged: 0,
        };
        self
    }

    fn main_checkout(mut self) -> Self {
        self.0.worktree.is_main = true;
        self
    }

    fn locked(mut self) -> Self {
        self.0.worktree.locked = Some(LockInfo {
            reason: Some("benchmark rig".into()),
        });
        self
    }

    fn open(mut self) -> Self {
        self.0.open_workspace = Some(OpenWorkspace {
            workspace_id: "ws-1".into(),
            label: "ui-review".into(),
            agent_status: None,
        });
        self
    }

    fn reason(mut self, reason: &str) -> Self {
        self.0.reason = reason.to_string();
        self
    }

    fn build(self) -> Candidate {
        self.0
    }
}

/// Seven rows: two genuinely safe, three that a bulk key must never touch
/// however they are labelled, one dirty review row, and one ordinary keep.
///
/// Rows 3 and 4 are deliberately impossible — a `Safe` verdict on a worktree
/// that is dirty, and another that is locked. They exist so the `a` key is
/// tested against a classifier that has gone wrong, not only against one that
/// has not.
fn inventory() -> Inventory {
    Inventory {
        repos: vec![Repo {
            key: RepoKey(format!("{REPO}/.git")),
            root: PathBuf::from(REPO),
            name: "app".into(),
        }],
        candidates: vec![
            Row::new(REPO, "main")
                .main_checkout()
                .verdict(Verdict::Blocked)
                .merged(Merged::Into("origin/main".into()))
                .size(Size::Bytes(126_000_000))
                .reason("main checkout: never removable, and there is no override")
                .build(),
            Row::new("/home/dev/repos/app-wt/feature-login", "feature/login")
                .verdict(Verdict::Safe)
                .classes(&[Class::Merged, Class::GoneUpstream])
                .merged(Merged::Into("origin/main".into()))
                .upstream_gone("origin/feature-login")
                .size(Size::Bytes(1_310_000_000))
                .reason("merged into origin/main, upstream gone, clean")
                .build(),
            Row::new("/home/dev/repos/app-wt/chore-deps", "chore/deps")
                .verdict(Verdict::Safe)
                .classes(&[Class::Merged, Class::GoneUpstream])
                .merged(Merged::Into("origin/main".into()))
                .upstream_gone("origin/chore-deps")
                .size(Size::Bytes(419_430_400))
                .reason("merged into origin/main, upstream gone, clean")
                .build(),
            Row::new("/home/dev/repos/app-wt/mislabelled-dirty", "spike/dirty")
                .verdict(Verdict::Safe)
                .classes(&[Class::Dirty])
                .dirt(0, 3, 0)
                .size(Size::Bytes(10_000_000))
                .reason("3 uncommitted files, and something has mislabelled this safe")
                .build(),
            Row::new("/home/dev/repos/app-wt/mislabelled-locked", "spike/locked")
                .verdict(Verdict::Safe)
                .classes(&[Class::Locked])
                .locked()
                .size(Size::Bytes(10_000_000))
                .reason("locked, and something has mislabelled this safe")
                .build(),
            Row::new("/home/dev/repos/app-wt/hotfix-payments", "hotfix/payments")
                .verdict(Verdict::Review)
                .classes(&[Class::Dirty, Class::Stale])
                .dirt(2, 4, 6)
                .size(Size::Bytes(50_400_000))
                .reason("12 uncommitted files; last commit 60 days ago")
                .build(),
            Row::new("/home/dev/repos/app-wt/review-ui", "review/ui")
                .verdict(Verdict::Keep)
                .classes(&[Class::OpenInHerdr])
                .open()
                .size(Size::Pending)
                .reason("open in herdr workspace ui-review")
                .build(),
        ],
        notes: Vec::new(),
    }
}

const MAIN: usize = 0;
const SAFE_A: usize = 1;
const SAFE_B: usize = 2;
const DIRTY_REVIEW: usize = 5;

fn review() -> Review {
    Review::new(inventory())
}

fn drive(mut review: Review, keys: &[Key]) -> Review {
    for key in keys {
        review = apply(review, *key);
    }
    review
}

fn typed(number: &str) -> Vec<Key> {
    number
        .bytes()
        .map(|byte| Key::Digit(byte - b'0'))
        .collect::<Vec<_>>()
}

fn selection(review: &Review) -> BTreeSet<usize> {
    review.selected.clone()
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[test]
fn the_bulk_key_selects_the_safe_rows_and_nothing_else() {
    let after = drive(review(), &[Key::SelectSafe]);
    assert_eq!(selection(&after), BTreeSet::from([SAFE_A, SAFE_B]));
}

#[test]
fn the_bulk_key_refuses_a_row_that_has_been_mislabelled_safe() {
    let after = drive(review(), &[Key::SelectSafe]);
    for index in [3usize, 4] {
        assert!(
            !after.selected.contains(&index),
            "row {index} carries a Safe verdict but is dirty or locked, and `a` must not \
             preselect it whatever the classifier said"
        );
    }
}

#[test]
fn the_bulk_key_replaces_the_selection_rather_than_adding_to_it() {
    // Hand-pick the dirty row first. `a` must not leave it selected: the whole
    // promise of the key is that what it leaves behind is safe.
    let mut state = review();
    state.cursor = DIRTY_REVIEW;
    let after = drive(state, &[Key::Toggle, Key::SelectSafe]);
    assert_eq!(selection(&after), BTreeSet::from([SAFE_A, SAFE_B]));
}

#[test]
fn the_main_checkout_can_never_be_selected() {
    let mut state = review();
    state.cursor = MAIN;
    let after = drive(state, &[Key::Toggle]);
    assert_eq!(after.cursor, MAIN);
    assert!(after.selected.is_empty());
    assert!(
        after.messages.iter().any(|m| m.contains("never removable")),
        "and the pane says why: {:?}",
        after.messages
    );
}

#[test]
fn a_blocked_row_can_never_be_selected() {
    // Any blocked row, not only the main checkout: the pane never grants the
    // permissions that could remove one, so a selection could only end in a
    // refusal at removal time.
    let mut state = review();
    state.inventory.candidates[DIRTY_REVIEW] =
        Row::new("/home/dev/repos/app-wt/hotfix-payments", "hotfix/payments")
            .verdict(Verdict::Blocked)
            .classes(&[Class::Locked])
            .locked()
            .reason("locked (benchmark rig): unlock it with `git worktree unlock`")
            .build();
    state.cursor = DIRTY_REVIEW;
    let after = drive(state, &[Key::Toggle]);
    assert!(after.selected.is_empty());
    assert!(
        after
            .messages
            .iter()
            .any(|m| m.contains("cannot be selected") && m.contains("unlock")),
        "the refusal names the unblocking action: {:?}",
        after.messages
    );
}

#[test]
fn space_toggles_and_n_clears() {
    let after = drive(review(), &[Key::Down, Key::Toggle]);
    assert_eq!(selection(&after), BTreeSet::from([SAFE_A]));
    let after = drive(after, &[Key::Toggle]);
    assert!(after.selected.is_empty());

    let after = drive(review(), &[Key::SelectSafe, Key::SelectNone]);
    assert!(after.selected.is_empty());
}

#[test]
fn the_cursor_moves_through_the_table_order_not_the_inventory_order() {
    // Rows are drawn safe-first, so the display order here is
    // [chore-deps, feature-login, mislabelled-dirty, mislabelled-locked,
    //  hotfix-payments, review-ui, main]. The cursor has to follow that, or `j`
    // jumps around the screen.
    let order = shear::tui::display_order(&inventory());
    assert_eq!(order, vec![SAFE_B, SAFE_A, 3, 4, DIRTY_REVIEW, 6, MAIN]);
    assert_eq!(
        review().cursor,
        SAFE_B,
        "the cursor starts on the first row"
    );

    let after = drive(review(), &[Key::Down]);
    assert_eq!(after.cursor, SAFE_A);
}

#[test]
fn the_cursor_never_leaves_the_table() {
    let after = drive(review(), &[Key::Up, Key::Up, Key::Up]);
    assert_eq!(after.cursor, SAFE_B, "the top row is the top");
    let rows = review().inventory.candidates.len();
    let after = drive(review(), &vec![Key::Down; rows + 5]);
    assert_eq!(after.cursor, MAIN, "and the last drawn row is the bottom");
}

#[test]
fn an_empty_inventory_has_nowhere_to_move_and_does_not_mind() {
    let after = drive(
        Review::new(Inventory::default()),
        &[
            Key::Down,
            Key::Up,
            Key::Toggle,
            Key::SelectSafe,
            Key::Remove,
        ],
    );
    assert_eq!(after.cursor, 0);
    assert!(after.selected.is_empty());
    assert_eq!(after.mode, Mode::Browsing);
}

// ---------------------------------------------------------------------------
// The clean confirmation
// ---------------------------------------------------------------------------

#[test]
fn a_clean_selection_is_confirmed_once_by_count_and_bytes() {
    let after = drive(review(), &[Key::SelectSafe, Key::Remove]);
    assert_eq!(
        after.mode,
        Mode::ConfirmClean {
            count: 2,
            bytes: 1_310_000_000 + 419_430_400,
        }
    );

    let rendered = frame(&after, 80, 24);
    assert!(
        rendered.contains("Remove 2 worktrees and reclaim 1.6 GB?"),
        "the question names both numbers:\n{rendered}"
    );

    let after = drive(after, &[Key::Confirm]);
    assert_eq!(
        after.mode,
        Mode::Removing,
        "a clean selection needs exactly one confirmation"
    );
}

#[test]
fn removing_nothing_asks_nothing() {
    let after = drive(review(), &[Key::Remove]);
    assert_eq!(after.mode, Mode::Browsing);
    assert!(after
        .messages
        .iter()
        .any(|m| m.contains("Nothing is selected")));
}

#[test]
fn cancelling_the_first_confirmation_removes_nothing() {
    let after = drive(review(), &[Key::SelectSafe, Key::Remove, Key::Cancel]);
    assert_eq!(after.mode, Mode::Browsing);
    assert_eq!(selection(&after), BTreeSet::from([SAFE_A, SAFE_B]));
    assert!(after
        .messages
        .iter()
        .any(|m| m.contains("Nothing was removed")));
}

// ---------------------------------------------------------------------------
// The dirty confirmation
// ---------------------------------------------------------------------------

/// Selects one safe row and the dirty review row.
fn with_a_dirty_row() -> Review {
    let mut review = review();
    review.selected = BTreeSet::from([SAFE_A, DIRTY_REVIEW]);
    review
}

#[test]
fn a_dirty_selection_gets_a_second_and_different_confirmation() {
    let after = drive(with_a_dirty_row(), &[Key::Remove]);
    assert!(matches!(after.mode, Mode::ConfirmClean { count: 2, .. }));

    let after = drive(after, &[Key::Confirm]);
    assert_eq!(
        after.mode,
        Mode::ConfirmDirty {
            files: 12,
            typed: String::new(),
            worktrees: 1,
        },
        "confirming the first question opens the second, it does not remove"
    );

    let rendered = frame(&after, 80, 24);
    let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flattened.contains("12 files that exist nowhere else"),
        "the second question names the exact count at risk:\n{rendered}"
    );
    assert!(
        flattened.contains("cannot be answered with `y`"),
        "and says so in different words from the first:\n{rendered}"
    );
    assert!(
        rendered.contains("files at risk: 12"),
        "and shows the number that has to be typed:\n{rendered}"
    );
}

#[test]
fn the_dirty_confirmation_cannot_be_answered_with_y() {
    // `y` and Enter both decode to Confirm, which is exactly the reflex this
    // confirmation exists to defeat.
    assert_eq!(decode(b"y"), vec![Key::Confirm]);
    let after = drive(
        with_a_dirty_row(),
        &[Key::Remove, Key::Confirm, Key::Confirm],
    );
    assert!(
        matches!(after.mode, Mode::ConfirmDirty { files: 12, .. }),
        "an empty answer is not an answer: {:?}",
        after.mode
    );
}

#[test]
fn the_dirty_confirmation_refuses_the_wrong_number() {
    let mut keys = vec![Key::Remove, Key::Confirm];
    keys.extend(typed("11"));
    keys.push(Key::Confirm);
    let after = drive(with_a_dirty_row(), &keys);

    assert_eq!(
        after.mode,
        Mode::ConfirmDirty {
            files: 12,
            typed: String::new(),
            worktrees: 1,
        },
        "a wrong number is refused and the field is cleared, not accepted"
    );
    assert!(
        after
            .messages
            .iter()
            .any(|m| m.contains("not the number of files at risk")),
        "{:?}",
        after.messages
    );
}

#[test]
fn the_dirty_confirmation_accepts_the_right_number() {
    let mut keys = vec![Key::Remove, Key::Confirm];
    keys.extend(typed("12"));
    keys.push(Key::Confirm);
    let after = drive(with_a_dirty_row(), &keys);
    assert_eq!(after.mode, Mode::Removing);
}

#[test]
fn a_typed_number_can_be_corrected() {
    let mut keys = vec![Key::Remove, Key::Confirm];
    keys.extend(typed("13"));
    keys.push(Key::Backspace);
    keys.extend(typed("2"));
    keys.push(Key::Confirm);
    assert_eq!(drive(with_a_dirty_row(), &keys).mode, Mode::Removing);
}

#[test]
fn cancelling_the_second_confirmation_removes_nothing() {
    let mut keys = vec![Key::Remove, Key::Confirm];
    keys.extend(typed("12"));
    keys.push(Key::Cancel);
    let after = drive(with_a_dirty_row(), &keys);
    assert_eq!(after.mode, Mode::Browsing);
    assert!(after
        .messages
        .iter()
        .any(|m| m.contains("Nothing was removed")));
}

// ---------------------------------------------------------------------------
// Quitting
// ---------------------------------------------------------------------------

#[test]
fn quitting_removes_nothing_however_far_the_session_got() {
    // A whole session: browse, select, open both confirmations, back out, and
    // quit. `Mode::Removing` is the only thing the driver acts on, so proving it
    // is never reached is proving nothing was removed.
    let mut review = review();
    let mut modes = Vec::new();
    let session = {
        // Browse, hand-pick, bulk-select, then add the dirty row so both
        // confirmations are opened before backing out of each.
        let mut keys = vec![
            Key::Down,
            Key::Toggle,
            Key::Up,
            Key::Toggle,
            Key::SelectSafe,
            Key::Down,
            Key::Down,
            Key::Down,
            Key::Down,
            Key::Toggle,
            Key::Remove,
            Key::Confirm,
        ];
        keys.extend(typed("12"));
        keys.extend([Key::Cancel, Key::Remove, Key::Cancel, Key::Quit]);
        keys
    };
    for key in session {
        review = apply(review, key);
        modes.push(review.mode.clone());
    }

    assert!(
        !modes.contains(&Mode::Removing),
        "the session never reached the one mode that removes anything: {modes:?}"
    );
    assert_eq!(review.mode, Mode::Done);
    assert!(review.is_finished());
    assert!(review
        .messages
        .iter()
        .any(|m| m.contains("Nothing was removed")));
}

#[test]
fn quitting_from_a_confirmation_goes_back_rather_than_out() {
    let after = drive(review(), &[Key::SelectSafe, Key::Remove, Key::Quit]);
    assert_eq!(after.mode, Mode::Browsing);
    assert!(!after.is_finished());
}

#[test]
fn apply_is_total() {
    // Every key, in every mode, including the ones a user cannot reach by
    // accident. Nothing here may panic and nothing may remove anything.
    let keys = [
        Key::Up,
        Key::Down,
        Key::Toggle,
        Key::SelectSafe,
        Key::SelectNone,
        Key::Remove,
        Key::Quit,
        Key::Confirm,
        Key::Cancel,
        Key::Digit(0),
        Key::Digit(9),
        Key::Backspace,
        Key::Other,
    ];
    for start in [
        Mode::Browsing,
        Mode::ConfirmClean {
            count: 2,
            bytes: 10,
        },
        Mode::ConfirmDirty {
            files: 12,
            typed: "1".into(),
            worktrees: 1,
        },
        Mode::Removing,
        Mode::Done,
    ] {
        for key in keys {
            let mut review = review();
            review.mode = start.clone();
            let _ = apply(review, key);
        }
        // And on an empty inventory, where there is no row to point at.
        for key in keys {
            let mut review = Review::new(Inventory::default());
            review.mode = start.clone();
            let after = apply(review, key);
            assert!(after.selected.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------

#[test]
fn the_frame_is_exactly_the_size_it_was_given() {
    let states = [
        review(),
        drive(review(), &[Key::SelectSafe]),
        drive(review(), &[Key::SelectSafe, Key::Remove]),
        drive(with_a_dirty_row(), &[Key::Remove, Key::Confirm]),
        Review::new(Inventory::default()),
    ];
    for state in &states {
        for columns in [40usize, 80, 100, 200] {
            for rows in [8usize, 12, 24, 50] {
                let rendered = frame(state, columns, rows);
                assert_eq!(
                    rendered.lines().count(),
                    rows,
                    "{columns}x{rows} frame:\n{rendered}"
                );
                for line in rendered.lines() {
                    assert!(
                        shear::render::display_width(line) <= columns,
                        "a line ran past {columns}: {line}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_sentence_that_makes_this_safe_is_on_screen_while_deciding() {
    for state in [
        review(),
        drive(review(), &[Key::SelectSafe, Key::Remove]),
        drive(with_a_dirty_row(), &[Key::Remove, Key::Confirm]),
    ] {
        let rendered = frame(&state, 80, 24);
        let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flattened.contains(
                "Removing a worktree leaves its branch and every commit on it intact: only the \
                 checkout goes."
            ),
            "the pane must say what survives a removal, where it can be read:\n{rendered}"
        );
    }
}

#[test]
fn the_cursor_row_is_always_in_view() {
    // Eight rows and a five-line pane: the window has to follow the cursor, or
    // the user is selecting something they cannot see.
    let mut state = review();
    for _ in 0..5 {
        state = apply(state, Key::Down);
    }
    let rendered = frame(&state, 80, 12);
    assert!(
        rendered.lines().any(|line| line.starts_with("> ")),
        "the cursor is inside the window:\n{rendered}"
    );
    assert!(
        rendered.contains("review-ui"),
        "and it is the row the cursor is on:\n{rendered}"
    );
}

#[test]
fn the_selection_is_visible_in_the_table_and_in_the_footer() {
    let state = drive(review(), &[Key::SelectSafe]);
    let rendered = frame(&state, 80, 24);
    assert!(
        rendered.lines().filter(|line| line.contains('*')).count() == 2,
        "both selected rows carry the marker:\n{rendered}"
    );
    assert!(
        rendered.contains("2 selected \u{b7} 1.6 GB"),
        "and the footer totals them:\n{rendered}"
    );
    assert!(rendered.contains("2 of 7 selected"));
}

#[test]
fn an_unmeasured_row_is_counted_rather_than_ignored() {
    let mut state = review();
    state.selected = BTreeSet::from([SAFE_A, 6]);
    let rendered = frame(&state, 80, 24);
    assert!(
        rendered.contains("1 not measured"),
        "the footer says how many rows the total is missing:\n{rendered}"
    );
    let state = apply(state, Key::Remove);
    let rendered = frame(&state, 80, 24);
    let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flattened.contains("1 of them was not measured, so the real figure is larger"),
        "and so does the confirmation:\n{rendered}"
    );
}

#[test]
fn the_pane_shows_why_the_cursor_row_is_what_it_is() {
    let state = drive(review(), &[Key::Down]);
    let rendered = frame(&state, 80, 24);
    assert!(rendered.contains("feature-login: safe"), "{rendered}");
    assert!(
        rendered.contains("upstream origin/feature-login is gone"),
        "{rendered}"
    );
    assert!(rendered.contains("merged into origin/main"), "{rendered}");
}

#[test]
fn the_detail_block_follows_the_cursor() {
    let first = frame(&review(), 80, 24);
    let moved = frame(&drive(review(), &[Key::Down]), 80, 24);

    assert!(first.contains("chore-deps: safe"), "{first}");
    assert!(!first.contains("feature-login: safe"), "{first}");
    assert!(moved.contains("feature-login: safe"), "{moved}");
    assert!(!moved.contains("chore-deps: safe"), "{moved}");
}

#[test]
fn the_detail_block_stays_bounded_in_narrow_and_short_frames() {
    let state = review();
    for (columns, rows) in [(40usize, 24usize), (80, 8), (40, 8)] {
        let rendered = frame(&state, columns, rows);
        assert_eq!(
            rendered.lines().count(),
            rows,
            "{columns}x{rows}:\n{rendered}"
        );
        assert!(
            rendered
                .lines()
                .all(|line| shear::render::display_width(line) <= columns),
            "{columns}x{rows}:\n{rendered}"
        );
    }

    let bounded = frame(&state, 40, 10);
    assert!(bounded.contains("chore-deps: safe"), "{bounded}");
    assert!(bounded.contains("  +2 more (widen the pane)"), "{bounded}");
    assert!(
        !bounded.contains("upstream origin/chore-deps is gone"),
        "a signal is omitted whole rather than truncated: {bounded}"
    );
}

#[test]
fn a_wrapped_signal_keeps_its_indent_at_the_minimum_width() {
    let state = drive(review(), &[Key::Down, Key::Down, Key::Down]);
    let rendered = frame(&state, 40, 24);

    assert!(
        rendered.contains(
            "mislabelled-locked: safe\n  locked (benchmark rig); run `git\n  worktree unlock\n"
        ),
        "the signal and its continuation stay grouped beneath the naming line:\n{rendered}"
    );
    assert_eq!(rendered.lines().count(), 24, "{rendered}");
    assert!(
        rendered
            .lines()
            .all(|line| shear::render::display_width(line) <= 40),
        "{rendered}"
    );
}

#[test]
fn a_long_detail_yields_before_confirmation_and_safety_lines() {
    let mut state = with_a_dirty_row();
    state.cursor = DIRTY_REVIEW;
    state = drive(state, &[Key::Remove, Key::Confirm]);
    let rendered = frame(&state, 80, 12);
    let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        flattened.contains("12 files that exist nowhere else"),
        "the destructive-work warning survives:\n{rendered}"
    );
    assert!(
        flattened.contains("Type the number of files at risk, then Enter. Esc cancels."),
        "the confirmation instructions survive:\n{rendered}"
    );
    assert!(rendered.contains("files at risk: 12"), "{rendered}");
    assert!(
        flattened.contains(
            "Removing a worktree leaves its branch and every commit on it intact: only the \
             checkout goes."
        ),
        "the safety note survives:\n{rendered}"
    );
    assert!(
        !rendered.contains("hotfix-payments: review"),
        "detail yields before protected blocks:\n{rendered}"
    );
}

/// The whole pane, character for character, after `a`. Twenty-four rows of
/// eighty columns is what an overlay pane usually gets.
#[test]
fn the_browsing_frame_is_pinned() {
    let expected =
        "shear \u{b7} review worktrees                                         2 of 7 selected
    verdict classes      age   disk branch          path

  repo app  /home/dev/repos/app
> * safe    gone,merged   3w 400 MB chore/deps      \u{2026}dev/repos/app-wt/chore-deps
  * safe    gone,merged   3w 1.2 GB feature/login   \u{2026}/repos/app-wt/feature-login
    safe    dirty         3w 9.5 MB spike/dirty     \u{2026}os/app-wt/mislabelled-dirty
    safe    locked        3w 9.5 MB spike/locked    \u{2026}s/app-wt/mislabelled-locked
    review  dirty,stale   3w  48 MB hotfix/payments \u{2026}epos/app-wt/hotfix-payments
    keep    open          3w      \u{2026} review/ui       \u{2026}/dev/repos/app-wt/review-ui
    blocked               3w 120 MB main            /home/dev/repos/app





chore-deps: safe
  upstream origin/chore-deps is gone
  merged into origin/main
2 selected \u{b7} 1.6 GB
Selected the 2 safe worktrees, and nothing else.
Removing a worktree leaves its branch and every commit on it intact: only the
checkout goes.
\u{2191}/k up  \u{2193}/j down  space select  a safe rows  n none  r remove  q quit
";
    assert_eq!(
        frame(&drive(review(), &[Key::SelectSafe]), 80, 24),
        expected
    );
}

/// The second confirmation, pinned: this is the screen someone reads before
/// destroying uncommitted work, so its exact wording is part of the contract.
#[test]
fn the_dirty_confirmation_frame_is_pinned() {
    let mut keys = vec![Key::Remove, Key::Confirm];
    keys.extend(typed("1"));
    let expected =
        "shear \u{b7} review worktrees                                         2 of 7 selected
    verdict classes      age   disk branch          path

  repo app  /home/dev/repos/app
>   safe    gone,merged   3w 400 MB chore/deps      \u{2026}dev/repos/app-wt/chore-deps
  * safe    gone,merged   3w 1.2 GB feature/login   \u{2026}/repos/app-wt/feature-login
    safe    dirty         3w 9.5 MB spike/dirty     \u{2026}os/app-wt/mislabelled-dirty
    safe    locked        3w 9.5 MB spike/locked    \u{2026}s/app-wt/mislabelled-locked
  * review  dirty,stale   3w  48 MB hotfix/payments \u{2026}epos/app-wt/hotfix-payments
    keep    open          3w      \u{2026} review/ui       \u{2026}/dev/repos/app-wt/review-ui
    blocked               3w 120 MB main            /home/dev/repos/app

chore-deps: safe
  upstream origin/chore-deps is gone
  merged into origin/main
2 selected \u{b7} 1.2 GB \u{b7} 12 uncommitted files in 1 of them
1 of the 2 selected worktrees has uncommitted work: 12 files that exist nowhere
else. Removing the checkout destroys them; no branch and no commit is touched.
This one cannot be answered with `y`. Type the number of files at risk, then
Enter. Esc cancels.
files at risk: 12    typed: 1_
Removing a worktree leaves its branch and every commit on it intact: only the
checkout goes.
\u{2191}/k up  \u{2193}/j down  space select  a safe rows  n none  r remove  q quit
";
    assert_eq!(frame(&drive(with_a_dirty_row(), &keys), 80, 24), expected);
}

// ---------------------------------------------------------------------------
// Key decoding
// ---------------------------------------------------------------------------

#[test]
fn arrow_keys_are_not_mistaken_for_escape() {
    assert_eq!(decode(b"\x1b[A"), vec![Key::Up]);
    assert_eq!(decode(b"\x1b[B"), vec![Key::Down]);
    assert_eq!(decode(b"\x1b"), vec![Key::Cancel]);
    assert_eq!(decode(b"kj"), vec![Key::Up, Key::Down]);
}

#[test]
fn every_documented_key_decodes() {
    assert_eq!(
        decode(b" anrq\r\x7f5"),
        vec![
            Key::Toggle,
            Key::SelectSafe,
            Key::SelectNone,
            Key::Remove,
            Key::Quit,
            Key::Confirm,
            Key::Backspace,
            Key::Digit(5),
        ]
    );
    assert_eq!(decode(b""), Vec::new());
    assert_eq!(decode(b"\x1b[Z"), vec![Key::Other]);
}

/// The regression test for the one bug this whole suite could not see.
///
/// `frame` is pure and joins with `\n`, which is what makes the pane testable
/// without a terminal — and is exactly why 30 passing tests said nothing about
/// what a terminal in raw mode actually does with that output. Raw mode turns
/// off ONLCR, so a bare line feed drops a row without returning to column 0 and
/// the pane staircases off the side of the screen. It was found by running the
/// binary in a real herdr pane and looking at it.
#[test]
fn every_line_the_pane_writes_ends_in_crlf() {
    let review = review();
    let drawn = for_raw_terminal(&frame(&review, 80, 20));

    assert!(
        drawn.contains("\r\n"),
        "the frame must carry carriage returns"
    );
    // No bare line feed anywhere: in raw mode each one is a staircase step.
    assert!(
        !drawn
            .as_bytes()
            .windows(2)
            .any(|pair| pair[1] == b'\n' && pair[0] != b'\r'),
        "a line feed with no carriage return before it staircases the pane"
    );
    assert!(
        !drawn.starts_with('\n'),
        "a leading line feed staircases the first row"
    );
    // The visible text is untouched: this rewrites line endings and nothing
    // else, so every assertion elsewhere in this file still describes what a
    // user sees.
    assert_eq!(drawn.replace("\r\n", "\n"), frame(&review, 80, 20));
}
