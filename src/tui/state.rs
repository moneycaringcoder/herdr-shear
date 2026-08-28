//! The review pane's pure state machine.
//!
//! Destructive interfaces earn trust by making every transition inspectable.
//! Nothing here reads a terminal, scans a repository, or removes a checkout:
//! [`apply`] is a total function, and fresh inventories enter only through
//! [`preflight`] and [`adopt`]. The ratatui view and crossterm runtime can change
//! without changing what any key is allowed to mean.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::model::{Class, Inventory, Size, Verdict};
use crate::render;

/// Keys. Deliberately small, and deliberately not vim-only: `j`/`k` and the
/// arrow keys both move.
///
/// ```text
///   ↑ / k        previous row
///   ↓ / j        next row
///   space        toggle selection
///   a            select every `safe` row (and nothing else, ever)
///   n            clear the selection
///   r            remove the selection, after confirming
///   R            rescan: re-read git and herdr without touching anything
///   q / Esc      quit without removing anything
///   0-9          type the file count the dirty confirmation asks for
/// ```
///
/// [`Key::Digit`] and [`Key::Backspace`] exist for the second confirmation
/// alone. It cannot be answered with `y`, so the typed digits have to reach
/// [`apply`] as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Toggle,
    SelectSafe,
    SelectNone,
    Remove,
    Quit,
    Confirm,
    Cancel,
    Rescan,
    Digit(u8),
    Backspace,
    Other,
}

/// The review pane's state. Pure: [`apply`] is a total function from state and
/// key to state, so `tests/render.rs` can drive an entire session — including
/// both confirmations for a dirty removal — without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub inventory: Inventory,
    pub cursor: usize,
    /// Indices into `inventory.candidates`.
    pub selected: BTreeSet<usize>,
    pub mode: Mode,
    /// Messages from the last action, shown under the table.
    pub messages: Vec<String>,
    /// Undo warnings survive redraws, later actions, and pane exit.
    pub undo_warnings: Vec<String>,
}

/// What the pane is currently asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    /// A fresh scan is owed before any destructive confirmation is shown.
    Preflighting,
    /// First confirmation, for a selection of clean worktrees.
    ConfirmClean {
        count: usize,
        bytes: u64,
    },
    /// Second, differently-worded confirmation for a selection that includes
    /// dirty worktrees. It names the file count at risk and requires typing that
    /// number, not pressing `y` — a confirmation that can be given without
    /// reading it is not a confirmation.
    ConfirmDirty {
        files: usize,
        typed: String,
        worktrees: usize,
    },
    Removing,
    /// A rescan is owed. Like [`Mode::Preflighting`] and [`Mode::Removing`], the
    /// driver owns this state: [`apply`] never performs I/O, so it can only ask.
    Rescanning,
    Done,
}

impl Review {
    pub fn new(inventory: Inventory) -> Self {
        let cursor = display_order(&inventory).first().copied().unwrap_or(0);
        Self {
            inventory,
            cursor,
            selected: BTreeSet::new(),
            mode: Mode::Browsing,
            messages: Vec::new(),
            undo_warnings: Vec::new(),
        }
    }

    /// The pane is finished and the driver should restore the terminal and
    /// leave. Reached by `q`, never by a removal on its own.
    pub fn is_finished(&self) -> bool {
        self.mode == Mode::Done
    }

    /// Selected rows, in table order.
    pub fn selection(&self) -> impl Iterator<Item = &crate::model::Candidate> {
        self.selected
            .iter()
            .filter_map(|index| self.inventory.candidates.get(*index))
    }

    /// Files that would be destroyed by removing the current selection, and how
    /// many worktrees carry them. Nothing else in the pane is allowed to invent
    /// this number.
    pub fn at_risk(&self) -> (usize, usize) {
        let mut files = 0;
        let mut worktrees = 0;
        for candidate in self.selection() {
            if candidate.dirt.is_dirty() {
                files += candidate.dirt.total();
                worktrees += 1;
            }
        }
        (files, worktrees)
    }
}

/// Rows in the order the table draws them: grouped by repository, and within a
/// repository sorted by verdict.
///
/// The cursor moves through *this*, not through the inventory's own order, or
/// `j` would jump around the screen — and a selection made by a cursor that
/// jumps is a selection nobody can trust.
pub fn display_order(inventory: &Inventory) -> Vec<usize> {
    render::grouped_indices(inventory)
        .into_iter()
        .flat_map(|(_, group)| group)
        .collect()
}

/// The row `steps` places from the cursor in display order.
fn step(review: &Review, steps: isize) -> usize {
    let order = display_order(&review.inventory);
    if order.is_empty() {
        return 0;
    }
    let at = order
        .iter()
        .position(|index| *index == review.cursor)
        .unwrap_or(0) as isize;
    let next = (at + steps).clamp(0, order.len() as isize - 1) as usize;
    order[next]
}

/// The rows `a` may preselect.
///
/// [`Verdict::Safe`] already excludes every one of these, so each extra test is
/// redundant — deliberately. If a future classifier bug ever called a dirty or
/// locked worktree safe, the bulk key would still not touch it.
pub fn preselectable(candidate: &crate::model::Candidate) -> bool {
    candidate.verdict == Verdict::Safe
        && !candidate.worktree.is_main
        && !candidate.is(Class::Dirty)
        && !candidate.is(Class::Locked)
        && !candidate.is(Class::OpenInHerdr)
        && !candidate.is(Class::Occupied)
        && candidate.worktree.locked.is_none()
        && candidate.open_workspace.is_none()
        && candidate.occupants.is_empty()
        && !candidate.dirt.is_dirty()
}

/// Applies one key. Never performs I/O; a transition into
/// [`Mode::Preflighting`] tells the driver to refresh before it asks for
/// destructive confirmation.
pub fn apply(mut review: Review, key: Key) -> Review {
    let rows = review.inventory.candidates.len();
    if rows == 0 {
        review.cursor = 0;
    } else if review.cursor >= rows {
        review.cursor = rows - 1;
    }

    match review.mode.clone() {
        Mode::Browsing => browsing(review, key),
        Mode::ConfirmClean { count, bytes } => confirm_clean(review, key, count, bytes),
        Mode::ConfirmDirty {
            files,
            typed,
            worktrees,
        } => confirm_dirty(review, key, files, typed, worktrees),
        // The driver owns these four: one is a preflight scan, one is a removal
        // in flight, one is a rescan in flight, and the last is a pane that has
        // already said its last word.
        Mode::Preflighting | Mode::Removing | Mode::Rescanning | Mode::Done => review,
    }
}

fn browsing(mut review: Review, key: Key) -> Review {
    match key {
        Key::Up => {
            review.cursor = step(&review, -1);
        }
        Key::Down => {
            review.cursor = step(&review, 1);
        }
        Key::Toggle => {
            review.messages.clear();
            let Some(candidate) = review.inventory.candidates.get(review.cursor) else {
                return review;
            };
            if candidate.worktree.is_main {
                review.messages.push(
                    "That is the main checkout. It is never removable, and there is no override."
                        .into(),
                );
                return review;
            }
            // A blocked row can never be removed from this pane — the pane
            // never grants `close_workspace`, and protection and locks have no
            // override at all — so selecting one could only end in a refusal
            // at removal time. Refuse at the keypress instead, when the reason
            // can still name the unblocking action next to the row it is about.
            //
            // Unlike `preselectable`, this gates on the verdict alone: `a` is
            // a bulk key acting on rows the user never looked at, so it
            // re-checks the blocking facts against a classifier gone wrong;
            // `space` is an explicit per-row choice, and the removal path
            // re-checks every fact regardless.
            if candidate.verdict == Verdict::Blocked {
                review.messages.push(format!(
                    "Blocked, so it cannot be selected: {}",
                    candidate.reason
                ));
                return review;
            }
            if !review.selected.remove(&review.cursor) {
                review.selected.insert(review.cursor);
            }
        }
        Key::SelectSafe => {
            review.messages.clear();
            // Replaces the selection rather than adding to it, so `a` cannot
            // leave anything unsafe selected even by accident.
            review.selected = review
                .inventory
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| preselectable(candidate))
                .map(|(index, _)| index)
                .collect();
            review.messages.push(format!(
                "Selected the {} safe {}, and nothing else.",
                review.selected.len(),
                if review.selected.len() == 1 {
                    "worktree"
                } else {
                    "worktrees"
                },
            ));
        }
        Key::SelectNone => {
            review.messages.clear();
            review.selected.clear();
            review.messages.push("Selection cleared.".into());
        }
        Key::Rescan => {
            review.messages.clear();
            review.mode = Mode::Rescanning;
        }
        Key::Remove => {
            review.messages.clear();
            if review.selected.is_empty() {
                review.messages.push(
                    "Nothing is selected. Move with the arrow keys, select with space, or press \
                     `a` for the safe rows."
                        .into(),
                );
                return review;
            }
            review.mode = Mode::Preflighting;
        }
        Key::Quit => {
            review.messages.clear();
            review.mode = Mode::Done;
            review.messages.push("Quit.".into());
        }
        Key::Confirm | Key::Cancel | Key::Digit(_) | Key::Backspace | Key::Other => {}
    }
    review
}

fn confirm_clean(mut review: Review, key: Key, count: usize, bytes: u64) -> Review {
    match key {
        Key::Confirm => {
            let (files, worktrees) = review.at_risk();
            review.mode = if files > 0 {
                // Anything dirty in the selection buys a second, differently
                // worded question — one that cannot be answered by reflex.
                Mode::ConfirmDirty {
                    files,
                    typed: String::new(),
                    worktrees,
                }
            } else {
                Mode::Removing
            };
        }
        Key::Cancel | Key::Quit => {
            review.mode = Mode::Browsing;
            review.messages = vec!["Cancelled. Nothing was removed.".into()];
        }
        _ => {
            let _ = (count, bytes);
        }
    }
    review
}

fn confirm_dirty(
    mut review: Review,
    key: Key,
    files: usize,
    mut typed: String,
    worktrees: usize,
) -> Review {
    match key {
        Key::Digit(digit) => {
            // Nine digits is more files than any worktree has, and it stops a
            // leaned-on key from growing the string without bound.
            if typed.len() < 9 {
                typed.push((b'0' + digit) as char);
            }
            review.mode = Mode::ConfirmDirty {
                files,
                typed,
                worktrees,
            };
        }
        Key::Backspace => {
            typed.pop();
            review.mode = Mode::ConfirmDirty {
                files,
                typed,
                worktrees,
            };
        }
        Key::Confirm => {
            if typed.parse::<usize>() == Ok(files) {
                review.mode = Mode::Removing;
            } else {
                review.mode = Mode::ConfirmDirty {
                    files,
                    typed: String::new(),
                    worktrees,
                };
                review.messages = vec![format!(
                    "That is not the number of files at risk. Nothing was removed. Type {files}."
                )];
            }
        }
        Key::Cancel | Key::Quit => {
            review.mode = Mode::Browsing;
            review.messages = vec!["Cancelled. Nothing was removed.".into()];
        }
        _ => {
            review.mode = Mode::ConfirmDirty {
                files,
                typed,
                worktrees,
            };
        }
    }
    review
}

/// Applies a pre-confirmation scan result without performing I/O.
///
/// Success adopts by exact path, using the same rules as `R`: vanished and
/// newly blocked rows are dropped. Only the surviving fresh selection can
/// produce a confirmation. Failure preserves the previous inventory,
/// selection, and cursor so the user can retry.
pub fn preflight<E: std::fmt::Display>(
    mut review: Review,
    scanned: std::result::Result<Inventory, E>,
) -> Review {
    let mut inventory = match scanned {
        Ok(inventory) => inventory,
        Err(err) => {
            review.mode = Mode::Browsing;
            review.messages = vec![format!(
                "could not refresh selection before confirmation ({err}); showing the previous \
                 scan. Nothing was removed. Press `r` to try again or `R` to rescan."
            )];
            return review;
        }
    };

    // A scan deliberately does no disk walking, so its rows arrive Pending.
    // Carry the exact state already known for the same path into the fresh
    // inventory; otherwise an unchanged selection flashes a false 0 B while
    // still-Pending rows restart their background walks.
    for candidate in &mut inventory.candidates {
        if candidate.size != Size::Pending {
            continue;
        }
        if let Some(previous) = review.inventory.find(candidate.path()) {
            candidate.size = previous.size;
        }
    }
    let selected_before = review.selected.len();
    review = adopt(review, inventory);
    let dropped = selected_before.saturating_sub(review.selected.len());

    if review.selected.is_empty() {
        review.messages = if dropped == 0 {
            vec!["Nothing is selected. Nothing was removed.".into()]
        } else {
            vec![format!(
                "{} Nothing remains selected, so nothing was removed.",
                dropped_selection_message("Refreshed selection", dropped)
            )]
        };
        return review;
    }

    review.messages = if dropped == 0 {
        Vec::new()
    } else {
        vec![dropped_selection_message("Refreshed selection", dropped)]
    };
    let (bytes, _) = crate::shear::reclaimable(review.selection());
    review.mode = Mode::ConfirmClean {
        count: review.selected.len(),
        bytes,
    };
    review
}

fn dropped_selection_message(action: &str, dropped: usize) -> String {
    format!(
        "{action}. Dropped {dropped} selected {}: gone, or not removable.",
        if dropped == 1 {
            "worktree"
        } else {
            "worktrees"
        }
    )
}

/// Swaps in a fresh inventory, carrying the selection and the cursor across by
/// path. Indices mean nothing across a rescan; paths do.
///
/// A selected row that is gone, or that the new scan calls blocked, drops out
/// of the selection and is counted in the message — silently keeping a
/// selection on a row whose facts changed is how a pane removes something the
/// user did not mean.
pub fn adopt(mut review: Review, inventory: Inventory) -> Review {
    let selected_paths: Vec<PathBuf> = review
        .selection()
        .map(|candidate| candidate.worktree.path.clone())
        .collect();
    let cursor_path = review
        .inventory
        .candidates
        .get(review.cursor)
        .map(|candidate| candidate.worktree.path.clone());

    review.inventory = inventory;

    let mut dropped = 0usize;
    review.selected = selected_paths
        .iter()
        .filter_map(|path| {
            let index = review
                .inventory
                .candidates
                .iter()
                .position(|candidate| &candidate.worktree.path == path);
            match index {
                Some(index) if review.inventory.candidates[index].verdict != Verdict::Blocked => {
                    Some(index)
                }
                _ => {
                    dropped += 1;
                    None
                }
            }
        })
        .collect();

    review.cursor = cursor_path
        .and_then(|path| {
            review
                .inventory
                .candidates
                .iter()
                .position(|candidate| candidate.worktree.path == path)
        })
        .unwrap_or_else(|| {
            display_order(&review.inventory)
                .first()
                .copied()
                .unwrap_or(0)
        });

    review.mode = Mode::Browsing;
    review.messages = if dropped == 0 {
        vec!["Rescanned.".into()]
    } else {
        vec![dropped_selection_message("Rescanned", dropped)]
    };
    review
}
