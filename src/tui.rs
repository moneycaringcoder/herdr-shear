//! The interactive review pane.
//!
//! It runs in a herdr **overlay pane**, not a popup. The pane is a working
//! surface: a user scans rows, builds a selection, and confirms something
//! destructive. A popup lost to a stray key would lose that deliberate state.
//!
//! The split is intentional. [`state`] contains the policy that must remain
//! boring and pure; [`view`] turns it into ratatui widgets; [`run`] owns every
//! terminal and filesystem edge.

mod run;
mod state;
pub mod view;

pub use run::{
    append_removal_failure, append_removal_messages, exit_messages, map_key_event, run_review,
};
pub use state::{adopt, apply, display_order, preflight, preselectable, Key, Mode, Review};
