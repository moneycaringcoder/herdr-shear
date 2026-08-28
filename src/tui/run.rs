//! Crossterm runtime for the interactive review pane.
//!
//! This is the impure edge around the state machine: scans, background sizing,
//! terminal events, and removal calls all live here. Raw mode and the alternate
//! screen are one resource, restored from [`Drop`], the panic hook, and signal
//! handlers. A cleanup tool that strands a pane in raw mode has caused more
//! damage than the worktrees it was meant to remove.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::Config;
use crate::model::{Inventory, Size};
use crate::render::{self, MIN_COLUMNS};
use crate::Result;

use super::state::{adopt, apply, display_order, preflight, Key, Mode, Review};
use super::view;

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

    // Raw mode and the alternate screen are entered exactly once. The guard is
    // deliberately outside ratatui's Terminal so it also owns setup failures.
    let guard = terminal::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = event_loop(config, inventory, &stop, &mut terminal);
    drop(terminal);
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

type ReviewTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn event_loop(
    config: &Config,
    inventory: Inventory,
    stop: &AtomicBool,
    terminal: &mut ReviewTerminal,
) -> Result<Review> {
    let mut review = Review::new(inventory);
    let mut sizer = Sizer::start(&review.inventory, config);
    let mut mouse = view::MouseMap::default();
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
            terminal.draw(|frame| {
                mouse = view::render(frame, &review);
            })?;
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

        let Some(event) = next_event()? else {
            continue;
        };
        match event {
            Event::Key(event) => {
                if let Some(key) = map_key_event(event) {
                    review = apply(review, key);
                    dirty = true;
                }
            }
            Event::Mouse(event) => match event.kind {
                MouseEventKind::Down(MouseButton::Left) if review.mode == Mode::Browsing => {
                    if let Some(candidate) = mouse.candidate_at(event.column, event.row) {
                        review.cursor = candidate;
                        dirty = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    review = apply(review, Key::Up);
                    dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    review = apply(review, Key::Down);
                    dirty = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => dirty = true,
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

fn next_event() -> Result<Option<Event>> {
    match event::poll(Duration::from_millis(50)) {
        Ok(false) => Ok(None),
        Ok(true) => match event::read() {
            Ok(event) => Ok(Some(event)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(err.into()),
        },
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Maps crossterm's already-decoded key events onto the pure state machine.
///
/// Release events are ignored. `Esc` maps to [`Key::Quit`]: browsing exits,
/// while both confirmation states already interpret Quit as cancellation.
#[doc(hidden)]
pub fn map_key_event(event: KeyEvent) -> Option<Key> {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let key = match event.code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::Char(' ') => Key::Toggle,
        KeyCode::Char('a') => Key::SelectSafe,
        KeyCode::Char('n') => Key::SelectNone,
        KeyCode::Char('r') => Key::Remove,
        KeyCode::Char('R') => Key::Rescan,
        KeyCode::Char('q') | KeyCode::Esc => Key::Quit,
        KeyCode::Char('y') | KeyCode::Enter => Key::Confirm,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => Key::Quit,
        KeyCode::Char(digit @ '0'..='9') => Key::Digit(digit as u8 - b'0'),
        _ => Key::Other,
    };
    Some(key)
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
// Terminal lifecycle
// ---------------------------------------------------------------------------

/// Crossterm raw mode and the three ways out of it.
///
/// Restoration is idempotent because a signal or panic can restore first and
/// the guard will still be dropped while unwinding. Every path disables raw
/// mode, leaves the alternate screen, disables mouse capture, and shows the
/// cursor before normal output resumes.
#[cfg(unix)]
mod terminal {
    use std::io::{self, IsTerminal};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Once;
    use std::time::Duration;

    use crossterm::cursor::{Hide, Show};
    use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static HOOKS: Once = Once::new();

    /// A live crossterm session. Dropping it restores the normal screen.
    pub struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            restore();
        }
    }

    pub fn enter() -> crate::Result<Guard> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(
                "the review pane needs a terminal on stdin and stdout; use --list or --json \
                 when there is not one"
                    .into(),
            );
        }

        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        install_hooks();
        if let Err(err) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide) {
            restore();
            return Err(err.into());
        }
        Ok(Guard(()))
    }

    /// Discards events typed while a blocking preflight scan was in flight.
    pub fn discard_input() -> crate::Result<()> {
        while event::poll(Duration::ZERO)? {
            let _ = event::read()?;
        }
        Ok(())
    }

    /// Puts the terminal back. Safe to call repeatedly from Drop, the panic
    /// hook, and the SIGINT/SIGTERM restoration handlers.
    pub fn restore() {
        if !ACTIVE.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
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
                // The state-machine flag handles an orderly exit. This handler
                // restores crossterm immediately as a final belt-and-braces
                // path when a terminating signal outruns the event loop.
                let _ = unsafe { signal_hook::low_level::register(signal, restore) };
            }
        });
    }
}

#[cfg(not(unix))]
mod terminal {
    pub struct Guard(());

    pub fn enter() -> crate::Result<Guard> {
        Err("the review pane is unix-only; use --list or --json".into())
    }

    pub fn discard_input() -> crate::Result<()> {
        Ok(())
    }
}
