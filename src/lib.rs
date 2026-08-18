//! shear — a worktree janitor for herdr.
//!
//! The crate is split into a library plus a thin binary so the integration tests
//! in `tests/` can reach the real modules.
//!
//! The shape of the thing:
//!
//! ```text
//!   herdr session.snapshot ─┐
//!   worktree.list  ─────────┤
//!                           ├─> shear::scan ──> Inventory ──> report / render / tui
//!   git worktree list ──────┤                       │
//!   git status/for-each-ref ┘                       └──> remove::remove_one
//! ```
//!
//! Every git invocation in `git.rs` is read-only and proven so by
//! `tests/read_only.rs`. The only code in the crate that may change a
//! repository is `remove.rs`, and only along a path the user has explicitly
//! selected.

pub mod classify;
pub mod config;
pub mod disk;
pub mod git;
pub mod herdr;
pub mod model;
pub mod remove;
pub mod render;
pub mod report;
pub mod shear;
pub mod timestamp;
pub mod tui;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
