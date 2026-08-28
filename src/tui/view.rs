//! The ratatui view of the review state.
//!
//! This module knows terminal geometry and colour, but no policy. It receives a
//! [`Review`](super::Review), draws exactly what that state says, and returns the
//! row hit boxes needed by mouse input. Selection, confirmation, and removal
//! decisions remain in the pure state machine in the parent module.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{Inventory, Size, Verdict};
use crate::render;

use super::{Mode, Review};

const TITLE: &str = "shear · review worktrees";
const HELP_WIDE: &str = "↑/k ↓/j move  wheel scroll  click focus  space select  a safe rows  n none  r remove  R rescan  q/Esc quit";
const HELP_NARROW: &str = "↑↓/wheel move · click focus · space select · a safe · r remove · q quit";

/// Candidate rows currently occupying screen coordinates.
///
/// A click is deliberately only a cursor move. Returning the map from the view
/// keeps that promise honest: the event loop can identify a row, but the map has
/// no operation that toggles or removes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MouseMap {
    rows: Vec<HitRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HitRow {
    y: u16,
    left: u16,
    right: u16,
    candidate: usize,
}

impl MouseMap {
    /// The candidate under a terminal cell, if that cell belongs to a visible
    /// worktree row. Repository headings and footer text are not clickable.
    pub fn candidate_at(&self, column: u16, row: u16) -> Option<usize> {
        self.rows
            .iter()
            .find(|hit| hit.y == row && column >= hit.left && column < hit.right)
            .map(|hit| hit.candidate)
    }
}

/// Draws one complete review frame and returns its mouse hit map.
///
/// The body scroll position is derived from the cursor, so the cursor can never
/// point at an off-screen row. No scroll offset is stored beside [`Review`]: it
/// is presentation state and therefore has no place in the safety state machine.
pub fn render(frame: &mut Frame<'_>, review: &Review) -> MouseMap {
    let area = frame.area();
    if area.is_empty() {
        return MouseMap::default();
    }

    let header_height = if area.height >= 16 { 5 } else { 3 };
    let footer_height = if area.height >= 20 {
        9
    } else {
        area.height.saturating_sub(header_height + 4).clamp(3, 7)
    };
    let regions = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(3),
        Constraint::Length(footer_height),
    ])
    .split(area);

    render_header(frame, review, regions[0]);
    let mouse = render_body(frame, review, regions[1]);
    render_footer(frame, review, regions[2]);
    render_modal(frame, review, area);
    mouse
}

fn render_header(frame: &mut Frame<'_>, review: &Review, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {TITLE} "),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(inventory_line(&review.inventory)),
        Line::from(reclaimable_line(&review.inventory)),
        Line::from(format!(
            "{} of {} selected",
            review.selected.len(),
            review.inventory.candidates.len()
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn inventory_line(inventory: &Inventory) -> String {
    let repos: BTreeSet<_> = inventory
        .candidates
        .iter()
        .map(|candidate| &candidate.worktree.repo)
        .collect();
    let count = |verdict| {
        inventory
            .candidates
            .iter()
            .filter(|candidate| candidate.verdict == verdict)
            .count()
    };
    format!(
        "{} worktrees in {} repositories · {} safe · {} review · {} keep · {} blocked",
        inventory.candidates.len(),
        repos.len(),
        count(Verdict::Safe),
        count(Verdict::Review),
        count(Verdict::Keep),
        count(Verdict::Blocked),
    )
}

fn reclaimable_line(inventory: &Inventory) -> String {
    let safe: Vec<_> = inventory.safe().collect();
    let (bytes, unknown) = crate::shear::reclaimable(safe.iter().copied());
    let skipped = safe
        .iter()
        .filter(|candidate| candidate.size == Size::Skipped)
        .count();
    let unmeasured = unknown.saturating_sub(skipped);
    if safe.is_empty() {
        return "safe reclaimable: nothing currently qualifies".into();
    }
    if skipped == safe.len() {
        return "safe reclaimable: disk measurement skipped".into();
    }
    let mut line = format!("safe reclaimable: {}", render::human_bytes(bytes));
    if unmeasured > 0 {
        line.push_str(&format!("+ · {unmeasured} not measured"));
    }
    if skipped > 0 {
        line.push_str(&format!(" · {skipped} skipped"));
    }
    line
}

#[derive(Debug)]
enum BodyLine {
    Repository(String),
    Candidate(usize),
    Note(String),
}

fn body_lines(review: &Review, width: usize) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    for (key, group) in render::grouped_indices(&review.inventory) {
        let Some(first) = group
            .first()
            .and_then(|index| review.inventory.candidates.get(*index))
        else {
            continue;
        };
        lines.push(BodyLine::Repository(render::repo_heading(
            &review.inventory,
            &key,
            first,
            width,
        )));
        lines.extend(group.into_iter().map(BodyLine::Candidate));
    }
    if !review.inventory.notes.is_empty() {
        lines.push(BodyLine::Repository("notes".into()));
        lines.extend(review.inventory.notes.iter().cloned().map(BodyLine::Note));
    }
    lines
}

fn render_body(frame: &mut Frame<'_>, review: &Review, area: Rect) -> MouseMap {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " worktrees ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return MouseMap::default();
    }

    if review.inventory.candidates.is_empty() {
        frame.render_widget(Paragraph::new("no worktrees found."), inner);
        return MouseMap::default();
    }

    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let rows_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    let columns = Columns::for_width(&review.inventory, inner.width);
    render_table_header(frame, header_area, &columns);

    let lines = body_lines(review, inner.width as usize);
    let capacity = rows_area.height as usize;
    if capacity == 0 {
        return MouseMap::default();
    }
    let cursor_line = lines
        .iter()
        .position(|line| matches!(line, BodyLine::Candidate(index) if *index == review.cursor))
        .unwrap_or(0);
    let top = if lines.len() <= capacity {
        0
    } else {
        cursor_line
            .saturating_add(1)
            .saturating_sub(capacity)
            .min(lines.len() - capacity)
    };

    let mut mouse = MouseMap::default();
    for (offset, line) in lines.iter().skip(top).take(capacity).enumerate() {
        let row = Rect::new(
            rows_area.x,
            rows_area.y.saturating_add(offset as u16),
            rows_area.width,
            1,
        );
        match line {
            BodyLine::Repository(heading) => frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    heading.clone(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ))),
                row,
            ),
            BodyLine::Candidate(index) => {
                render_candidate(frame, review, *index, row, &columns);
                mouse.rows.push(HitRow {
                    y: row.y,
                    left: row.x,
                    right: row.x.saturating_add(row.width),
                    candidate: *index,
                });
            }
            BodyLine::Note(note) => frame.render_widget(
                Paragraph::new(format!("  {note}")).style(Style::default().fg(Color::Yellow)),
                row,
            ),
        }
    }
    mouse
}

#[derive(Debug, Clone)]
struct Columns {
    constraints: Vec<Constraint>,
    classes: bool,
    age: bool,
    disk: bool,
    branch: bool,
}

impl Columns {
    fn for_width(inventory: &Inventory, width: u16) -> Self {
        // The list renderer remains the source of desired widths. The TUI
        // spends three more cells on `>[x]` than its one-cell marker, then
        // removes whole columns until those constraints fit. Keeping that
        // decision here matters below the list renderer's 40-column floor:
        // ratatui must drop branch, classes, age, and disk rather than squeeze
        // their headings into misleading fragments.
        let mut widths = render::widths_for(inventory, width.saturating_sub(3) as usize);
        if tui_widths_total(&widths) > width as usize {
            widths.branch = 0;
        }
        if tui_widths_total(&widths) > width as usize {
            widths.classes = 0;
        }
        if tui_widths_total(&widths) > width as usize {
            widths.age = 0;
        }
        if tui_widths_total(&widths) > width as usize {
            widths.size = 0;
        }
        let mut constraints = vec![
            Constraint::Length(4),
            Constraint::Length((widths.verdict + 1) as u16),
        ];
        for optional in [widths.classes, widths.age, widths.size, widths.branch] {
            if optional > 0 {
                constraints.push(Constraint::Length((optional + 1) as u16));
            }
        }
        constraints.push(Constraint::Fill(1));
        Self {
            constraints,
            classes: widths.classes > 0,
            age: widths.age > 0,
            disk: widths.size > 0,
            branch: widths.branch > 0,
        }
    }

    fn areas(&self, area: Rect) -> std::rc::Rc<[Rect]> {
        Layout::horizontal(self.constraints.clone()).split(area)
    }
}

fn tui_widths_total(widths: &render::Widths) -> usize {
    4 + [
        widths.verdict,
        widths.classes,
        widths.age,
        widths.size,
        widths.branch,
        widths.path,
    ]
    .into_iter()
    .filter(|width| *width > 0)
    .map(|width| width + 1)
    .sum::<usize>()
}

fn render_table_header(frame: &mut Frame<'_>, area: Rect, columns: &Columns) {
    let cells = columns.areas(area);
    let mut at = 0;
    render_cell(frame, cells[at], "", Alignment::Left, Style::default());
    at += 1;
    render_cell(
        frame,
        cells[at],
        "verdict",
        Alignment::Left,
        Style::default().add_modifier(Modifier::UNDERLINED),
    );
    at += 1;
    for (shown, label, alignment) in [
        (columns.classes, "classes", Alignment::Left),
        (columns.age, "age", Alignment::Right),
        (columns.disk, "disk", Alignment::Right),
        (columns.branch, "branch", Alignment::Left),
    ] {
        if shown {
            render_cell(
                frame,
                cells[at],
                label,
                alignment,
                Style::default().add_modifier(Modifier::UNDERLINED),
            );
            at += 1;
        }
    }
    render_cell(
        frame,
        cells[at],
        "path",
        Alignment::Left,
        Style::default().add_modifier(Modifier::UNDERLINED),
    );
}

fn render_candidate(
    frame: &mut Frame<'_>,
    review: &Review,
    index: usize,
    area: Rect,
    columns: &Columns,
) {
    let Some(candidate) = review.inventory.candidates.get(index) else {
        return;
    };
    let selected = review.selected.contains(&index);
    let cursor = review.cursor == index;
    let mut row_style = Style::default();
    if cursor {
        row_style = row_style.add_modifier(Modifier::REVERSED);
    }
    frame.render_widget(Block::default().style(row_style), area);

    let cells = columns.areas(area);
    let mut at = 0;
    let checkbox = format!(
        "{}[{}]",
        if cursor { '>' } else { ' ' },
        if selected { 'x' } else { ' ' }
    );
    let checkbox_style = if selected {
        row_style
            .fg(verdict_color(candidate.verdict))
            .add_modifier(Modifier::BOLD)
    } else {
        row_style
    };
    frame.render_widget(
        Paragraph::new(checkbox)
            .alignment(Alignment::Left)
            .style(checkbox_style),
        cells[at],
    );
    at += 1;

    render_cell(
        frame,
        cells[at],
        candidate.verdict.label(),
        Alignment::Left,
        row_style.patch(verdict_style(candidate.verdict)),
    );
    at += 1;

    if columns.classes {
        let width = cells[at].width.saturating_sub(1) as usize;
        render_cell(
            frame,
            cells[at],
            &render::classes_cell(candidate, width),
            Alignment::Left,
            row_style,
        );
        at += 1;
    }
    if columns.age {
        let age = render::human_age(candidate.last_commit.map(|tip| {
            SystemTime::now()
                .duration_since(tip)
                .unwrap_or(Duration::ZERO)
        }));
        render_cell(frame, cells[at], &age, Alignment::Right, row_style);
        at += 1;
    }
    if columns.disk {
        render_cell(
            frame,
            cells[at],
            &render::size_cell(candidate.size),
            Alignment::Right,
            row_style,
        );
        at += 1;
    }
    if columns.branch {
        let value = render::truncate_right(
            &render::branch_cell(candidate),
            cells[at].width.saturating_sub(1) as usize,
        );
        let style = if selected {
            row_style.fg(verdict_color(candidate.verdict))
        } else {
            row_style
        };
        render_cell(frame, cells[at], &value, Alignment::Left, style);
        at += 1;
    }
    let path = render::truncate_left(
        &candidate.path().to_string_lossy(),
        cells[at].width.saturating_sub(1) as usize,
    );
    let style = if selected {
        row_style.fg(verdict_color(candidate.verdict))
    } else {
        row_style
    };
    render_cell(frame, cells[at], &path, Alignment::Left, style);
}

fn verdict_color(verdict: Verdict) -> Color {
    match verdict {
        Verdict::Safe => Color::Green,
        Verdict::Review => Color::Yellow,
        Verdict::Keep => Color::Cyan,
        Verdict::Blocked => Color::Red,
    }
}

fn verdict_style(verdict: Verdict) -> Style {
    Style::default()
        .fg(verdict_color(verdict))
        .add_modifier(Modifier::BOLD)
}

fn render_cell(frame: &mut Frame<'_>, area: Rect, value: &str, alignment: Alignment, style: Style) {
    if area.is_empty() {
        return;
    }
    let value = if area.x == 0 || area.width <= 1 {
        value.to_string()
    } else {
        format!(" {value}")
    };
    frame.render_widget(
        Paragraph::new(value).alignment(alignment).style(style),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, review: &Review, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" review ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let help_y = inner.bottom().saturating_sub(1);
    let safety_y = help_y.saturating_sub(2);
    let selection_area = Rect::new(inner.x, inner.y, inner.width, 1.min(inner.height));
    frame.render_widget(
        Paragraph::new(selection_line(review)).style(Style::default().add_modifier(Modifier::BOLD)),
        selection_area,
    );

    let detail_y = inner.y.saturating_add(1);
    let detail_height = if inner.height >= 7 { 2 } else { 1 };
    if detail_y < safety_y {
        let area = Rect::new(
            inner.x,
            detail_y,
            inner.width,
            detail_height.min(safety_y.saturating_sub(detail_y)),
        );
        frame.render_widget(
            Paragraph::new(detail_text(review))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Reset)),
            area,
        );
    }

    let message_y = detail_y.saturating_add(detail_height);
    if message_y < safety_y {
        let area = Rect::new(
            inner.x,
            message_y,
            inner.width,
            safety_y.saturating_sub(message_y),
        );
        frame.render_widget(
            Paragraph::new(message_text(review))
                .wrap(Wrap { trim: false })
                .style(message_style(review)),
            area,
        );
    }

    if safety_y >= inner.y {
        frame.render_widget(
            Paragraph::new(render::SAFETY_NOTE)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(Color::Green)),
            Rect::new(inner.x, safety_y, inner.width, 2.min(inner.height)),
        );
    }
    if help_y >= inner.y {
        let help = if inner.width as usize >= render::display_width(HELP_WIDE) {
            HELP_WIDE
        } else {
            HELP_NARROW
        };
        frame.render_widget(
            Paragraph::new(help).style(Style::default().fg(Color::Reset)),
            Rect::new(inner.x, help_y, inner.width, 1),
        );
    }
}

fn detail_text(review: &Review) -> Text<'static> {
    let Some(candidate) = review.inventory.candidates.get(review.cursor) else {
        return Text::default();
    };
    let name = candidate
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| candidate.path().to_string_lossy().into_owned());
    let mut lines = vec![Line::from(format!(
        "{name}: {} — {}",
        candidate.verdict.label(),
        candidate.reason
    ))];
    if let Some(signal) = crate::classify::signals(candidate, SystemTime::now())
        .into_iter()
        .next()
    {
        lines.push(Line::from(format!("  {signal}")));
    }
    Text::from(lines)
}

fn message_text(review: &Review) -> String {
    let mut messages = review
        .undo_warnings
        .iter()
        .rev()
        .chain(review.messages.iter())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(status) = match review.mode {
        Mode::Preflighting => Some("Refreshing selection…"),
        Mode::Removing => Some("Removing…"),
        Mode::Rescanning => Some("Rescanning…"),
        _ => None,
    } {
        messages.push(status.into());
    }
    messages.join("\n")
}

fn message_style(review: &Review) -> Style {
    if review.undo_warnings.is_empty() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
}

fn selection_line(review: &Review) -> String {
    if review.selected.is_empty() {
        return "nothing selected".into();
    }
    let (bytes, unknown) = crate::shear::reclaimable(review.selection());
    let skipped = review
        .selection()
        .filter(|candidate| candidate.size == Size::Skipped)
        .count();
    let unmeasured = unknown.saturating_sub(skipped);
    let mut line = if skipped == review.selected.len() {
        format!("{} selected · disk size skipped", review.selected.len())
    } else {
        format!(
            "{} selected · {}",
            review.selected.len(),
            render::human_bytes(bytes)
        )
    };
    if unmeasured > 0 {
        line.push_str(&format!(" · {unmeasured} not measured"));
    }
    if skipped > 0 && skipped != review.selected.len() {
        line.push_str(&format!(" · {skipped} size skipped"));
    }
    let (files, worktrees) = review.at_risk();
    if files > 0 {
        line.push_str(&format!(
            " · {files} uncommitted files in {worktrees} of them"
        ));
    }
    line
}

fn render_modal(frame: &mut Frame<'_>, review: &Review, screen: Rect) {
    match &review.mode {
        Mode::ConfirmClean { count, .. } => {
            let area = centered(screen, 74, 10);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " Confirm removal ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let text = format!(
                "{}\n\n{}\n\nPress y or Enter to confirm. Esc cancels.",
                clean_question(review, *count),
                render::SAFETY_NOTE,
            );
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
        }
        Mode::ConfirmDirty {
            files,
            typed,
            worktrees,
        } => {
            let area = centered(screen, 78, 14);
            frame.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red))
                .title(Span::styled(
                    " Uncommitted work ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let body = format!(
                "{worktrees} of the {} selected {} uncommitted work: {files} {} that exist nowhere else. Removing the checkout destroys them; no branch and no commit is touched.\n\nfiles at risk: {files}    typed: {typed}_\n\n{}\n\nThis one cannot be answered with `y`. Type the exact number of files at risk, then Enter. Esc cancels.",
                review.selected.len(),
                if *worktrees == 1 {
                    "worktrees has"
                } else {
                    "worktrees have"
                },
                if *files == 1 { "file" } else { "files" },
                render::SAFETY_NOTE,
            );
            frame.render_widget(
                Paragraph::new(body)
                    .wrap(Wrap { trim: true })
                    .style(Style::default().fg(Color::Reset)),
                inner,
            );
        }
        _ => {}
    }
}

fn clean_question(review: &Review, count: usize) -> String {
    let (bytes, unknown) = crate::shear::reclaimable(review.selection());
    let skipped = review
        .selection()
        .filter(|candidate| candidate.size == Size::Skipped)
        .count();
    let unmeasured = unknown.saturating_sub(skipped);
    let all_non_skipped_unmeasured = bytes == 0 && unmeasured > 0 && unmeasured + skipped == count;
    let noun = if count == 1 { "worktree" } else { "worktrees" };
    let mut ask = if skipped == count {
        format!("Remove {count} {noun}? Disk measurement was skipped.")
    } else if all_non_skipped_unmeasured {
        format!("Remove {count} {noun}? Disk size is not measured yet.")
    } else {
        format!(
            "Remove {count} {noun} and reclaim {}?",
            render::human_bytes(bytes)
        )
    };
    if unmeasured > 0 && !all_non_skipped_unmeasured {
        ask.push_str(&format!(
            " ({unmeasured} of them was not measured, so the real figure is larger.)"
        ));
    }
    if skipped > 0 && skipped != count {
        ask.push_str(&format!(
            " Size measurement was skipped for {skipped} of them."
        ));
    }
    ask
}

fn centered(screen: Rect, max_width: u16, height: u16) -> Rect {
    let width = max_width.min(screen.width.saturating_sub(2).max(1));
    let height = height.min(screen.height.saturating_sub(2).max(1));
    Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
