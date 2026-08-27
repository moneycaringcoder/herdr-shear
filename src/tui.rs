//! The interactive review pane.
//!
//! It runs in a herdr **overlay pane**, not a popup. The reason is written down
//! in the README, and it is this: the pane is a working surface. A user scans
//! forty rows, selects several, and then confirms something destructive. A popup
//! is dismissed by a stray key, and losing a selection to a mis-key is worse
//! than having to press `q`. An overlay pane survives the mis-key.
//!
//! Terminal discipline, which matters more here than anywhere else in the crate:
//! raw mode is entered once, restored from `Drop`, from a panic hook, and from
//! SIGINT/SIGTERM. A janitor that leaves somebody's pane in raw mode has done
//! more damage than the worktrees it removed.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::SystemTime;

use crate::config::Config;
use crate::model::{Class, Inventory, Size, Verdict};
use crate::render::{self, MIN_COLUMNS};
use crate::Result;

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

// ---------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------

const TITLE: &str = "shear \u{b7} review worktrees";
const HELP_WIDE: &str = "\u{2191}/k up  \u{2193}/j down  space select  a safe rows  n none  \
                         r remove  R rescan  q quit";
const HELP_NARROW: &str = "\u{2191}\u{2193} move  space select  a safe  r remove  q quit";

/// Renders the current state to a frame of exactly the given size.
///
/// Exactly: the result always has `rows` lines, each at most `columns` display
/// columns wide. An overlay pane that sometimes emits one line too many scrolls
/// its own header off the top.
pub fn frame(review: &Review, columns: usize, rows: usize) -> String {
    let width = columns.max(MIN_COLUMNS);
    let rows = rows.max(1);

    let head = head_lines(review, width);
    let foot = foot_lines(review, width, rows.saturating_sub(head.len() + 1));
    let body_budget = rows.saturating_sub(head.len() + foot.len());

    let mut lines: Vec<String> = head;
    lines.extend(body_lines(review, width, body_budget));
    while lines.len() + foot.len() < rows {
        lines.push(String::new());
    }
    lines.extend(foot);
    lines.truncate(rows);

    let mut out = String::new();
    for line in lines {
        render::push_line(&mut out, &line, width);
    }
    out
}

fn head_lines(review: &Review, width: usize) -> Vec<String> {
    let counted = format!(
        "{} of {} selected",
        review.selected.len(),
        review.inventory.candidates.len()
    );
    let pad = width
        .saturating_sub(render::display_width(TITLE))
        .saturating_sub(render::display_width(&counted));
    let title = if pad >= 2 {
        format!("{TITLE}{}{counted}", " ".repeat(pad))
    } else {
        TITLE.to_string()
    };

    let widths = render::widths_for(&review.inventory, width.saturating_sub(2));
    vec![title, format!("  {}", render::header(&widths))]
}

/// The scrollable half: repo headings, rows, and the scan's notes. The cursor is
/// always inside the window.
fn body_lines(review: &Review, width: usize, budget: usize) -> Vec<String> {
    if budget == 0 {
        return Vec::new();
    }
    let widths = render::widths_for(&review.inventory, width.saturating_sub(2));
    let mut lines: Vec<(String, Option<usize>)> = Vec::new();

    if review.inventory.candidates.is_empty() {
        lines.push(("  no worktrees found.".to_string(), None));
    }
    for (key, group) in render::grouped_indices(&review.inventory) {
        let Some(first) = group
            .first()
            .and_then(|i| review.inventory.candidates.get(*i))
        else {
            continue;
        };
        lines.push((String::new(), None));
        lines.push((
            format!(
                "  {}",
                render::repo_heading(&review.inventory, &key, first, width.saturating_sub(2))
            ),
            None,
        ));
        for index in group {
            let Some(candidate) = review.inventory.candidates.get(index) else {
                continue;
            };
            let cursor = if index == review.cursor { "> " } else { "  " };
            let row = render::row(candidate, &widths, review.selected.contains(&index));
            lines.push((format!("{cursor}{row}"), Some(index)));
        }
    }
    if !review.inventory.notes.is_empty() {
        lines.push((String::new(), None));
        lines.push(("  notes".to_string(), None));
        for note in &review.inventory.notes {
            for line in render::wrap(note, width.saturating_sub(4)) {
                lines.push((format!("    {line}"), None));
            }
        }
    }

    // Scroll so the cursor row is inside the window, and never past the end.
    let cursor_line = lines
        .iter()
        .position(|(_, index)| *index == Some(review.cursor))
        .unwrap_or(0);
    let mut top = 0;
    if lines.len() > budget {
        if cursor_line >= budget {
            top = cursor_line + 1 - budget;
        }
        top = top.min(lines.len() - budget);
    }

    lines
        .into_iter()
        .skip(top)
        .take(budget)
        .map(|(line, _)| line)
        .collect()
}

fn foot_lines(review: &Review, width: usize, budget: usize) -> Vec<String> {
    let mut blocks: Vec<Vec<String>> = Vec::new();
    blocks.push(vec![String::new()]);
    blocks.push(detail_block(review, width, usize::MAX).unwrap_or_default());
    blocks.push(vec![selection_line(review)]);
    blocks.push(mode_lines(review, width));
    // The sentence that makes the action feel safe, where it can be read while
    // the user is deciding. It is never the block that gets dropped.
    blocks.push(render::wrap(render::SAFETY_NOTE, width));
    blocks.push(vec![
        // Derived from the string, not a magic number, so growing the help can
        // never leave a band of widths where it is silently clipped — and the
        // part that gets cut is `q quit`, the documented way out.
        if width >= render::display_width(HELP_WIDE) {
            HELP_WIDE
        } else {
            HELP_NARROW
        }
        .to_string(),
    ]);

    let mut dropped: BTreeSet<usize> = BTreeSet::new();
    let total = |dropped: &BTreeSet<usize>, blocks: &[Vec<String>]| {
        blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, block)| block.len())
            .sum::<usize>()
    };

    // Trim from the least useful block up when the pane is short. Help yields
    // first. The detail then gives up whole signals, and finally the entire
    // block, before selection and the blank separator yield. The safety note
    // and whatever the pane is currently asking always survive.
    if total(&dropped, &blocks) > budget {
        dropped.insert(5);
    }
    if total(&dropped, &blocks) > budget {
        let without_detail = blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 1 && !dropped.contains(index))
            .map(|(_, block)| block.len())
            .sum::<usize>();
        let detail_budget = budget.saturating_sub(without_detail);
        match detail_block(review, width, detail_budget) {
            Some(detail) => blocks[1] = detail,
            None => {
                dropped.insert(1);
            }
        }
    }
    for index in [2usize, 0] {
        if total(&dropped, &blocks) <= budget {
            break;
        }
        dropped.insert(index);
    }

    blocks
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !dropped.contains(index))
        .flat_map(|(_, block)| block)
        .collect()
}

fn detail_block(review: &Review, width: usize, budget: usize) -> Option<Vec<String>> {
    let candidate = review.inventory.candidates.get(review.cursor)?;
    let name = candidate
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| candidate.path().to_string_lossy().into_owned());
    let mut lines = render::wrap(&format!("{name}: {}", candidate.verdict.label()), width);
    if lines.len() > budget {
        return None;
    }

    let wrapped_signals: Vec<Vec<String>> = crate::classify::signals(candidate, SystemTime::now())
        .into_iter()
        .map(|signal| {
            let mut wrapped = String::new();
            render::push_wrapped(&mut wrapped, "  ", "  ", &signal, width);
            wrapped.lines().map(str::to_string).collect()
        })
        .collect();
    let signal_count = wrapped_signals.len();
    let full_len = lines.len() + wrapped_signals.iter().map(Vec::len).sum::<usize>();
    if full_len <= budget {
        lines.extend(wrapped_signals.into_iter().flatten());
        return Some(lines);
    }

    // The omission marker needs its own line. If even the naming line plus that
    // marker cannot fit, the normal footer drop order removes the whole detail
    // block rather than showing an ambiguous fragment.
    if lines.len() + 1 > budget {
        return None;
    }
    let signal_budget = budget - lines.len() - 1;
    let mut used = 0;
    let mut shown = 0;
    for wrapped in wrapped_signals {
        if used + wrapped.len() > signal_budget {
            break;
        }
        used += wrapped.len();
        shown += 1;
        lines.extend(wrapped);
    }
    lines.push(format!("  +{} more (widen the pane)", signal_count - shown));
    Some(lines)
}

fn selection_line(review: &Review) -> String {
    if review.selected.is_empty() {
        return "nothing selected".to_string();
    }
    let (bytes, unknown) = crate::shear::reclaimable(review.selection());
    let skipped = review
        .selection()
        .filter(|candidate| candidate.size == Size::Skipped)
        .count();
    let unmeasured = unknown.saturating_sub(skipped);
    let mut line = if skipped == review.selected.len() {
        format!(
            "{} selected \u{b7} disk size skipped",
            review.selected.len()
        )
    } else {
        format!(
            "{} selected \u{b7} {}",
            review.selected.len(),
            render::human_bytes(bytes)
        )
    };
    if unmeasured > 0 {
        line.push_str(&format!(" \u{b7} {unmeasured} not measured"));
    }
    if skipped > 0 && skipped != review.selected.len() {
        line.push_str(&format!(" \u{b7} {skipped} size skipped"));
    }
    let (files, worktrees) = review.at_risk();
    if files > 0 {
        line.push_str(&format!(
            " \u{b7} {files} uncommitted files in {worktrees} of them"
        ));
    }
    line
}

fn mode_lines(review: &Review, width: usize) -> Vec<String> {
    let messages = || {
        review
            .undo_warnings
            .iter()
            .rev()
            .chain(review.messages.iter())
            .flat_map(|message| render::wrap(message, width))
            .collect::<Vec<_>>()
    };
    let status = |text: &str| {
        let mut lines = messages();
        lines.push(text.to_string());
        lines
    };
    match &review.mode {
        Mode::Preflighting => status("Refreshing selection\u{2026}"),
        Mode::ConfirmClean { count, bytes: _ } => {
            // Sizing continues behind the confirmation. Derive both figures
            // from the live inventory so a Pending row that settles cannot
            // leave the question pinned to its initial zero.
            let (bytes, unknown) = crate::shear::reclaimable(review.selection());
            let skipped = review
                .selection()
                .filter(|candidate| candidate.size == Size::Skipped)
                .count();
            let unmeasured = unknown.saturating_sub(skipped);
            let all_non_skipped_unmeasured =
                bytes == 0 && unmeasured > 0 && unmeasured + skipped == *count;
            let mut ask = if skipped == *count {
                format!(
                    "Remove {count} {}? Disk measurement was skipped.",
                    if *count == 1 { "worktree" } else { "worktrees" },
                )
            } else if all_non_skipped_unmeasured {
                format!(
                    "Remove {count} {}? Disk size is not measured yet.",
                    if *count == 1 { "worktree" } else { "worktrees" },
                )
            } else {
                format!(
                    "Remove {count} {} and reclaim {}?",
                    if *count == 1 { "worktree" } else { "worktrees" },
                    render::human_bytes(bytes),
                )
            };
            if unmeasured > 0 && !all_non_skipped_unmeasured {
                ask.push_str(&format!(
                    " ({unmeasured} of them was not measured, so the real figure is larger.)"
                ));
            }
            if skipped > 0 && skipped != *count {
                ask.push_str(&format!(
                    " Size measurement was skipped for {skipped} of them."
                ));
            }
            let mut lines = messages();
            lines.extend(render::wrap(&ask, width));
            lines.extend(render::wrap(
                "Press y or Enter to confirm, Esc to cancel.",
                width,
            ));
            lines
        }
        Mode::ConfirmDirty {
            files,
            typed,
            worktrees,
        } => {
            let mut lines = messages();
            lines.extend(render::wrap(
                &format!(
                    "{worktrees} of the {} selected {} uncommitted work: {files} {} that exist \
                     nowhere else. Removing the checkout destroys them; no branch and no commit \
                     is touched.",
                    review.selected.len(),
                    if *worktrees == 1 {
                        "worktrees has"
                    } else {
                        "worktrees have"
                    },
                    if *files == 1 { "file" } else { "files" },
                ),
                width,
            ));
            lines.extend(render::wrap(
                "This one cannot be answered with `y`. Type the number of files at risk, then \
                 Enter. Esc cancels.",
                width,
            ));
            lines.push(format!("files at risk: {files}    typed: {typed}_"));
            lines
        }
        Mode::Removing => status("Removing\u{2026}"),
        Mode::Rescanning => status("Rescanning\u{2026}"),
        Mode::Browsing | Mode::Done => messages(),
    }
}

// ---------------------------------------------------------------------------
// Key decoding
// ---------------------------------------------------------------------------

/// Decodes a raw read from the terminal into keys.
///
/// Pure, so `tests/tui.rs` can prove that an arrow key is not mistaken for Esc
/// and that a digit reaches the dirty confirmation intact.
pub fn decode(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        let key = match byte {
            0x1b => {
                // `Esc [ A` and friends. A lone Esc is a cancel, so the two must
                // never be confused: losing a selection to an arrow key would be
                // exactly the mis-key an overlay pane exists to survive.
                //
                // The rest of an unrecognized CSI or SS3 sequence is consumed,
                // never decoded byte by byte: F3 arrives as `Esc O R`, and a
                // trailing byte that happens to be a binding must not act.
                if bytes.get(index) == Some(&b'[') && index + 1 < bytes.len() {
                    index += 1;
                    // Parameter and intermediate bytes run to the first final
                    // byte (0x40..=0x7e), which ends the sequence.
                    let mut final_byte = 0u8;
                    while index < bytes.len() {
                        let candidate = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&candidate) {
                            final_byte = candidate;
                            break;
                        }
                    }
                    match final_byte {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        _ => Key::Other,
                    }
                } else if bytes.get(index) == Some(&b'O') && index + 1 < bytes.len() {
                    // SS3: one final byte follows.
                    index += 2;
                    Key::Other
                } else {
                    Key::Cancel
                }
            }
            b'k' => Key::Up,
            b'j' => Key::Down,
            b' ' => Key::Toggle,
            b'a' => Key::SelectSafe,
            b'n' => Key::SelectNone,
            b'r' => Key::Remove,
            b'R' => Key::Rescan,
            b'q' => Key::Quit,
            b'y' | b'\r' | b'\n' => Key::Confirm,
            0x08 | 0x7f => Key::Backspace,
            // Ctrl-C reaches us as a signal, not as a byte, because ISIG stays
            // on; this is the belt-and-braces path if a terminal sends it raw.
            0x03 => Key::Quit,
            b'0'..=b'9' => Key::Digit(byte - b'0'),
            _ => Key::Other,
        };
        keys.push(key);
    }
    keys
}

// ---------------------------------------------------------------------------
// The --review verb
// ---------------------------------------------------------------------------

const CLEAR_SCREEN: &str = "\u{1b}[H\u{1b}[2J";
const HIDE_CURSOR: &str = "\u{1b}[?25l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";
const RESET_ATTRS: &str = "\u{1b}[0m";

/// Rewrites a frame's line endings for a terminal in **raw mode**.
///
/// Raw mode turns off `ONLCR`, so a bare `\n` is a line feed and nothing else:
/// the cursor drops a row without returning to column 0, and every line starts
/// one column further right than the last until the frame walks off the side of
/// the pane. Found by running the review pane in a real herdr pane and looking
/// at it — the whole rendering suite passed throughout, because [`frame`] is
/// pure and every test reads its `\n`-joined output rather than what reaches a
/// terminal.
///
/// [`frame`] deliberately keeps joining with `\n` so it stays testable without a
/// terminal. The carriage returns are added here, at the single place that
/// writes to a real one.
pub fn for_raw_terminal(frame: &str) -> String {
    frame.replace('\n', "\r\n")
}

/// Messages printed after raw mode is restored.
#[doc(hidden)]
pub fn exit_messages(review: &Review) -> impl Iterator<Item = &str> {
    review
        .undo_warnings
        .iter()
        .chain(review.messages.iter())
        .map(String::as_str)
}

/// `--review`: the interactive verb.
pub fn run_review(config: &Config) -> Result<()> {
    let mut inventory = crate::shear::scan(config)?;
    // Last run's figures, drawn provisionally on the first frame while the
    // walk re-measures. Skipped when measurement is off: a pane that will
    // never replace the figure must not show one.
    if config.measure_disk {
        crate::disk::recall(&mut inventory, &crate::config::size_cache());
    }

    let stop = Arc::new(AtomicBool::new(false));
    register_stop_signals(&stop)?;

    // Raw mode is entered exactly once, and every way out of this function goes
    // through the guard's `Drop`.
    let guard = terminal::enter()?;
    let mut out = std::io::stdout();
    let _ = write!(out, "{HIDE_CURSOR}");
    let _ = out.flush();

    let result = event_loop(config, inventory, &stop, &mut out);

    let _ = write!(out, "{SHOW_CURSOR}{RESET_ATTRS}");
    let _ = out.flush();
    drop(guard);

    // What this run measured, for the next first frame. Written after the
    // terminal is restored: a slow disk must not sit between the user and
    // their prompt.
    if config.measure_disk {
        if let Ok(review) = result.as_ref() {
            crate::disk::remember(&review.inventory, &crate::config::size_cache());
        }
    }
    let review = result?;
    // Printed after the terminal is back the way we found it, so the outcome
    // survives the pane closing.
    let (columns, _) = render::terminal_size();
    for message in exit_messages(&review) {
        let mut line = String::new();
        render::push_wrapped(&mut line, "", "  ", message, columns.max(MIN_COLUMNS));
        print!("{line}");
    }
    Ok(())
}

fn event_loop(
    config: &Config,
    inventory: Inventory,
    stop: &AtomicBool,
    out: &mut impl Write,
) -> Result<Review> {
    let mut review = Review::new(inventory);
    let mut sizer = Sizer::start(&review.inventory, config);
    let mut dirty = true;

    loop {
        if stop.load(Ordering::Relaxed) {
            review.messages = vec!["Interrupted.".into()];
            return Ok(review);
        }
        if sizer.drain(&mut review.inventory) {
            dirty = true;
        }
        if dirty {
            let (columns, rows) = render::terminal_size();
            let frame = frame(&review, columns, rows.saturating_sub(1).max(1));
            out.write_all(CLEAR_SCREEN.as_bytes())?;
            out.write_all(for_raw_terminal(&frame).as_bytes())?;
            out.flush()?;
            dirty = false;
        }
        if review.is_finished() {
            return Ok(review);
        }
        if review.mode == Mode::Preflighting {
            let (next, adopted) = refresh_before_confirmation(review, config)?;
            review = next;
            if adopted {
                sizer = Sizer::start(&review.inventory, config);
            }
            dirty = true;
            continue;
        }
        if review.mode == Mode::Removing {
            review = perform(review, config);
            sizer = Sizer::start(&review.inventory, config);
            dirty = true;
            continue;
        }
        if review.mode == Mode::Rescanning {
            review = rescan(review, config);
            sizer = Sizer::start(&review.inventory, config);
            dirty = true;
            continue;
        }

        for key in read_keys()? {
            review = apply(review, key);
            dirty = true;
        }
    }
}

/// Stores one successful removal's persistent warning and transient success.
#[doc(hidden)]
pub fn append_removal_messages(review: &mut Review, outcome: crate::remove::RemovalOutcome) {
    if let Some(warning) = outcome.undo_warning {
        review.undo_warnings.push(warning);
    }
    review.messages.push(format!(
        "removed {} \u{2014} restore it with: {}",
        outcome.record.path, outcome.record.restore_command
    ));
}

/// Stores a route failure without losing an undo warning emitted before it.
#[doc(hidden)]
pub fn append_removal_failure(
    review: &mut Review,
    path: &std::path::Path,
    failure: crate::remove::RemovalFailure,
) {
    if let Some(warning) = failure.undo_warning {
        review.undo_warnings.push(warning);
    }
    review
        .messages
        .push(format!("{}: {}", path.display(), failure.message));
}

/// Carries out the removals the user has confirmed.
///
/// The only way into this function is [`Mode::Removing`], and the only way into
/// that mode is through confirmations built by [`preflight`] from a fresh scan.
/// Every guard in `remove::check` still runs underneath: this is the last of
/// several gates, not the only one.
fn perform(mut review: Review, config: &Config) -> Review {
    review.messages.clear();
    let mut herdr = crate::herdr::Herdr::connect().ok();
    let selected: Vec<PathBuf> = review
        .selection()
        .map(|candidate| candidate.worktree.path.clone())
        .collect();

    for path in &selected {
        let Some(candidate) = review.inventory.find(path) else {
            review
                .messages
                .push(format!("{}: no longer in the inventory", path.display()));
            continue;
        };
        let permissions = crate::remove::Permissions {
            // Dirty rows in the selection have already been paid for, once with
            // the clean confirmation and once by typing the file count.
            force_dirty: candidate.dirt.is_dirty(),
            acknowledged_files: candidate.dirt.is_dirty().then_some(candidate.dirt.total()),
            // The pane never closes somebody's workspace. A worktree held open
            // is Blocked, and the refusal says which workspace to close.
            close_workspace: false,
        };
        match crate::remove::remove_one(candidate, permissions, herdr.as_mut(), config) {
            Ok(outcome) => append_removal_messages(&mut review, outcome),
            Err(failure) => append_removal_failure(&mut review, path, failure),
        }
    }

    match crate::shear::scan(config) {
        Ok(inventory) => review.inventory = inventory,
        Err(err) => review
            .messages
            .push(format!("could not rescan after removing: {err}")),
    }
    review.selected.clear();
    // Indices mean nothing across a rescan, so the cursor goes back to the top
    // of the table rather than to whatever row inherited its number.
    review.cursor = display_order(&review.inventory)
        .first()
        .copied()
        .unwrap_or(0);
    review.mode = Mode::Browsing;
    review
}

/// Runs the pre-confirmation scan. The boolean tells the event loop whether a
/// fresh inventory was adopted and its background sizer must restart.
fn refresh_before_confirmation(review: Review, config: &Config) -> Result<(Review, bool)> {
    let scanned = crate::shear::scan(config).map_err(|err| err.to_string());
    finish_preflight(review, scanned, terminal::discard_input)
}

fn finish_preflight<E: std::fmt::Display>(
    review: Review,
    scanned: std::result::Result<Inventory, E>,
    discard_input: impl FnOnce() -> Result<()>,
) -> Result<(Review, bool)> {
    // A scan can block long enough for reflexive Enter presses to accumulate.
    // Drop everything typed while it was in flight before either the fresh
    // confirmation or the failure message can accept another key.
    discard_input()?;
    let adopted = scanned.is_ok();
    Ok((preflight(review, scanned), adopted))
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

#[cfg(test)]
mod preflight_driver_tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn queued_input_is_discarded_after_success_and_failure() {
        for scanned in [
            Ok::<Inventory, &str>(Inventory::default()),
            Err("scan failed"),
        ] {
            let discarded = Cell::new(false);
            let review = Review::new(Inventory::default());
            finish_preflight(review, scanned, || {
                discarded.set(true);
                Ok(())
            })
            .unwrap();
            assert!(discarded.get());
        }
    }
}

/// Re-reads git and herdr without touching anything, keeping the selection.
///
/// A scan that fails leaves the pane exactly as it was: the previous
/// inventory is still true of the world as last observed, and a rescan that
/// destroyed a built-up selection on a transient error would make `R` a key
/// nobody dares press.
fn rescan(mut review: Review, config: &Config) -> Review {
    match crate::shear::scan(config) {
        Ok(inventory) => adopt(review, inventory),
        Err(err) => {
            review.mode = Mode::Browsing;
            review.messages = vec![format!(
                "could not rescan ({err}); showing the previous scan"
            )];
            review
        }
    }
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

/// One read from the terminal, decoded. Empty when the read timed out, which is
/// how the loop stays responsive to the sizing thread and to signals.
fn read_keys() -> Result<Vec<Key>> {
    use std::io::Read;

    let mut buffer = [0u8; 32];
    match std::io::stdin().read(&mut buffer) {
        Ok(0) => Ok(Vec::new()),
        Ok(read) => Ok(decode(&buffer[..read])),
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

// ---------------------------------------------------------------------------
// Background sizing
// ---------------------------------------------------------------------------

/// Disk sizes filled in behind the rendering.
///
/// The first frame is drawn from a scan that measured nothing, so the pane
/// appears immediately and the sizes arrive as they are counted. The cancel flag
/// is what lets a teardown skip a slow filesystem instead of waiting for it.
struct Sizer {
    results: Option<Receiver<(PathBuf, Size)>>,
    cancel: Arc<AtomicBool>,
}

impl Sizer {
    fn start(inventory: &Inventory, config: &Config) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        if !config.measure_disk {
            return Self {
                results: None,
                cancel,
            };
        }
        let paths: Vec<PathBuf> = inventory
            .candidates
            .iter()
            // Provisional rows are re-measured too: the figure on screen is
            // last run's, drawn while the walk replaces it.
            .filter(|candidate| matches!(candidate.size, Size::Pending | Size::Provisional(_)))
            .map(|candidate| candidate.worktree.path.clone())
            .collect();
        if paths.is_empty() {
            return Self {
                results: None,
                cancel,
            };
        }

        let (sender, results) = mpsc::channel();
        let thread_cancel = Arc::clone(&cancel);
        std::thread::spawn(move || {
            for path in paths {
                if thread_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let size = crate::disk::measure(&path, &thread_cancel);
                if sender.send((path, size)).is_err() {
                    return;
                }
            }
        });
        Self {
            results: Some(results),
            cancel,
        }
    }

    /// Writes whatever has been measured so far into the inventory. Returns
    /// whether anything changed, so the pane only redraws when it must.
    fn drain(&mut self, inventory: &mut Inventory) -> bool {
        let Some(results) = self.results.as_ref() else {
            return false;
        };
        let mut changed = false;
        loop {
            match results.try_recv() {
                Ok((path, size)) => {
                    for candidate in inventory.candidates.iter_mut() {
                        if candidate.worktree.path == path {
                            candidate.size = size;
                            changed = true;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.results = None;
                    break;
                }
            }
        }
        changed
    }
}

impl Drop for Sizer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(unix)]
fn register_stop_signals(stop: &Arc<AtomicBool>) -> Result<()> {
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(stop))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn register_stop_signals(_stop: &Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw mode
// ---------------------------------------------------------------------------

/// Raw mode, and the three ways out of it.
///
/// The saved `termios` lives behind an `AtomicPtr` rather than a `Mutex` because
/// the signal handler reads it: taking a lock in a signal handler is not
/// async-signal-safe, and `tcsetattr` is.
#[cfg(unix)]
mod terminal {
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
    use std::sync::Once;

    static SAVED: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());
    static FD: AtomicI32 = AtomicI32::new(-1);
    static HOOKS: Once = Once::new();

    /// Live raw mode. Dropping it restores the terminal.
    pub struct RawMode(());

    impl Drop for RawMode {
        fn drop(&mut self) {
            restore();
        }
    }

    pub fn enter() -> crate::Result<RawMode> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `fd` is stdin's descriptor, valid for the life of the process.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(
                "the review pane needs a terminal on stdin; use --list or --json when \
                        there is not one"
                    .into(),
            );
        }

        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `original` is a correctly sized, owned `termios`.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        let mut raw = original;
        // SAFETY: `raw` is a correctly sized, owned `termios`.
        unsafe { libc::cfmakeraw(&mut raw) };
        // ISIG stays on so Ctrl-C is still a signal. The handler below restores
        // the terminal before the process goes anywhere.
        raw.c_lflag |= libc::ISIG;
        // Reads return after 100ms with nothing, which is what lets the event
        // loop notice a signal and pick up freshly measured sizes.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // SAFETY: `raw` is a correctly sized, owned `termios`.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        FD.store(fd, Ordering::SeqCst);
        // Leaked on purpose and never freed: the signal handler may read it at
        // any moment, and freeing it would be the one bug that leaves a pane in
        // raw mode.
        SAVED.store(Box::into_raw(Box::new(original)), Ordering::SeqCst);
        install_hooks();
        Ok(RawMode(()))
    }

    /// Discards bytes typed while a blocking operation was in flight.
    pub fn discard_input() -> crate::Result<()> {
        let fd = FD.load(Ordering::SeqCst);
        if fd < 0 {
            return Ok(());
        }
        // SAFETY: `fd` is the live stdin descriptor published by `enter`.
        if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Puts the terminal back. Idempotent, and safe to call from a signal
    /// handler: two atomic loads and one `tcsetattr`, all async-signal-safe.
    pub fn restore() {
        let fd = FD.load(Ordering::SeqCst);
        let saved = SAVED.load(Ordering::SeqCst);
        if fd < 0 || saved.is_null() {
            return;
        }
        // SAFETY: `saved` points at a leaked, never-freed `termios` written
        // before it was published.
        unsafe {
            libc::tcsetattr(fd, libc::TCSAFLUSH, saved);
        }
    }

    fn install_hooks() {
        HOOKS.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));

            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ] {
                // SAFETY: the handler calls only async-signal-safe functions.
                // The event loop's own flag handles the orderly exit; this is
                // what makes the terminal survive a kill that outruns it.
                let _ = unsafe { signal_hook::low_level::register(signal, restore) };
            }
        });
    }
}

#[cfg(not(unix))]
mod terminal {
    pub struct RawMode(());

    pub fn enter() -> crate::Result<RawMode> {
        Err("the review pane is unix-only; use --list or --json".into())
    }

    pub fn discard_input() -> crate::Result<()> {
        Ok(())
    }
}
