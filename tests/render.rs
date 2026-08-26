//! The table is the product. These tests pin what it prints, column by column,
//! from hand-built inventories — no repository, no session, no disk.
//!
//! Every assertion here is about something a user would notice: a line that
//! overflows the pane, a path whose informative half was cut off, a failed
//! measurement that reads as a plausible zero, or an unanswerable question
//! rendered as an answer.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use shear::model::{
    Candidate, Class, Dirt, Head, Inventory, LockInfo, Merged, OpenWorkspace, PrunableInfo, Repo,
    RepoKey, Size, Upstream, Verdict, Worktree,
};
use shear::render::{
    self, display_width, human_age, size_cell, summary, table, truncate_left, truncate_right,
    widths_for, MIN_COLUMNS,
};

// ---------------------------------------------------------------------------
// Hand-built inventories
// ---------------------------------------------------------------------------

/// One row, built field by field. Hand-built rather than fixture-driven so a
/// combination that no repository would produce — a `Safe` verdict on a dirty
/// worktree, say — can still be rendered and asserted on.
struct Row(Candidate);

impl Row {
    fn new(repo_root: &str, path: &str, head: Head) -> Self {
        Self(Candidate {
            worktree: Worktree {
                repo: RepoKey(format!("{repo_root}/.git")),
                repo_root: PathBuf::from(repo_root),
                path: PathBuf::from(path),
                head,
                head_oid: Some("c0ffee".repeat(6) + "abcd"),
                is_main: false,
                locked: None,
                prunable: None,
                notes: Vec::new(),
            },
            dirt: Dirt::default(),
            upstream: Upstream::default(),
            merged: Merged::Unknown,
            last_commit: None,
            open_workspace: None,
            protected: None,
            classes: BTreeSet::new(),
            verdict: Verdict::Keep,
            size: Size::Pending,
            reason: String::new(),
        })
    }

    fn branch(repo_root: &str, path: &str, branch: &str) -> Self {
        Self::new(repo_root, path, Head::Branch(branch.to_string()))
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

    fn size(mut self, size: Size) -> Self {
        self.0.size = size;
        self
    }

    fn days_old(mut self, days: u64) -> Self {
        // Half a day past the boundary, so a slow test run cannot tip the cell
        // into the next unit.
        self.0.last_commit = Some(SystemTime::now() - Duration::from_secs(days * 86_400 + 43_200));
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

    fn locked(mut self, reason: &str) -> Self {
        self.0.worktree.locked = Some(LockInfo {
            reason: Some(reason.to_string()),
        });
        self
    }

    fn prunable(mut self, reason: &str) -> Self {
        self.0.worktree.prunable = Some(PrunableInfo {
            reason: Some(reason.to_string()),
        });
        self
    }

    fn open(mut self, label: &str) -> Self {
        self.0.open_workspace = Some(OpenWorkspace {
            workspace_id: "ws-1".into(),
            label: label.into(),
        });
        self
    }

    fn note(mut self, note: &str) -> Self {
        self.0.worktree.notes.push(note.to_string());
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

const APP: &str = "/home/dev/repos/app";
const LONG: &str = "/home/dev/repos/very-long-name-for-a-repository";

fn repo(root: &str, name: &str) -> Repo {
    Repo {
        key: RepoKey(format!("{root}/.git")),
        root: PathBuf::from(root),
        name: name.to_string(),
    }
}

/// Two repositories covering every [`Verdict`], every [`Class`], every [`Size`]
/// state and all three [`Merged`] states.
fn full_inventory() -> Inventory {
    Inventory {
        repos: vec![repo(APP, "app"), repo(LONG, "very-long-name-for-a-repository")],
        candidates: vec![
            Row::branch(APP, APP, "main")
                .main_checkout()
                .verdict(Verdict::Blocked)
                .merged(Merged::Into("origin/main".into()))
                .days_old(2)
                .size(Size::Bytes(126_000_000))
                .reason("main checkout: never removable, and there is no override")
                .build(),
            Row::branch(APP, "/home/dev/repos/app-wt/feature-login", "feature/login")
                .verdict(Verdict::Safe)
                .classes(&[Class::Merged, Class::GoneUpstream])
                .merged(Merged::Into("origin/main".into()))
                .days_old(21)
                .size(Size::Bytes(1_310_000_000))
                .reason("merged into origin/main, upstream gone, clean")
                .build(),
            Row::branch(APP, "/home/dev/repos/app-wt/hotfix-payments", "hotfix/payments")
                .verdict(Verdict::Review)
                .classes(&[Class::Dirty, Class::Stale])
                .merged(Merged::No("origin/main".into()))
                .days_old(60)
                .dirt(1, 2, 4)
                .size(Size::Bytes(50_400_000))
                .reason("7 uncommitted files; last commit 60 days ago")
                .build(),
            Row::branch(APP, "/home/dev/repos/app-wt/spike-wasm", "spike/wasm")
                .verdict(Verdict::Blocked)
                .classes(&[Class::Locked, Class::Stale])
                .merged(Merged::No("origin/main".into()))
                .locked("benchmark rig")
                .days_old(140)
                .size(Size::Failed)
                .reason("locked (benchmark rig): unlock it with `git worktree unlock`")
                .build(),
            Row::branch(APP, "/home/dev/repos/app-wt/review-ui", "review/ui")
                .verdict(Verdict::Blocked)
                .classes(&[Class::OpenInHerdr])
                .merged(Merged::No("origin/main".into()))
                .open("ui-review")
                .days_old(3)
                .size(Size::Bytes(310_000_000))
                .reason("open in herdr workspace ui-review: close it first")
                .build(),
            Row::branch(APP, "/home/dev/repos/app-wt/chore-deps", "chore/deps")
                .verdict(Verdict::Review)
                .classes(&[Class::Prunable, Class::GoneUpstream])
                .merged(Merged::No("origin/main".into()))
                .prunable("gitdir file points to non-existent location")
                .note("gitdir file points to non-existent location")
                .days_old(9)
                .size(Size::Gone)
                .reason("prunable: the checkout directory is gone")
                .build(),
            Row::new(APP, "/home/dev/repos/app-wt/bisect", Head::Detached)
                .verdict(Verdict::Keep)
                .merged(Merged::Unknown)
                .size(Size::Pending)
                .reason("detached HEAD: nothing suggests this is dead")
                .build(),
            Row::branch(
                LONG,
                "/home/dev/repos/very-long-name-for-a-repository/worktrees/2026-01-release-candidate",
                "release/2026-01-candidate",
            )
            .verdict(Verdict::Review)
            .classes(&[Class::Merged])
            .merged(Merged::Into("origin/main".into()))
            .days_old(240)
            .size(Size::Bytes(2_684_354_560))
            .reason("merged into origin/main, but its upstream still exists")
            .build(),
            Row::branch(
                LONG,
                "/home/dev/repos/very-long-name-for-a-repository/worktrees/spike",
                "feature/a-branch-name-that-will-not-fit",
            )
            .verdict(Verdict::Keep)
            .merged(Merged::No("origin/main".into()))
            .days_old(0)
            .size(Size::Bytes(716_800))
            .reason("nothing suggests this is dead")
            .build(),
        ],
        notes: vec![
            "herdr is not reachable (no socket at /run/herdr.sock); worktrees held open by a \
             workspace cannot be identified"
                .into(),
        ],
    }
}

fn empty_inventory() -> Inventory {
    Inventory::default()
}

fn six_repository_inventory() -> Inventory {
    let specs = [
        (
            "/home/dev/repos/repository-three-with-an-intentionally-long-root-name",
            "repository-three-with-an-intentionally-long-display-name",
            3_u64,
        ),
        (
            "/home/dev/repos/repository-one-with-an-intentionally-long-root-name",
            "repository-one-with-an-intentionally-long-display-name",
            1,
        ),
        (
            "/home/dev/repos/repository-six-with-an-intentionally-long-root-name",
            "repository-six-with-an-intentionally-long-display-name",
            6,
        ),
        (
            "/home/dev/repos/repository-two-with-an-intentionally-long-root-name",
            "repository-two-with-an-intentionally-long-display-name",
            2,
        ),
        (
            "/home/dev/repos/repository-five-with-an-intentionally-long-root-name",
            "repository-five-with-an-intentionally-long-display-name",
            5,
        ),
        (
            "/home/dev/repos/repository-four-with-an-intentionally-long-root-name",
            "repository-four-with-an-intentionally-long-display-name",
            4,
        ),
    ];
    Inventory {
        repos: specs
            .iter()
            .map(|(root, name, _)| repo(root, name))
            .collect(),
        candidates: specs
            .iter()
            .map(|(root, _, mib)| {
                let path = format!("{root}/worktrees/cleanup");
                Row::branch(root, &path, "cleanup")
                    .verdict(Verdict::Safe)
                    .size(Size::Bytes(mib * 1024 * 1024))
                    .build()
            })
            .collect(),
        notes: Vec::new(),
    }
}

fn widths_of(text: &str) -> Vec<usize> {
    text.lines().map(display_width).collect()
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// Eighty columns is the width a pane is judged at, so this is the one place
/// the whole table is pinned character for character: every verdict, every
/// class, all four size states and all three merged states in one frame.
#[test]
fn the_table_at_eighty_columns_is_pinned() {
    let expected = "  verdict classes        age   disk branch           path

repo app  /home/dev/repos/app
  safe    gone,merged     3w 1.2 GB feature/login    \u{2026}repos/app-wt/feature-login
  review  prunable,gone   9d      - chore/deps       \u{2026}ev/repos/app-wt/chore-deps
      gitdir file points to non-existent location
  review  dirty,stale    2mo  48 MB hotfix/payments  \u{2026}pos/app-wt/hotfix-payments
  keep    merged?          -      \u{2026} (detached)       \u{2026}me/dev/repos/app-wt/bisect
  blocked                 2d 120 MB main             /home/dev/repos/app
  blocked open            3d 295 MB review/ui        \u{2026}dev/repos/app-wt/review-ui
  blocked locked,stale   5mo      ? spike/wasm       \u{2026}ev/repos/app-wt/spike-wasm

repo very-long-name-for-a-repository  \u{2026}dev/repos/very-long-name-for-a-repository
  review  merged         8mo 2.5 GB release/2026-01\u{2026} \u{2026}/2026-01-release-candidate
  keep                   12h 700 kB feature/a-branc\u{2026} \u{2026}repository/worktrees/spike

merged? — shear could not ask whether that worktree's commit is contained in the
  integration ref: no integration ref resolved in the repository, or the
  worktree has no commit to test. That is not the same as `not merged`. A row
  carrying `merged` was tested and is contained; a row carrying neither was
  tested and is not; nothing carrying `merged?` is ever called safe.

notes
  herdr is not reachable (no socket at /run/herdr.sock); worktrees held open by
    a workspace cannot be identified
";
    assert_eq!(table(&full_inventory(), 80), expected);
}

#[test]
fn the_summary_at_eighty_columns_is_pinned() {
    let expected = "\
9 worktrees in 2 repositories: 1 safe worktree, 3 review, 2 keep, 3 blocked.
Removing the 1 safe worktree would reclaim 1.2 GB of the 4.1 GB all 9 worktrees
occupy.
2 worktrees could not be measured, so that figure is a floor, not an estimate.
  app: 7 worktrees, 1 safe · 1.2 GB reclaimable of 1.6 GB+ (2 unmeasured)
  very-long-name-f\u{2026}: 2 worktrees, 0 safe · nothing safe to remove · 2.5 GB total
Removing a worktree leaves its branch and every commit on it intact: only the
checkout goes.
";
    assert_eq!(summary(&full_inventory(), 80), expected);
}

/// At forty columns the branch column is gone and the path is squeezed, but
/// every row still says what it is, what it costs and how old it is.
#[test]
fn the_table_survives_forty_columns() {
    let rendered = table(&full_inventory(), 40);
    for line in rendered.lines() {
        assert!(display_width(line) <= 40, "{line}");
    }
    let row = rendered
        .lines()
        .find(|line| line.contains("safe"))
        .expect("the safe row is rendered");
    assert!(row.contains("1.2 GB"), "the disk figure survives: {row}");
    assert!(
        row.trim_end().ends_with("ature-login") && row.contains('\u{2026}'),
        "and the path is cut from the left, so its tail still names the worktree: {row}"
    );
    assert!(
        !row.contains("feature/login"),
        "the branch column is what gets dropped at this width, not the path: {row}"
    );
}

// ---------------------------------------------------------------------------
// Width discipline
// ---------------------------------------------------------------------------

#[test]
fn no_line_ever_exceeds_the_width_it_was_given() {
    let inventory = full_inventory();
    let six_repositories = six_repository_inventory();
    for columns in [MIN_COLUMNS, 40, 80, 100, 200] {
        for (name, text) in [
            ("table", table(&inventory, columns)),
            ("summary", summary(&inventory, columns)),
            (
                "six-repository summary",
                summary(&six_repositories, columns),
            ),
            ("empty table", table(&empty_inventory(), columns)),
            ("empty summary", summary(&empty_inventory(), columns)),
        ] {
            for (index, width) in widths_of(&text).into_iter().enumerate() {
                assert!(
                    width <= columns,
                    "{name} line {index} is {width} columns wide at {columns}"
                );
            }
        }
    }
}

#[test]
fn every_width_from_the_minimum_up_holds_the_line() {
    let inventory = full_inventory();
    for columns in MIN_COLUMNS..=200 {
        let widths = widths_for(&inventory, columns);
        assert!(
            widths.total() <= columns,
            "widths total {} at {columns} columns: {widths:?}",
            widths.total()
        );
        for width in widths_of(&table(&inventory, columns)) {
            assert!(width <= columns, "a line ran to {width} at {columns}");
        }
    }
}

#[test]
fn a_width_below_the_minimum_is_clamped_not_honoured() {
    // Nothing readable fits in twelve columns, so the table renders at
    // MIN_COLUMNS. What it must never do is silently emit a 60-column line.
    let inventory = full_inventory();
    for width in widths_of(&table(&inventory, 12)) {
        assert!(width <= MIN_COLUMNS, "a line ran to {width}");
    }
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

#[test]
fn a_path_too_long_for_its_column_keeps_its_tail() {
    let inventory = full_inventory();
    let rendered = table(&inventory, 100);
    let line = rendered
        .lines()
        .find(|line| line.contains("release-candidate"))
        .expect("the long path's row is rendered");
    assert!(
        line.contains('\u{2026}'),
        "a truncated path is marked with an ellipsis: {line}"
    );
    assert!(
        line.trim_end().ends_with("2026-01-release-candidate"),
        "the informative half of the path survives: {line}"
    );
    assert!(
        !line.contains("/home/dev"),
        "the head of the path is what was dropped: {line}"
    );
}

#[test]
fn labels_truncate_from_the_right_and_paths_from_the_left() {
    assert_eq!(truncate_right("feature/login", 8), "feature\u{2026}");
    assert_eq!(truncate_left("/home/dev/repos/app", 8), "\u{2026}pos/app");
    assert_eq!(
        truncate_left("/home/dev/repos/app", 100),
        "/home/dev/repos/app"
    );
    assert_eq!(truncate_right("abc", 0), "");
    assert_eq!(truncate_left("abc", 1), "\u{2026}");
}

#[test]
fn display_width_counts_columns_not_bytes() {
    assert_eq!(display_width("abc"), 3);
    // Two-column CJK, a zero-width combining mark, and a colour escape that
    // occupies nothing at all.
    assert_eq!(display_width("日本"), 4);
    assert_eq!(display_width("e\u{0301}"), 1);
    assert_eq!(display_width("\u{1b}[31mred\u{1b}[0m"), 3);
}

// ---------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------

#[test]
fn a_failed_measurement_never_reads_as_a_plausible_zero() {
    assert_eq!(size_cell(Size::Failed), "?");
    assert_ne!(size_cell(Size::Failed), "0 B");

    let inventory = full_inventory();
    let rendered = table(&inventory, 100);
    let line = rendered
        .lines()
        .find(|line| line.contains("spike-wasm"))
        .expect("the failed row is rendered");
    assert!(line.contains(" ?"), "the failed row shows `?`: {line}");
    assert!(
        !rendered.contains("0 B"),
        "no row invents a zero for a measurement that did not happen"
    );
}

#[test]
fn the_five_size_states_are_five_different_cells() {
    assert_eq!(size_cell(Size::Pending), "\u{2026}");
    assert_eq!(size_cell(Size::Gone), "-");
    assert_eq!(size_cell(Size::Failed), "?");
    assert_eq!(size_cell(Size::Bytes(0)), "0 B");
    assert_eq!(size_cell(Size::Bytes(512)), "512 B");
    assert_eq!(size_cell(Size::Bytes(12_288)), "12 kB");
    assert_eq!(size_cell(Size::Bytes(1_310_000_000)), "1.2 GB");
    // A provisional figure is last run's claim, marked so it can never be
    // read as a measurement — and it is not the pending leader either, so a
    // user can tell "measuring, no idea yet" from "measuring, last time it
    // was about this".
    assert_eq!(size_cell(Size::Provisional(1_310_000_000)), "~1.2 GB");
    assert_ne!(
        size_cell(Size::Provisional(0)),
        size_cell(Size::Bytes(0)),
        "a claim and a measurement must never render alike"
    );
}

#[test]
fn bytes_are_truncated_rather_than_rounded_up() {
    // 1.29 GB must not read as 1.3 GB: this number's job is to be a floor the
    // user can check against `df`.
    assert_eq!(render::human_bytes(1_385_126_297), "1.2 GB");
}

// ---------------------------------------------------------------------------
// Merged-ness
// ---------------------------------------------------------------------------

#[test]
fn an_unanswerable_merge_question_never_reads_as_not_merged() {
    let inventory = full_inventory();
    let rendered = table(&inventory, 100);

    let unknown = rendered
        .lines()
        .find(|line| line.contains("app-wt/bisect"))
        .expect("the detached row is rendered");
    assert!(
        unknown.contains("merged?"),
        "an unasked question is marked unknown: {unknown}"
    );

    // A row that *was* tested and is not contained says nothing at all in the
    // classes column, and must never borrow the unknown marker.
    let tested = rendered
        .lines()
        .find(|line| line.contains("hotfix-payments"))
        .expect("the tested row is rendered");
    assert!(!tested.contains("merged"), "no merge token: {tested}");

    let merged = rendered
        .lines()
        .find(|line| line.contains("feature-login"))
        .expect("the merged row is rendered");
    assert!(
        merged.contains("merged"),
        "the merged row says so: {merged}"
    );
    assert!(!merged.contains("merged?"), "and says it plainly: {merged}");

    // The legend wraps, so it is matched with the line breaks flattened out.
    let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flattened.contains("merged? — shear could not ask"),
        "the legend explains what `merged?` means"
    );
    assert!(
        flattened.contains("not the same as `not merged`"),
        "and says what it is not"
    );
}

#[test]
fn the_legend_appears_only_when_something_needs_it() {
    let mut inventory = full_inventory();
    inventory
        .candidates
        .retain(|candidate| candidate.merged != Merged::Unknown);
    assert!(!table(&inventory, 100).contains("merged?"));
}

// ---------------------------------------------------------------------------
// Ages
// ---------------------------------------------------------------------------

#[test]
fn ages_are_compact_and_never_wider_than_four_columns() {
    let day = 86_400;
    for (seconds, expected) in [
        (0, "<1h"),
        (3_599, "<1h"),
        (3_600, "1h"),
        (5 * 3_600, "5h"),
        (3 * day, "3d"),
        (13 * day, "13d"),
        (14 * day, "2w"),
        (55 * day, "7w"),
        (56 * day, "2mo"),
        (60 * day, "2mo"),
        // Eleven months is the last month before a year: `days / 30` would call
        // this "12mo", which is a year by another name.
        (364 * day, "11mo"),
        (365 * day, "1y"),
        (900 * day, "2y"),
    ] {
        let rendered = human_age(Some(Duration::from_secs(seconds)));
        assert_eq!(rendered, expected, "{seconds}s");
        assert!(display_width(&rendered) <= 4, "{rendered} is too wide");
    }
    assert_eq!(human_age(None), "-");
    assert!(display_width(&human_age(Some(Duration::from_secs(400_000 * day)))) <= 4);
}

// ---------------------------------------------------------------------------
// Grouping and summary
// ---------------------------------------------------------------------------

#[test]
fn rows_are_grouped_by_repository_and_never_mixed() {
    let inventory = full_inventory();
    let groups = render::grouped(&inventory);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].0, RepoKey(format!("{APP}/.git")));
    for (key, group) in &groups {
        for candidate in group {
            assert_eq!(&candidate.worktree.repo, key);
        }
    }
    // Safe first, then review, then keep, then blocked: the rows a user came for
    // are at the top of their group.
    let verdicts: Vec<Verdict> = groups[0].1.iter().map(|c| c.verdict).collect();
    let mut sorted = verdicts.clone();
    sorted.sort();
    assert_eq!(verdicts, sorted);
}

#[test]
fn the_summary_counts_the_rows_it_could_not_measure() {
    let inventory = full_inventory();
    let rendered = summary(&inventory, 100);
    assert!(rendered.contains("9 worktrees in 2 repositories"));
    assert!(rendered.contains("1 safe worktree"));
    // Two rows are unmeasured (pending and failed); `Gone` is measured and
    // reclaims nothing.
    assert!(
        rendered.contains("2 worktrees could not be measured"),
        "the summary says how many are missing rather than undercounting quietly: {rendered}"
    );
    assert!(rendered.contains("floor"), "and calls the total a floor");
    assert!(
        rendered.contains("only the checkout goes"),
        "the safety sentence is under every table"
    );
}

#[test]
fn a_single_repository_does_not_repeat_the_global_summary() {
    let inventory = Inventory {
        repos: vec![repo(APP, "app")],
        candidates: vec![
            Row::branch(APP, "/home/dev/repos/app-wt/cleanup", "cleanup")
                .verdict(Verdict::Safe)
                .size(Size::Bytes(2 * 1024))
                .build(),
        ],
        notes: Vec::new(),
    };
    let expected = "\
1 worktree in 1 repository: 1 safe worktree, 0 review, 0 keep, 0 blocked.
Removing the 1 safe worktree would reclaim 2.0 kB of the 2.0 kB all 1 worktree occupy.
Removing a worktree leaves its branch and every commit on it intact: only the checkout goes.
";
    assert_eq!(summary(&inventory, 200), expected);
}

#[test]
fn six_repositories_are_ordered_by_reclaimable_bytes() {
    let rendered = summary(&six_repository_inventory(), 200);
    let names: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with("  repository-"))
        .map(|line| {
            line.trim_start()
                .split_once(':')
                .expect("repository summary has a name")
                .0
        })
        .collect();
    assert_eq!(
        names,
        [
            "repository-six-with-an-intentionally-long-display-name",
            "repository-five-with-an-intentionally-long-display-name",
            "repository-four-with-an-intentionally-long-display-name",
            "repository-three-with-an-intentionally-long-display-name",
            "repository-two-with-an-intentionally-long-display-name",
            "repository-one-with-an-intentionally-long-display-name",
        ]
    );
}

#[test]
fn six_repositories_are_one_line_each_at_eighty_columns() {
    let inventory = six_repository_inventory();
    let repository_keys: BTreeSet<_> = inventory
        .candidates
        .iter()
        .map(|candidate| &candidate.worktree.repo)
        .collect();
    let rendered = summary(&inventory, 80);
    let lines: Vec<_> = rendered.lines().collect();
    let block_start = lines
        .iter()
        .position(|line| line.starts_with("  "))
        .expect("the per-repository block is rendered");
    let block_end = lines
        .iter()
        .position(|line| line.starts_with("Removing a worktree"))
        .expect("the safety note follows the per-repository block");
    assert_eq!(
        block_end - block_start,
        repository_keys.len(),
        "each repository occupies exactly one line:\n{rendered}"
    );
}

#[test]
fn registered_repositories_without_candidates_have_no_summary_line() {
    let first_root = "/home/dev/repos/first";
    let second_root = "/home/dev/repos/second";
    let candidate_free_root = "/home/dev/repos/candidate-free";
    let inventory = Inventory {
        repos: vec![
            repo(first_root, "first"),
            repo(second_root, "second"),
            repo(candidate_free_root, "candidate-free"),
        ],
        candidates: vec![
            Row::branch(
                first_root,
                "/home/dev/repos/first/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(1024))
            .build(),
            Row::branch(
                second_root,
                "/home/dev/repos/second/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(2048))
            .build(),
        ],
        notes: Vec::new(),
    };
    let rendered = summary(&inventory, 200);
    let repository_lines: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with("  "))
        .collect();
    assert_eq!(
        repository_lines.len(),
        2,
        "only repositories with candidates get summary lines:\n{rendered}"
    );
    assert!(
        repository_lines
            .iter()
            .any(|line| line.starts_with("  first:")),
        "the first repository is rendered: {rendered}"
    );
    assert!(
        repository_lines
            .iter()
            .any(|line| line.starts_with("  second:")),
        "the second repository is rendered: {rendered}"
    );
    assert!(
        !rendered.contains("candidate-free"),
        "a registered repository without candidates is absent: {rendered}"
    );
}

#[test]
fn unregistered_repositories_with_candidates_use_the_repo_root_name() {
    let registered_root = "/home/dev/repos/registered-root";
    let unregistered_root = "/home/dev/repos/unregistered-fallback";
    let inventory = Inventory {
        repos: vec![repo(registered_root, "registered-name")],
        candidates: vec![
            Row::branch(
                registered_root,
                "/home/dev/repos/registered-root/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(1024))
            .build(),
            Row::branch(
                unregistered_root,
                "/home/dev/repos/unregistered-fallback/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(2048))
            .build(),
        ],
        notes: Vec::new(),
    };
    let rendered = summary(&inventory, 200);
    let repository_lines: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with("  "))
        .collect();
    assert_eq!(
        repository_lines.len(),
        2,
        "both repositories with candidates get summary lines:\n{rendered}"
    );
    assert!(
        repository_lines
            .iter()
            .any(|line| line.starts_with("  registered-name:")),
        "the registered repository uses its configured name: {rendered}"
    );
    assert!(
        repository_lines
            .iter()
            .any(|line| line.starts_with("  unregistered-fallback:")),
        "the unregistered repository falls back to its repo root name: {rendered}"
    );
}

#[test]
fn repositories_with_equal_bytes_are_ordered_by_root() {
    let inventory = Inventory {
        repos: vec![
            repo("/home/dev/repos/zeta-root", "zeta"),
            repo("/home/dev/repos/alpha-root", "alpha"),
        ],
        candidates: vec![
            Row::branch(
                "/home/dev/repos/zeta-root",
                "/home/dev/repos/zeta-root/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(1024))
            .build(),
            Row::branch(
                "/home/dev/repos/alpha-root",
                "/home/dev/repos/alpha-root/worktrees/cleanup",
                "cleanup",
            )
            .size(Size::Bytes(1024))
            .build(),
        ],
        notes: Vec::new(),
    };
    let first = summary(&inventory, 200);
    let second = summary(&inventory, 200);
    assert_eq!(first, second);
    assert!(
        first
            .find("  alpha:")
            .expect("alpha repository is rendered")
            < first.find("  zeta:").expect("zeta repository is rendered"),
        "the root, not discovery order, breaks the tie: {first}"
    );
}

#[test]
fn a_fully_unmeasured_repository_never_claims_a_measured_zero() {
    let unknown_root = "/home/dev/repos/unknown";
    let known_root = "/home/dev/repos/known";
    let inventory = Inventory {
        repos: vec![repo(unknown_root, "unknown"), repo(known_root, "known")],
        candidates: vec![
            Row::branch(
                unknown_root,
                "/home/dev/repos/unknown/worktrees/pending",
                "pending",
            )
            .size(Size::Pending)
            .build(),
            Row::branch(
                unknown_root,
                "/home/dev/repos/unknown/worktrees/failed",
                "failed",
            )
            .size(Size::Failed)
            .build(),
            Row::branch(
                known_root,
                "/home/dev/repos/known/worktrees/measured",
                "measured",
            )
            .size(Size::Bytes(2048))
            .build(),
        ],
        notes: Vec::new(),
    };
    let rendered = summary(&inventory, 200);
    let line = rendered
        .lines()
        .find(|line| line.starts_with("  unknown:"))
        .expect("unknown repository is rendered");
    assert!(
        line.contains("nothing safe to remove · 0 B+ total (2 unmeasured)"),
        "{line}"
    );
    assert!(!line.contains("0 B total"), "{line}");
}

#[test]
fn safe_counts_and_bytes_are_per_repository() {
    let safe_root = "/home/dev/repos/with-safe";
    let none_root = "/home/dev/repos/without-safe";
    let inventory = Inventory {
        repos: vec![
            repo(safe_root, "with-safe"),
            repo(none_root, "without-safe"),
        ],
        candidates: vec![
            Row::branch(
                safe_root,
                "/home/dev/repos/with-safe/worktrees/safe",
                "safe",
            )
            .verdict(Verdict::Safe)
            .size(Size::Bytes(1024 * 1024))
            .build(),
            Row::branch(
                safe_root,
                "/home/dev/repos/with-safe/worktrees/gone",
                "gone",
            )
            .size(Size::Gone)
            .build(),
            Row::branch(
                none_root,
                "/home/dev/repos/without-safe/worktrees/keep",
                "keep",
            )
            .size(Size::Bytes(2 * 1024 * 1024))
            .build(),
        ],
        notes: Vec::new(),
    };
    let rendered = summary(&inventory, 200);
    let with_safe = rendered
        .lines()
        .find(|line| line.starts_with("  with-safe:"))
        .expect("safe repository is rendered");
    assert!(
        with_safe.contains("2 worktrees, 1 safe · 1.0 MB reclaimable of 1.0 MB"),
        "{with_safe}"
    );
    let without_safe = rendered
        .lines()
        .find(|line| line.starts_with("  without-safe:"))
        .expect("repository without safe rows is rendered");
    assert!(
        without_safe.contains("1 worktree, 0 safe · nothing safe to remove · 2.0 MB total"),
        "{without_safe}"
    );
    assert!(
        !rendered.contains("0 B reclaimable"),
        "an unmeasured reclaimable figure is never invented: {rendered}"
    );
}

#[test]
fn an_empty_inventory_says_so_rather_than_printing_a_bare_table() {
    let rendered = table(&empty_inventory(), 80);
    assert!(rendered.contains("no worktrees found."));
    assert!(!rendered.contains("verdict"));
}

#[test]
fn scan_notes_are_never_swallowed() {
    let rendered = table(&full_inventory(), 100);
    assert!(rendered.contains("notes"));
    assert!(rendered.contains("herdr is not reachable"));
    // A per-worktree note travels with its row.
    assert!(rendered.contains("gitdir file points to non-existent location"));
}
