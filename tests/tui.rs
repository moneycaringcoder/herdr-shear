//! The review pane, driven entirely without a terminal.
//!
//! [`shear::tui::apply`] is a total function from state and key to state, which
//! is the whole reason a destructive interface can be tested at all: every
//! assertion below is a session someone could sit through, including both
//! confirmations for a dirty removal and the one that ends in `q`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use shear::model::{
    Candidate, Class, Dirt, Head, Inventory, LockInfo, Merged, OpenWorkspace, Repo, RepoKey, Size,
    Upstream, Verdict, Worktree,
};
use shear::tui::{adopt, apply, preflight, Key, Mode, Review};

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
            paths: staged + unstaged + untracked,
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
const SAFE_A_PATH: &str = "/home/dev/repos/app-wt/feature-login";
const SAFE_B_PATH: &str = "/home/dev/repos/app-wt/chore-deps";

fn review() -> Review {
    Review::new(inventory())
}

fn drive(mut review: Review, keys: &[Key]) -> Review {
    for key in keys {
        review = apply(review, *key);
        if review.mode == Mode::Preflighting {
            // `scan` returns Pending sizes; preflight must carry any state the
            // pane already knows by path rather than resetting confirmation
            // bytes to zero.
            let mut fresh = review.inventory.clone();
            for candidate in &mut fresh.candidates {
                candidate.size = Size::Pending;
            }
            review = preflight(review, Ok::<Inventory, &str>(fresh));
        }
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
fn incomplete_visibility_rows_are_never_bulk_selected_but_remain_explicitly_selectable() {
    let mut inventory = inventory();
    let blind = inventory.candidates.len();
    inventory.candidates.push(
        Row::new("/home/dev/repos/app-wt/blind", "landed/blind")
            .verdict(Verdict::Review)
            .classes(&[Class::Merged, Class::GoneUpstream])
            .merged(Merged::Into("origin/main".into()))
            .upstream_gone("origin/landed-blind")
            .reason("herdr workspace and pane visibility is incomplete")
            .build(),
    );

    let after_bulk = drive(Review::new(inventory.clone()), &[Key::SelectSafe]);
    assert!(!after_bulk.selected.contains(&blind));

    let mut explicit = Review::new(inventory);
    explicit.cursor = blind;
    let after_toggle = drive(explicit, &[Key::Toggle]);
    assert_eq!(selection(&after_toggle), BTreeSet::from([blind]));
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
// Removal preflight
// ---------------------------------------------------------------------------

#[test]
fn remove_enters_preflight_before_any_confirmation() {
    let selected = drive(review(), &[Key::SelectSafe]);
    let after = apply(selected, Key::Remove);

    assert_eq!(after.mode, Mode::Preflighting);
    assert_eq!(
        apply(after, Key::Confirm).mode,
        Mode::Preflighting,
        "confirmation input cannot bypass the fresh scan"
    );
}

#[test]
fn a_failed_preflight_preserves_the_old_inventory_selection_and_cursor() {
    let selected = drive(review(), &[Key::SelectSafe]);
    let requested = apply(selected, Key::Remove);
    let old_inventory = requested.inventory.clone();
    let old_selection = requested.selected.clone();
    let old_cursor = requested.cursor;

    let after = preflight(requested, Err::<Inventory, _>("git status timed out"));

    assert_eq!(after.inventory, old_inventory);
    assert_eq!(after.selected, old_selection);
    assert_eq!(after.cursor, old_cursor);
    assert_eq!(after.mode, Mode::Browsing);
    assert!(
        after.messages.iter().any(|message| {
            message.contains("git status timed out")
                && message.contains("Nothing was removed")
                && message.contains("Press `r` to try again")
        }),
        "the old state remains usable and the message says how to retry: {:?}",
        after.messages
    );
}

#[test]
fn a_newly_blocked_selection_is_dropped_before_confirmation() {
    let mut selected = review();
    selected.selected = BTreeSet::from([SAFE_A]);
    let requested = apply(selected, Key::Remove);
    let mut fresh = inventory();
    fresh.candidates[SAFE_A].verdict = Verdict::Blocked;
    fresh.candidates[SAFE_A].reason = "opened in herdr workspace ui-review".into();

    let after = preflight(requested, Ok::<Inventory, &str>(fresh));

    assert_eq!(after.mode, Mode::Browsing);
    assert!(after.selected.is_empty());
    assert!(
        after
            .messages
            .iter()
            .any(|message| message.contains("Dropped 1 selected worktree")),
        "{:?}",
        after.messages
    );
}

#[test]
fn a_vanished_selection_is_dropped_before_confirmation() {
    let mut selected = review();
    selected.selected = BTreeSet::from([SAFE_A]);
    let requested = apply(selected, Key::Remove);
    let mut fresh = inventory();
    fresh
        .candidates
        .retain(|candidate| candidate.worktree.path != Path::new(SAFE_A_PATH));

    let after = preflight(requested, Ok::<Inventory, &str>(fresh));

    assert_eq!(after.mode, Mode::Browsing);
    assert!(after.selected.is_empty());
    assert!(
        after
            .messages
            .iter()
            .any(|message| message.contains("Nothing remains selected")),
        "{:?}",
        after.messages
    );
}

#[test]
fn a_partial_fresh_selection_confirms_only_survivors_and_keeps_the_drop_message() {
    let selected = drive(review(), &[Key::SelectSafe]);
    let requested = apply(selected, Key::Remove);
    let mut fresh = inventory();
    fresh.candidates[SAFE_B].verdict = Verdict::Blocked;
    fresh.candidates[SAFE_B].reason = "occupied by another herdr pane".into();

    let after = preflight(requested, Ok::<Inventory, &str>(fresh));

    assert!(matches!(&after.mode, Mode::ConfirmClean { count: 1, .. }));
    assert_eq!(
        after
            .selection()
            .map(|candidate| candidate.worktree.path.as_path())
            .collect::<Vec<_>>(),
        vec![Path::new(SAFE_A_PATH)]
    );
    assert!(after
        .messages
        .iter()
        .any(|message| message.contains("Dropped 1 selected worktree")));

    let removing = apply(after, Key::Confirm);
    assert_eq!(removing.mode, Mode::Removing);
    assert_eq!(
        removing
            .selection()
            .map(|candidate| candidate.worktree.path.as_path())
            .collect::<Vec<_>>(),
        vec![Path::new(SAFE_A_PATH)],
        "the blocked row is absent from the only selection perform can see"
    );
}

#[test]
fn scan_shaped_pending_rows_preserve_known_confirmation_bytes() {
    let selected = drive(review(), &[Key::SelectSafe]);
    let requested = apply(selected, Key::Remove);
    let mut fresh = inventory();
    for candidate in &mut fresh.candidates {
        candidate.size = Size::Pending;
    }

    let after = preflight(requested, Ok::<Inventory, &str>(fresh));

    assert_eq!(
        after.mode,
        Mode::ConfirmClean {
            count: 2,
            bytes: 1_310_000_000 + 419_430_400,
        }
    );
    assert!(after
        .selection()
        .all(|candidate| matches!(candidate.size, Size::Bytes(_))));
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
}

#[test]
fn the_dirty_confirmation_aggregates_unique_paths_not_status_dimensions() {
    let mut review = with_a_dirty_row();
    review.inventory.candidates[DIRTY_REVIEW].dirt = Dirt {
        paths: 1,
        staged: 1,
        unstaged: 1,
        untracked: 0,
        unmerged: 0,
    };

    let after = drive(review, &[Key::Remove, Key::Confirm]);
    assert_eq!(
        after.mode,
        Mode::ConfirmDirty {
            files: 1,
            typed: String::new(),
            worktrees: 1,
        }
    );
}

#[test]
fn a_changed_dirty_count_is_the_count_that_must_be_typed() {
    let requested = apply(with_a_dirty_row(), Key::Remove);
    let mut fresh = inventory();
    fresh.candidates[DIRTY_REVIEW].dirt = Dirt {
        paths: 17,
        staged: 5,
        unstaged: 7,
        untracked: 5,
        unmerged: 0,
    };

    let first = preflight(requested, Ok::<Inventory, &str>(fresh));
    assert!(matches!(&first.mode, Mode::ConfirmClean { count: 2, .. }));
    let current = apply(first, Key::Confirm);
    assert_eq!(
        current.mode,
        Mode::ConfirmDirty {
            files: 17,
            typed: String::new(),
            worktrees: 1,
        }
    );

    let mut stale_answer = typed("12");
    stale_answer.push(Key::Confirm);
    let refused = drive(current, &stale_answer);
    assert!(matches!(
        &refused.mode,
        Mode::ConfirmDirty {
            files: 17,
            typed,
            ..
        } if typed.is_empty()
    ));

    let mut fresh_answer = typed("17");
    fresh_answer.push(Key::Confirm);
    assert_eq!(drive(refused, &fresh_answer).mode, Mode::Removing);
}

#[test]
fn the_dirty_confirmation_cannot_be_answered_with_y() {
    // An empty Confirm is exactly the reflex this confirmation exists to
    // defeat.
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
        if review.mode == Mode::Preflighting {
            let fresh = review.inventory.clone();
            review = preflight(review, Ok::<Inventory, &str>(fresh));
        }
        modes.push(review.mode.clone());
    }

    assert!(
        !modes.contains(&Mode::Removing),
        "the session never reached the one mode that removes anything: {modes:?}"
    );
    assert_eq!(review.mode, Mode::Done);
    assert_eq!(review.messages, ["Quit."]);
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
        Key::Rescan,
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
        Mode::Preflighting,
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
        Mode::Rescanning,
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
// Rescanning
// ---------------------------------------------------------------------------

#[test]
fn capital_r_asks_for_a_rescan_and_only_while_browsing() {
    let after = drive(review(), &[Key::Rescan]);
    assert_eq!(after.mode, Mode::Rescanning, "the driver owns the rescan");

    // Mid-confirmation, `R` is not a rescan: a stray key must never dismiss
    // the question being asked.
    let mut state = review();
    state.cursor = DIRTY_REVIEW;
    let confirming = drive(
        state,
        &[Key::Toggle, Key::Remove, Key::Confirm, Key::Rescan],
    );
    assert!(
        matches!(confirming.mode, Mode::ConfirmDirty { .. }),
        "still confirming: {:?}",
        confirming.mode
    );
}

#[test]
fn a_rescan_carries_the_selection_and_the_cursor_by_path_not_by_index() {
    let mut state = drive(review(), &[Key::SelectSafe]);
    state.cursor = DIRTY_REVIEW;

    // The new scan reverses the table, so every index means something else.
    let mut rescanned = inventory();
    rescanned.candidates.reverse();
    let after = adopt(state, rescanned);

    let selected_paths: BTreeSet<&str> = after
        .selection()
        .map(|candidate| candidate.worktree.path.to_str().unwrap())
        .collect();
    assert_eq!(
        selected_paths,
        BTreeSet::from([
            "/home/dev/repos/app-wt/feature-login",
            "/home/dev/repos/app-wt/chore-deps",
        ]),
        "the same worktrees stay selected whatever their new indices are"
    );
    assert_eq!(
        after.inventory.candidates[after.cursor].worktree.path,
        PathBuf::from("/home/dev/repos/app-wt/hotfix-payments"),
        "the cursor follows its row"
    );
    assert_eq!(after.mode, Mode::Browsing);
    assert!(after.messages.iter().any(|m| m == "Rescanned."));
}

#[test]
fn a_rescan_drops_selected_rows_that_are_gone_or_newly_blocked_and_says_so() {
    let state = drive(review(), &[Key::SelectSafe]);

    // The new scan lost one selected row entirely and calls the other blocked
    // — somebody opened a workspace on it between the two scans.
    let mut rescanned = inventory();
    rescanned
        .candidates
        .retain(|candidate| candidate.worktree.path != Path::new(SAFE_A_PATH));
    for candidate in rescanned.candidates.iter_mut() {
        if candidate.worktree.path == Path::new(SAFE_B_PATH) {
            candidate.verdict = Verdict::Blocked;
        }
    }
    let after = adopt(state, rescanned);

    assert!(after.selected.is_empty(), "{:?}", after.selected);
    assert!(
        after
            .messages
            .iter()
            .any(|m| m.contains("Dropped 2 selected worktrees")),
        "the drop is said, not silent: {:?}",
        after.messages
    );
}
