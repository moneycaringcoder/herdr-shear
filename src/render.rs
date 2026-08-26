//! Rendering. Pure functions from an [`Inventory`] to text.
//!
//! Nothing here does I/O except [`run_list`], which is the `--list` verb. That
//! split is what lets `tests/render.rs` pin the table's alignment and truncation
//! without a repository or a session.
//!
//! Cosmetic output is a feature here, not polish. A janitor is judged on whether
//! its columns line up and its paths are readable at 80 columns, because that is
//! the whole interface.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::model::{Candidate, Head, Inventory, Merged, RepoKey, Size, Verdict};
use crate::shear::reclaimable;
use crate::Result;

/// Width assumed when the real terminal width is unknown.
pub const DEFAULT_COLUMNS: usize = 100;
/// Below this the table stops trying to stay pretty, but it still never emits a
/// line wider than the width it was given.
pub const MIN_COLUMNS: usize = 40;

const ELLIPSIS: char = '\u{2026}'; // …
const DOT_LEADER: char = '\u{2026}'; // … — a pending measurement, not a number

/// The one sentence that makes the action feel safe, and that is true. Shown
/// under the table and inside the review pane, where the user can read it while
/// they are deciding.
pub const SAFETY_NOTE: &str = "Removing a worktree leaves its branch and every commit on it \
     intact: only the checkout goes.";

/// Token shown in the classes column for [`Merged::Unknown`].
///
/// It is deliberately its own token rather than the absence of `merged`: "I
/// asked and the answer is no" and "I could not ask" are different facts, and
/// the table renders three states because the model carries three.
pub const MERGE_UNKNOWN: &str = "merged?";

const MERGE_UNKNOWN_LEGEND: &str = "merged? — shear could not ask whether that worktree's commit \
     is contained in the integration ref: no integration ref resolved in the repository, or the \
     worktree has no commit to test. That is not the same as `not merged`. A row carrying \
     `merged` was tested and is contained; a row carrying neither was tested and is not; nothing \
     carrying `merged?` is ever called safe.";

/// Column headings. Each column is at least as wide as its heading, so a table
/// of short values is still readable.
const HEAD_VERDICT: &str = "verdict";
const HEAD_CLASSES: &str = "classes";
const HEAD_AGE: &str = "age";
const HEAD_SIZE: &str = "disk";
const HEAD_BRANCH: &str = "branch";
const HEAD_PATH: &str = "path";

/// Marker in the selection column. One display column, so a table rendered with
/// nothing selected lines up with the same table rendered with everything
/// selected.
pub const MARK_SELECTED: char = '*';
pub const MARK_UNSELECTED: char = ' ';

/// The path column is the last thing to be squeezed and the last thing to be
/// dropped: a row whose path cannot be read is not a row.
const PATH_MIN: usize = 12;
const CLASSES_MAX: usize = 20;
const CLASSES_MIN: usize = 5;
const BRANCH_MAX: usize = 24;
const BRANCH_MIN: usize = 8;
/// `<1h`, `23h`, `13d`, `7w`, `11mo`, `99y+` — never wider than this.
const AGE_COLUMNS: usize = 4;

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

/// The full review table, grouped by repository.
///
/// Columns: selection marker, verdict, classes, age, disk, branch, and the
/// worktree path. Paths truncate from the **left**, because the tail is the
/// informative half; branches and labels truncate from the right.
pub fn table(inventory: &Inventory, columns: usize) -> String {
    let width = columns.max(MIN_COLUMNS);
    let widths = widths_for(inventory, width);
    let mut out = String::new();

    if inventory.candidates.is_empty() {
        push_line(&mut out, "no worktrees found.", width);
        push_notes(&mut out, &inventory.notes, width);
        return out;
    }

    push_line(&mut out, &header(&widths), width);
    for (key, group) in grouped(inventory) {
        out.push('\n');
        push_line(
            &mut out,
            &repo_heading(inventory, &key, group[0], width),
            width,
        );
        for candidate in group {
            push_line(&mut out, &row(candidate, &widths, false), width);
            for note in &candidate.worktree.notes {
                push_wrapped(&mut out, "      ", "        ", note, width);
            }
        }
    }

    if inventory.candidates.iter().any(merge_unknown) {
        out.push('\n');
        push_wrapped(&mut out, "", "  ", MERGE_UNKNOWN_LEGEND, width);
    }
    push_notes(&mut out, &inventory.notes, width);
    out
}

/// The heading row, at the widths the table computed.
pub fn header(widths: &Widths) -> String {
    let mut line = String::new();
    line.push(MARK_UNSELECTED);
    push_cell(&mut line, HEAD_VERDICT, widths.verdict, Align::Left);
    push_cell(&mut line, HEAD_CLASSES, widths.classes, Align::Left);
    push_cell(&mut line, HEAD_AGE, widths.age, Align::Right);
    push_cell(&mut line, HEAD_SIZE, widths.size, Align::Right);
    push_cell(&mut line, HEAD_BRANCH, widths.branch, Align::Left);
    push_cell(&mut line, HEAD_PATH, widths.path, Align::Left);
    line.trim_end().to_string()
}

/// One row of the table, at the column widths the table computed.
pub fn row(candidate: &Candidate, widths: &Widths, selected: bool) -> String {
    let mut line = String::new();
    line.push(if selected {
        MARK_SELECTED
    } else {
        MARK_UNSELECTED
    });
    push_cell(
        &mut line,
        candidate.verdict.label(),
        widths.verdict,
        Align::Left,
    );
    push_cell(
        &mut line,
        &classes_cell(candidate, widths.classes),
        widths.classes,
        Align::Left,
    );
    push_cell(&mut line, &age_cell(candidate), widths.age, Align::Right);
    push_cell(
        &mut line,
        &size_cell(candidate.size),
        widths.size,
        Align::Right,
    );
    push_cell(
        &mut line,
        &branch_cell(candidate),
        widths.branch,
        Align::Left,
    );
    // The path is last and truncates from the LEFT, so the tail — the part that
    // says which worktree this is — always survives.
    if widths.path > 0 {
        line.push(' ');
        line.push_str(&truncate_left(
            &candidate.path().to_string_lossy(),
            widths.path,
        ));
    }
    line.trim_end().to_string()
}

/// Column widths, computed from the content so a session of short paths does not
/// get a table sized for long ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Widths {
    pub verdict: usize,
    pub classes: usize,
    pub age: usize,
    pub size: usize,
    pub branch: usize,
    pub path: usize,
}

impl Widths {
    /// Display columns one row occupies at these widths, marker included.
    pub fn total(&self) -> usize {
        let mut total = 1;
        for width in [
            self.verdict,
            self.classes,
            self.age,
            self.size,
            self.branch,
            self.path,
        ] {
            if width > 0 {
                total += 1 + width;
            }
        }
        total
    }
}

/// Widths for one inventory at a given terminal width.
///
/// Everything except the marker, the verdict and the path is optional. When the
/// terminal cannot hold them all they are dropped in reverse order of how much
/// they justify a decision — branch first, then classes, then age, then disk —
/// rather than every column being squeezed until none of them can be read.
pub fn widths_for(inventory: &Inventory, columns: usize) -> Widths {
    let width = columns.max(MIN_COLUMNS);
    let rows = &inventory.candidates;

    let verdict = rows
        .iter()
        .map(|c| display_width(c.verdict.label()))
        .max()
        .unwrap_or(0)
        .max(display_width(HEAD_VERDICT));
    let classes_desired = rows
        .iter()
        .map(|c| display_width(&classes_cell(c, usize::MAX)))
        .max()
        .unwrap_or(0)
        .max(display_width(HEAD_CLASSES))
        .min(CLASSES_MAX);
    let age_desired = AGE_COLUMNS.max(display_width(HEAD_AGE));
    let size_desired = rows
        .iter()
        .map(|c| display_width(&size_cell(c.size)))
        .max()
        .unwrap_or(0)
        .max(display_width(HEAD_SIZE));
    let branch_desired = rows
        .iter()
        .map(|c| display_width(&branch_cell(c)))
        .max()
        .unwrap_or(0)
        .max(display_width(HEAD_BRANCH))
        // A branch name never gets so much of a narrow pane that the path — the
        // column that says *which* worktree this is — ends up unreadable.
        .min(BRANCH_MAX.min((width / 5).max(BRANCH_MIN)));
    let path_desired = rows
        .iter()
        .map(|c| display_width(&c.path().to_string_lossy()))
        .max()
        .unwrap_or(0)
        .max(display_width(HEAD_PATH));

    let mut widths = Widths {
        verdict,
        ..Widths::default()
    };
    // The marker, its separator, the verdict and a readable path are the floor.
    let mut spare = width
        .saturating_sub(1 + 1 + verdict)
        .saturating_sub(1 + PATH_MIN);

    // (desired, minimum). A minimum equal to the desired width means the column
    // is all-or-nothing: a right-aligned number that has been truncated is a
    // different number, and a two-letter age is not an age.
    let optional: [(&mut usize, usize, usize); 4] = [
        (&mut widths.size, size_desired, size_desired),
        (&mut widths.age, age_desired, age_desired),
        (&mut widths.classes, classes_desired, CLASSES_MIN),
        (&mut widths.branch, branch_desired, BRANCH_MIN),
    ];
    for (slot, desired, minimum) in optional {
        // `spare` has to cover the column *and* the space in front of it, hence
        // the comparison against the width alone rather than width + separator.
        let take = if spare > desired {
            desired
        } else if spare > minimum {
            spare - 1
        } else {
            0
        };
        if take > 0 {
            *slot = take;
            spare -= 1 + take;
        }
    }

    widths.path = (PATH_MIN + spare).min(path_desired.max(PATH_MIN));
    widths
}

/// The summary line under the table: how many worktrees, how many safe, and what
/// the total reclaimable space is.
///
/// Both halves of the disk question are answered — per row and as a total —
/// because the per-row number is what justifies a particular pick and the total
/// is what makes anyone bother.
pub fn summary(inventory: &Inventory, columns: usize) -> String {
    let width = columns.max(MIN_COLUMNS);
    let mut out = String::new();
    let rows = &inventory.candidates;

    let repos: BTreeSet<&RepoKey> = rows.iter().map(|c| &c.worktree.repo).collect();
    let count = |verdict: Verdict| rows.iter().filter(|c| c.verdict == verdict).count();
    let counts = format!(
        "{}, {} review, {} keep, {} blocked.",
        plural(count(Verdict::Safe), "safe worktree", "safe worktrees"),
        count(Verdict::Review),
        count(Verdict::Keep),
        count(Verdict::Blocked),
    );
    push_wrapped(
        &mut out,
        "",
        "",
        &format!(
            "{} in {}: {counts}",
            plural(rows.len(), "worktree", "worktrees"),
            plural(repos.len(), "repository", "repositories"),
        ),
        width,
    );

    let (safe_bytes, safe_unknown) = reclaimable(inventory.safe());
    let (total_bytes, total_unknown) = reclaimable(rows.iter());
    let safe_rows = inventory.safe().count();
    let disk = if safe_rows == 0 {
        format!(
            "Nothing here is safe to remove without review. All {} occupy {}.",
            plural(rows.len(), "worktree", "worktrees"),
            human_bytes(total_bytes),
        )
    } else {
        format!(
            "Removing the {} would reclaim {} of the {} all {} occupy.",
            plural(safe_rows, "safe worktree", "safe worktrees"),
            human_bytes(safe_bytes),
            human_bytes(total_bytes),
            plural(rows.len(), "worktree", "worktrees"),
        )
    };
    push_wrapped(&mut out, "", "", &disk, width);

    // A total that quietly omits the rows it could not measure is a total that
    // undercounts, so say how many rather than letting the number lie.
    if total_unknown > 0 {
        let unmeasured = if safe_unknown > 0 && safe_rows > 0 {
            format!(
                "{} could not be measured ({} of them safe), so both figures are floors, not \
                 estimates.",
                plural(total_unknown, "worktree", "worktrees"),
                safe_unknown,
            )
        } else {
            format!(
                "{} could not be measured, so that figure is a floor, not an estimate.",
                plural(total_unknown, "worktree", "worktrees"),
            )
        };
        push_wrapped(&mut out, "", "", &unmeasured, width);
    }

    // Use the same candidate-derived repository count the sentence above just
    // reported; registered repositories with no worktrees do not belong here.
    if repos.len() >= 2 {
        let mut by_repo: Vec<_> = grouped(inventory)
            .into_iter()
            .filter_map(|(key, group)| {
                let first = group.first().copied()?;
                let (name, root) = repo_identity(inventory, &key, first);
                let safe_rows = group
                    .iter()
                    .filter(|candidate| candidate.verdict == Verdict::Safe)
                    .count();
                let (safe_bytes, safe_unknown) = reclaimable(
                    group
                        .iter()
                        .copied()
                        .filter(|candidate| candidate.verdict == Verdict::Safe),
                );
                let (total_bytes, total_unknown) = reclaimable(group.iter().copied());
                Some((
                    root,
                    name,
                    group.len(),
                    safe_rows,
                    safe_bytes,
                    safe_unknown,
                    total_bytes,
                    total_unknown,
                ))
            })
            .collect();
        by_repo.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| a.0.cmp(b.0)));

        for (_, name, row_count, safe_rows, safe_bytes, safe_unknown, total_bytes, total_unknown) in
            by_repo
        {
            let safe_figure = format!(
                "{}{}",
                human_bytes(safe_bytes),
                if safe_unknown > 0 { "+" } else { "" }
            );
            let total_figure = format!(
                "{}{}",
                human_bytes(total_bytes),
                if total_unknown > 0 { "+" } else { "" }
            );
            let unmeasured = if total_unknown > 0 {
                format!(" ({total_unknown} unmeasured)")
            } else {
                String::new()
            };
            let details = if safe_rows == 0 {
                format!(
                    ": {}, {safe_rows} safe · nothing safe to remove · {total_figure} total{unmeasured}",
                    plural(row_count, "worktree", "worktrees"),
                )
            } else {
                format!(
                    ": {}, {safe_rows} safe · {safe_figure} reclaimable of {total_figure}{unmeasured}",
                    plural(row_count, "worktree", "worktrees"),
                )
            };
            let available_name_width = width
                .saturating_sub(display_width("  "))
                .saturating_sub(display_width(&details));
            let display_name = if available_name_width > 0 {
                truncate_right(&name, available_name_width)
            } else {
                name.to_string()
            };
            push_wrapped(
                &mut out,
                "  ",
                "    ",
                &format!("{display_name}{details}"),
                width,
            );
        }
    }

    push_wrapped(&mut out, "", "", SAFETY_NOTE, width);
    out
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

/// The classes cell: every reason this worktree looks dead, most alarming
/// first, plus [`MERGE_UNKNOWN`] when the merge question could not be asked.
///
/// When the labels do not fit, whole labels are kept and the rest are counted
/// (`merged +2`) rather than one long string being cut mid-word, because half a
/// label reads as a different label.
pub fn classes_cell(candidate: &Candidate, max: usize) -> String {
    let mut tokens: Vec<&str> = candidate.classes.iter().map(|c| c.label()).collect();
    if merge_unknown(candidate) {
        tokens.push(MERGE_UNKNOWN);
    }
    if tokens.is_empty() {
        return String::new();
    }
    fit_tokens(&tokens, max)
}

fn fit_tokens(tokens: &[&str], max: usize) -> String {
    let all = tokens.join(",");
    if display_width(&all) <= max {
        return all;
    }
    let mut kept: Vec<&str> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let mut candidate = kept.join(",");
        if !candidate.is_empty() {
            candidate.push(',');
        }
        candidate.push_str(token);
        let omitted = tokens.len() - index - 1;
        let suffix = if omitted > 0 {
            format!(" +{omitted}")
        } else {
            String::new()
        };
        if display_width(&candidate) + display_width(&suffix) > max {
            break;
        }
        kept.push(token);
    }
    if kept.is_empty() {
        // Not even one whole label fits. Truncating is the honest last resort:
        // the row still says *something*, and the pane's detail line says the
        // rest.
        return truncate_right(&all, max);
    }
    let omitted = tokens.len() - kept.len();
    if omitted == 0 {
        kept.join(",")
    } else {
        format!("{} +{omitted}", kept.join(","))
    }
}

/// Whether this row's merge question went unanswered. [`Merged::Unknown`] is
/// "we could not ask", and is never rendered as "not merged".
pub fn merge_unknown(candidate: &Candidate) -> bool {
    candidate.merged == Merged::Unknown
}

/// The branch cell. A worktree without a branch says which kind of headless it
/// is, because a detached HEAD and a deleted branch call for different actions.
pub fn branch_cell(candidate: &Candidate) -> String {
    match &candidate.worktree.head {
        Head::Branch(name) => name.clone(),
        Head::Detached => "(detached)".to_string(),
        Head::Unborn => "(no branch)".to_string(),
        Head::Bare => "(bare)".to_string(),
    }
}

fn age_cell(candidate: &Candidate) -> String {
    human_age(candidate.last_commit.map(|tip| {
        SystemTime::now()
            .duration_since(tip)
            .unwrap_or(Duration::ZERO)
    }))
}

/// The disk cell.
///
/// The three non-numbers are deliberately distinct and none of them is a zero:
/// a dot leader while the measurement is still running, a dash for a checkout
/// that is not on disk at all, and `?` when the walk failed. A failed
/// measurement rendered as `0 B` would be a lie in the one column the user is
/// about to make a decision on.
///
/// [`crate::disk::human`] prints the same vocabulary for one-off messages; the
/// table keeps its own copy so `tests/render.rs` can pin every cell without the
/// disk layer being present.
pub fn size_cell(size: Size) -> String {
    match size {
        Size::Pending => DOT_LEADER.to_string(),
        // Marked with a tilde: last run's figure, drawn while the walk
        // re-measures, and never presented as a measurement.
        Size::Provisional(bytes) => format!("~{}", human_bytes(bytes)),
        Size::Gone => "-".to_string(),
        Size::Failed => "?".to_string(),
        Size::Bytes(bytes) => human_bytes(bytes),
    }
}

/// Bytes as `du -h` would print them: powers of 1024, SI-style suffixes, and
/// truncated rather than rounded so the figure never overstates what is there.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        // Truncate the tenth rather than round it: 1.29 GB reads as 1.2 GB.
        format!(
            "{}.{} {}",
            value as u64,
            (value * 10.0) as u64 % 10,
            UNITS[unit]
        )
    } else {
        format!("{} {}", value as u64, UNITS[unit])
    }
}

/// Compact age, never wider than four display columns: `3d`, `12d`, `4w`, `7mo`,
/// `2y`, `-` when there is no commit to date.
pub fn human_age(age: Option<std::time::Duration>) -> String {
    let Some(age) = age else {
        return "-".to_string();
    };
    let seconds = age.as_secs();
    let days = seconds / 86_400;
    match (seconds, days) {
        (0..=3_599, _) => "<1h".to_string(),
        (_, 0) => format!("{}h", seconds / 3_600),
        (_, 1..=13) => format!("{days}d"),
        (_, 14..=55) => format!("{}w", days / 7),
        // Months from the year, not from a 30-day month: `days / 30` calls 364
        // days "12mo", which is a year by any other name. Rounded to the nearest
        // month and capped at eleven, so the last month before a year still
        // reads as months.
        (_, 56..=364) => format!("{}mo", ((days * 12 + 182) / 365).clamp(1, 11)),
        (_, _) => {
            let years = days / 365;
            if years > 99 {
                "99y+".to_string()
            } else {
                format!("{years}y")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

/// Candidates grouped by repository, in the order the repos were discovered,
/// with any repository the inventory did not name trailing in the order its
/// worktrees appeared. Grouping never merges two repos: a `RepoKey` is the whole
/// identity.
pub fn grouped(inventory: &Inventory) -> Vec<(RepoKey, Vec<&Candidate>)> {
    grouped_indices(inventory)
        .into_iter()
        .map(|(key, group)| {
            let group = group
                .into_iter()
                .filter_map(|index| inventory.candidates.get(index))
                .collect();
            (key, group)
        })
        .collect()
}

/// [`grouped`], as indices into `inventory.candidates`. The review pane needs
/// the index to know what the cursor is on, and both views must agree on the
/// order.
pub fn grouped_indices(inventory: &Inventory) -> Vec<(RepoKey, Vec<usize>)> {
    let mut order: Vec<RepoKey> = inventory.repos.iter().map(|r| r.key.clone()).collect();
    for candidate in &inventory.candidates {
        if !order.contains(&candidate.worktree.repo) {
            order.push(candidate.worktree.repo.clone());
        }
    }
    order
        .into_iter()
        .map(|key| {
            let mut group: Vec<usize> = inventory
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| c.worktree.repo == key)
                .map(|(index, _)| index)
                .collect();
            // The rows a user is looking for come first: safe, then review, then
            // keep, then the ones nothing can be done with yet.
            group.sort_by(|a, b| {
                let (a, b) = (&inventory.candidates[*a], &inventory.candidates[*b]);
                a.verdict
                    .cmp(&b.verdict)
                    .then_with(|| a.worktree.path.cmp(&b.worktree.path))
            });
            (key, group)
        })
        .filter(|(_, group)| !group.is_empty())
        .collect()
}

/// The heading above one repository's rows: its name, and its root truncated
/// from the left.
pub fn repo_heading(
    inventory: &Inventory,
    key: &RepoKey,
    first: &Candidate,
    width: usize,
) -> String {
    let (name, root) = repo_identity(inventory, key, first);
    let prefix = format!("repo {name}  ");
    let budget = width.saturating_sub(display_width(&prefix)).max(8);
    format!("{prefix}{}", truncate_left(&root.to_string_lossy(), budget))
}

fn repo_identity<'a>(
    inventory: &'a Inventory,
    key: &RepoKey,
    first: &'a Candidate,
) -> (Cow<'a, str>, &'a Path) {
    if let Some(repo) = inventory.repo(key) {
        (Cow::Borrowed(&repo.name), &repo.root)
    } else {
        let root = first.worktree.repo_root.as_path();
        (Cow::Owned(name_of(root)), root)
    }
}

fn name_of(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Non-fatal problems the scan collected. They belong on screen: silently
/// dropping them renders as a suspiciously tidy machine.
pub fn push_notes(out: &mut String, notes: &[String], width: usize) {
    if notes.is_empty() {
        return;
    }
    out.push('\n');
    push_line(out, "notes", width);
    for note in notes {
        push_wrapped(out, "  ", "    ", note, width);
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

fn push_cell(line: &mut String, text: &str, width: usize, align: Align) {
    if width == 0 {
        return;
    }
    let text = truncate_right(text, width);
    let pad = width.saturating_sub(display_width(&text));
    line.push(' ');
    match align {
        Align::Left => {
            line.push_str(&text);
            line.extend(std::iter::repeat_n(' ', pad));
        }
        Align::Right => {
            line.extend(std::iter::repeat_n(' ', pad));
            line.push_str(&text);
        }
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Width of `text` in terminal display columns. Hand-rolled because the crate
/// takes no width dependency.
pub fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // A CSI sequence occupies no columns. shear emits none itself, but a
            // branch name or a note can carry anything.
            if chars.peek() == Some(&'[') {
                chars.next();
                for tail in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&tail) {
                        break;
                    }
                }
            }
            continue;
        }
        width += char_columns(ch);
    }
    width
}

fn char_columns(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    let code = ch as u32;
    let zero_width = matches!(code,
        0x0300..=0x036f      // combining diacriticals
        | 0x1ab0..=0x1aff    // combining diacriticals extended
        | 0x20d0..=0x20ff    // combining marks for symbols
        | 0x200b..=0x200f    // zero width space .. RLM
        | 0xfe00..=0xfe0f    // variation selectors
        | 0xfe20..=0xfe2f    // combining half marks
        | 0xfeff);
    if zero_width {
        return 0;
    }
    let wide = matches!(code,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x2fffd
        | 0x30000..=0x3fffd);
    if wide {
        2
    } else {
        1
    }
}

/// Trims to `max` display columns, dropping characters from the LEFT and marking
/// the cut with `…`. For paths.
pub fn truncate_left(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return ELLIPSIS.to_string();
    }

    let budget = max - 1;
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let columns = char_columns(ch);
        if used + columns > budget {
            break;
        }
        used += columns;
        kept.push(ch);
    }
    kept.reverse();

    let mut out = String::from(ELLIPSIS);
    out.extend(kept);
    out
}

/// Trims to `max` display columns from the right. For labels and headings.
pub fn truncate_right(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return ELLIPSIS.to_string();
    }

    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let columns = char_columns(ch);
        if used + columns > budget {
            break;
        }
        used += columns;
        out.push(ch);
    }
    out.push(ELLIPSIS);
    out
}

/// Appends one line, trimmed of trailing space and clipped to `width`. Every
/// line the crate prints goes through here, which is what makes "no line is ever
/// wider than the width it was given" true by construction rather than by
/// arithmetic.
pub fn push_line(out: &mut String, line: &str, width: usize) {
    out.push_str(&truncate_right(line.trim_end(), width));
    out.push('\n');
}

/// Greedy word wrap. Notes and explanations wrap rather than truncate, because
/// truncating an explanation removes the explanation.
pub fn push_wrapped(out: &mut String, first: &str, rest: &str, text: &str, width: usize) {
    let mut prefix = first;
    let mut line = String::new();

    for word in text.split_whitespace() {
        let budget = width.saturating_sub(display_width(prefix)).max(1);
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if display_width(&candidate) <= budget || line.is_empty() {
            line = candidate;
        } else {
            push_line(out, &format!("{prefix}{line}"), width);
            prefix = rest;
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        push_line(out, &format!("{prefix}{line}"), width);
    }
}

/// The wrapped form of `text` as separate lines, for callers that assemble a
/// frame line by line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = String::new();
    push_wrapped(&mut out, "", "", text, width);
    out.lines().map(str::to_string).collect()
}

// ---------------------------------------------------------------------------
// Terminal size
// ---------------------------------------------------------------------------

/// Terminal size in (columns, rows). Read rather than cached: an overlay pane
/// can be resized under us.
pub fn terminal_size() -> (usize, usize) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let fd = std::io::stdout().as_raw_fd();
        // SAFETY: `size` is a correctly sized, owned `winsize`.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
        if rc == 0 && size.ws_col > 0 {
            let rows = if size.ws_row > 0 {
                size.ws_row as usize
            } else {
                24
            };
            return (size.ws_col as usize, rows);
        }
    }
    env_terminal_size()
}

fn env_terminal_size() -> (usize, usize) {
    let columns = crate::config::non_empty_env("COLUMNS")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|c| *c > 0)
        .unwrap_or(DEFAULT_COLUMNS);
    let rows = crate::config::non_empty_env("LINES")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|r| *r > 0)
        .unwrap_or(24);
    (columns, rows)
}

// ---------------------------------------------------------------------------
// The --list verb
// ---------------------------------------------------------------------------

/// `--list`: scan, size, print the table, exit. A dry run by construction —
/// this verb has no path to `remove`.
///
/// The only functions it can reach are [`crate::shear::scan`],
/// [`crate::disk::measure_all`] and the pure renderers above. Adding a removal
/// here would mean importing `remove`, which is why this module does not.
pub fn run_list(config: &Config) -> Result<()> {
    let mut inventory = crate::shear::scan(config)?;
    if config.measure_disk {
        // `--list` has nothing to keep responsive, so it measures up front and
        // prints one settled table rather than one that changes under the user.
        crate::disk::measure_all(&mut inventory, &AtomicBool::new(false));
    }

    let (columns, _) = terminal_size();
    let mut out = table(&inventory, columns);
    out.push('\n');
    out.push_str(&summary(&inventory, columns));
    print!("{out}");
    Ok(())
}
