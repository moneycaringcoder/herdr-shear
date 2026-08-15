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
}

/// What the pane is currently asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
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
        && candidate.worktree.locked.is_none()
        && candidate.open_workspace.is_none()
        && !candidate.dirt.is_dirty()
}

/// Applies one key. Never performs I/O; a transition into [`Mode::Removing`] is
/// what tells the driver to act.
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
        // The driver owns these two: one is a removal in flight, the other is a
        // pane that has already said its last word.
        Mode::Removing | Mode::Done => review,
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
            let blocked = candidate.verdict == Verdict::Blocked;
            let reason = candidate.reason.clone();
            if review.selected.remove(&review.cursor) {
                return review;
            }
            review.selected.insert(review.cursor);
            if blocked {
                review.messages.push(format!(
                    "Selected, but it is blocked and the removal will be refused: {reason}"
                ));
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
            let (bytes, _) = crate::shear::reclaimable(review.selection());
            review.mode = Mode::ConfirmClean {
                count: review.selected.len(),
                bytes,
            };
        }
        Key::Quit => {
            review.messages.clear();
            review.mode = Mode::Done;
            review.messages.push("Quit. Nothing was removed.".into());
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
const HELP_WIDE: &str =
    "\u{2191}/k up  \u{2193}/j down  space select  a safe rows  n none  r remove  q quit";
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
    if let Some(detail) = detail_line(review) {
        blocks.push(render::wrap(&detail, width));
    }
    blocks.push(vec![selection_line(review)]);
    blocks.push(mode_lines(review, width));
    // The sentence that makes the action feel safe, where it can be read while
    // the user is deciding. It is never the block that gets dropped.
    blocks.push(render::wrap(render::SAFETY_NOTE, width));
    blocks.push(vec![
        if width >= 72 { HELP_WIDE } else { HELP_NARROW }.to_string()
    ]);

    // Trim from the least useful block up when the pane is short: the help line
    // first, then the cursor detail, then the selection line. The safety note
    // and whatever the pane is currently asking always survive.
    let droppable = [5usize, 1, 2, 0];
    let mut dropped: BTreeSet<usize> = BTreeSet::new();
    let total = |dropped: &BTreeSet<usize>, blocks: &[Vec<String>]| {
        blocks
            .iter()
            .enumerate()
            .filter(|(index, _)| !dropped.contains(index))
            .map(|(_, block)| block.len())
            .sum::<usize>()
    };
    for index in droppable {
        if total(&dropped, &blocks) <= budget {
            break;
        }
        if index < blocks.len() {
            dropped.insert(index);
        }
    }

    blocks
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !dropped.contains(index))
        .flat_map(|(_, block)| block)
        .collect()
}

fn detail_line(review: &Review) -> Option<String> {
    let candidate = review.inventory.candidates.get(review.cursor)?;
    let name = candidate
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| candidate.path().to_string_lossy().into_owned());
    let reason = if candidate.reason.trim().is_empty() {
        render::classes_cell(candidate, usize::MAX)
    } else {
        candidate.reason.clone()
    };
    if reason.is_empty() {
        Some(format!("{name}: {}", candidate.verdict.label()))
    } else {
        Some(format!("{name}: {reason}"))
    }
}

fn selection_line(review: &Review) -> String {
    if review.selected.is_empty() {
        return "nothing selected".to_string();
    }
    let (bytes, unknown) = crate::shear::reclaimable(review.selection());
    let mut line = format!(
        "{} selected \u{b7} {}",
        review.selected.len(),
        render::human_bytes(bytes)
    );
    if unknown > 0 {
        line.push_str(&format!(" \u{b7} {unknown} not measured"));
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
    match &review.mode {
        Mode::ConfirmClean { count, bytes } => {
            let (_, unknown) = crate::shear::reclaimable(review.selection());
            let mut ask = format!(
                "Remove {count} {} and reclaim {}?",
                if *count == 1 { "worktree" } else { "worktrees" },
                render::human_bytes(*bytes),
            );
            if unknown > 0 {
                ask.push_str(&format!(
                    " ({unknown} of them was not measured, so the real figure is larger.)"
                ));
            }
            let mut lines = render::wrap(&ask, width);
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
            let mut lines = render::wrap(
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
            );
            lines.extend(render::wrap(
                "This one cannot be answered with `y`. Type the number of files at risk, then \
                 Enter. Esc cancels.",
                width,
            ));
            lines.push(format!("files at risk: {files}    typed: {typed}_"));
            lines
        }
        Mode::Removing => vec!["Removing\u{2026}".to_string()],
        Mode::Browsing | Mode::Done => review
            .messages
            .iter()
            .flat_map(|message| render::wrap(message, width))
            .collect(),
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
                if bytes.get(index) == Some(&b'[') && index + 1 < bytes.len() {
                    let final_byte = bytes[index + 1];
                    index += 2;
                    match final_byte {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        _ => Key::Other,
                    }
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

/// `--review`: the interactive verb.
pub fn run_review(config: &Config) -> Result<()> {
    let inventory = crate::shear::scan(config)?;

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

    let review = result?;
    // Printed after the terminal is back the way we found it, so the outcome
    // survives the pane closing.
    let (columns, _) = render::terminal_size();
    for message in &review.messages {
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
            review.messages = vec!["Interrupted. Nothing was removed.".into()];
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
        if review.mode == Mode::Removing {
            review = perform(review, config);
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

/// Carries out the removals the user has confirmed.
///
/// The only way into this function is [`Mode::Removing`], and the only way into
/// that mode is through the confirmations in [`apply`]. Every guard in
/// `remove::check` still runs underneath: this is the last of several gates, not
/// the only one.
fn perform(mut review: Review, config: &Config) -> Review {
    let mut messages: Vec<String> = Vec::new();
    let mut herdr = crate::herdr::Herdr::connect().ok();
    let selected: Vec<PathBuf> = review
        .selection()
        .map(|candidate| candidate.worktree.path.clone())
        .collect();

    for path in &selected {
        let Some(candidate) = review.inventory.find(path) else {
            messages.push(format!("{}: no longer in the inventory", path.display()));
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
            Ok(record) => messages.push(format!(
                "removed {} \u{2014} restore it with: {}",
                record.path, record.restore_command
            )),
            Err(err) => messages.push(format!("{}: {err}", path.display())),
        }
    }

    match crate::shear::scan(config) {
        Ok(inventory) => review.inventory = inventory,
        Err(err) => messages.push(format!("could not rescan after removing: {err}")),
    }
    review.selected.clear();
    // Indices mean nothing across a rescan, so the cursor goes back to the top
    // of the table rather than to whatever row inherited its number.
    review.cursor = display_order(&review.inventory)
        .first()
        .copied()
        .unwrap_or(0);
    review.mode = Mode::Browsing;
    review.messages = messages;
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
            .filter(|candidate| candidate.size == Size::Pending)
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
}
