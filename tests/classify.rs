//! Classification, against real repositories.
//!
//! Every case here is a repository git built, read back through the real git
//! layer, and classified by the real classifier. The verdict *and* the class set
//! are asserted for each, because the table shows both and a right verdict for
//! the wrong reason is a bug waiting to be trusted.
//!
//! The negative cases are the point. `Safe` is the only verdict a bulk action
//! may preselect, so the tests that matter most are the ones proving something
//! is *not* safe: a dirty worktree that is merged and gone, a repository where
//! the merged question cannot be asked at all, and a prunable worktree that is
//! clean by construction.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use shear::classify::{self, Facts};
use shear::config::Config;
use shear::git;
use shear::model::{
    Candidate, Class, Dirt, Head, Inventory, LockInfo, Merged, OpenWorkspace, PrunableInfo,
    RepoKey, Upstream, Verdict, Worktree,
};

use fixtures::{pin_git_env, Fixture};

const TIMEOUT: Duration = Duration::from_secs(60);

fn config(repo: &Path) -> Config {
    Config {
        // The session is replaced entirely, so no test ever reaches the user's
        // own repositories or the current working directory.
        only_repos: vec![repo.to_path_buf()],
        measure_disk: false,
        ..Config::default()
    }
}

fn scan(repo: &Path) -> Inventory {
    shear::shear::scan(&config(repo)).expect("scan the fixture")
}

fn at<'a>(inventory: &'a Inventory, path: &Path) -> &'a Candidate {
    // `resolve` rather than `find`: it accepts either the exact path git
    // reported or any path that canonicalizes to it, which is what a scratch
    // directory behind a symlink needs.
    shear::shear::resolve(inventory, path).unwrap_or_else(|| {
        panic!(
            "no candidate for {}; scanned: {:?}",
            path.display(),
            inventory
                .candidates
                .iter()
                .map(|c| c.path().display().to_string())
                .collect::<Vec<_>>()
        )
    })
}

fn classes(list: &[Class]) -> BTreeSet<Class> {
    list.iter().copied().collect()
}

/// Asserts the verdict and the whole class set at once, so a new class quietly
/// appearing on a row is a failure rather than an unnoticed change of meaning.
fn assert_row(candidate: &Candidate, verdict: Verdict, expected: &[Class]) {
    assert_eq!(
        (candidate.verdict, candidate.classes.clone()),
        (verdict, classes(expected)),
        "{}: reason was {:?}",
        candidate.path().display(),
        candidate.reason
    );
}

// ---------------------------------------------------------------------------
// One repository holding every class
// ---------------------------------------------------------------------------

/// Every class in one repository, scanned once. Building each shape in its own
/// fixture would be slower and would also stop testing the thing that actually
/// goes wrong: classes are decided per worktree from repo-wide facts, and a
/// repo-wide fact computed once has to be right for all of them.
struct Sink {
    fixture: Fixture,
    inventory: Inventory,
    safe: PathBuf,
    merged: PathBuf,
    active: PathBuf,
    dirty: PathBuf,
    stale: PathBuf,
    locked: PathBuf,
    locked_bare: PathBuf,
    detached: PathBuf,
    prunable: PathBuf,
    broken: PathBuf,
    unborn: PathBuf,
}

fn kitchen_sink(tag: &str) -> Sink {
    pin_git_env();
    let fixture = Fixture::new(tag);
    // Order matters, and it is the repository's real history rather than a test
    // convenience: `merged_worktree` merges into local `main` without pushing,
    // and `safe_worktree` pushes `main` afterwards. Built the other way round,
    // `merged-branch` would not be contained in `origin/main` — which is the
    // integration ref — and the fixture would be testing a different thing than
    // its name says.
    let merged = fixture.merged_worktree("merged");
    let safe = fixture.safe_worktree("safe");
    let active = fixture.active_worktree("active");
    let dirty = fixture.dirty_worktree("dirty");
    let stale = fixture.stale_worktree("stale", 400);
    let locked = fixture.locked_worktree("locked", "held for demo");
    let locked_bare = fixture.locked_worktree_no_reason("locked2");
    let detached = fixture.detached_worktree("detached");
    let prunable = fixture.prunable_worktree("prunable");
    let broken = fixture.broken_head_worktree("broken");
    let unborn = fixture.unborn_worktree("unborn");

    let inventory = scan(&fixture.repo);
    Sink {
        inventory,
        safe,
        merged,
        active,
        dirty,
        stale,
        locked,
        locked_bare,
        detached,
        prunable,
        broken,
        unborn,
        fixture,
    }
}

#[test]
fn the_only_safe_shape_is_clean_merged_and_gone() {
    let sink = kitchen_sink("safe");
    let safe = at(&sink.inventory, &sink.safe);

    assert_row(safe, Verdict::Safe, &[Class::Merged, Class::GoneUpstream]);
    assert_eq!(safe.merged, Merged::Into("origin/main".into()));
    assert!(safe.upstream.gone);
    assert!(!safe.dirt.is_dirty());
    assert_eq!(safe.branch(), Some("safe-branch"));
    // The detail line has to be specific enough to act on.
    assert!(
        safe.reason.contains("merged into origin/main")
            && safe.reason.contains("gone")
            && safe.reason.contains("clean"),
        "{}",
        safe.reason
    );

    // And it is the only one in the repository. `safe()` is what a bulk action
    // preselects, so the count is the whole product.
    let safe_paths: Vec<&Path> = sink.inventory.safe().map(|c| c.path()).collect();
    assert_eq!(safe_paths, vec![sink.safe.as_path()]);
}

#[test]
fn merged_but_with_a_live_upstream_is_review_never_safe() {
    let sink = kitchen_sink("merged");
    let merged = at(&sink.inventory, &sink.merged);

    assert_row(merged, Verdict::Review, &[Class::Merged]);
    assert_eq!(merged.merged, Merged::Into("origin/main".into()));
    assert!(
        !merged.upstream.gone,
        "the remote branch is still there, so the work may not have landed"
    );
    assert!(
        merged.reason.contains("still exists"),
        "the row has to say why it is not safe: {}",
        merged.reason
    );
}

#[test]
fn an_active_worktree_is_kept() {
    let sink = kitchen_sink("active");
    let active = at(&sink.inventory, &sink.active);

    assert_row(active, Verdict::Keep, &[]);
    assert_eq!(active.merged, Merged::No("origin/main".into()));
    assert!(!active.upstream.gone);
    assert!(active.upstream.name.is_some());
    assert!(
        active.reason.contains("not merged into origin/main"),
        "{}",
        active.reason
    );
}

#[test]
fn a_dirty_worktree_names_what_is_at_risk() {
    let sink = kitchen_sink("dirty");
    let dirty = at(&sink.inventory, &sink.dirty);

    // Its branch was cut from the main tip, so it is contained in `main` and
    // carries `Merged` too.
    assert_row(dirty, Verdict::Review, &[Class::Dirty, Class::Merged]);
    assert_eq!(
        dirty.dirt,
        Dirt {
            staged: 0,
            unstaged: 1,
            untracked: 1,
            unmerged: 0,
        }
    );
    assert!(
        dirty.reason.contains("1 unstaged") && dirty.reason.contains("1 untracked"),
        "the second confirmation names these numbers: {}",
        dirty.reason
    );
}

#[test]
fn a_stale_branch_is_review_on_its_age_alone() {
    let sink = kitchen_sink("stale");
    let stale = at(&sink.inventory, &sink.stale);

    assert_row(stale, Verdict::Review, &[Class::Stale]);
    assert_eq!(
        stale.merged,
        Merged::No("origin/main".into()),
        "it has a commit main does not have"
    );
    assert_eq!(stale.upstream.name, None, "it was never pushed");
    assert!(!stale.upstream.gone, "never pushed is not the same as gone");
    assert!(
        stale.reason.contains("staleness window"),
        "{}",
        stale.reason
    );
}

#[test]
fn a_locked_worktree_is_blocked_and_the_row_says_how_to_unblock() {
    let sink = kitchen_sink("locked");

    let with_reason = at(&sink.inventory, &sink.locked);
    assert_row(
        with_reason,
        Verdict::Blocked,
        &[Class::Locked, Class::Merged],
    );
    assert_eq!(
        with_reason.worktree.locked,
        Some(LockInfo {
            reason: Some("held for demo".into())
        })
    );
    assert!(
        with_reason.reason.contains("held for demo")
            && with_reason.reason.contains("git worktree unlock"),
        "{}",
        with_reason.reason
    );

    // A lock taken with no `--reason` is the bare word `locked`, and the row has
    // to say that rather than showing an empty parenthesis.
    let bare = at(&sink.inventory, &sink.locked_bare);
    assert_row(bare, Verdict::Blocked, &[Class::Locked, Class::Merged]);
    assert_eq!(bare.worktree.locked, Some(LockInfo { reason: None }));
    assert!(
        bare.reason.contains("no reason given") && bare.reason.contains("git worktree unlock"),
        "{}",
        bare.reason
    );
}

#[test]
fn a_detached_head_is_classified_on_its_commit() {
    let sink = kitchen_sink("detached");
    let detached = at(&sink.inventory, &sink.detached);

    assert_eq!(detached.worktree.head, Head::Detached);
    assert_eq!(detached.branch(), None);
    // It sits on the main tip, so `merge-base --is-ancestor` says contained —
    // the question a detached HEAD is asked instead of "is this branch merged".
    assert_row(detached, Verdict::Review, &[Class::Merged]);
    assert_eq!(detached.merged, Merged::Into("origin/main".into()));
    assert!(
        !detached.upstream.gone,
        "a detached HEAD has no branch, so it can never be gone-upstream, and so \
         can never be safe"
    );
}

#[test]
fn a_prunable_worktree_is_review_and_never_safe() {
    let sink = kitchen_sink("prunable");
    let prunable = at(&sink.inventory, &sink.prunable);

    assert_row(prunable, Verdict::Review, &[Class::Prunable, Class::Merged]);
    assert_eq!(
        prunable.worktree.prunable,
        Some(PrunableInfo {
            reason: Some("gitdir file points to non-existent location".into())
        })
    );
    // Clean by construction — the directory is not there to be dirty — and still
    // not safe, because that reason is also what an unmounted filesystem looks
    // like. git's own words are shown so the user can tell them apart.
    assert!(!prunable.dirt.is_dirty());
    assert_ne!(prunable.verdict, Verdict::Safe);
    assert!(
        prunable
            .reason
            .contains("gitdir file points to non-existent location"),
        "{}",
        prunable.reason
    );
}

#[test]
fn a_broken_head_is_distinguished_from_an_unborn_one() {
    let sink = kitchen_sink("broken");

    let broken = at(&sink.inventory, &sink.broken);
    assert_eq!(broken.worktree.head, Head::Unborn);
    assert_eq!(broken.worktree.head_oid, None);
    assert!(
        broken
            .worktree
            .notes
            .iter()
            .any(|note| note.contains(git::NOTE_BROKEN_HEAD)),
        "a worktree whose branch was deleted underneath it has a logs/HEAD: {:?}",
        broken.worktree.notes
    );
    assert_eq!(
        broken.merged,
        Merged::Unknown,
        "there is no commit here to test"
    );
    assert_ne!(broken.verdict, Verdict::Safe);

    let unborn = at(&sink.inventory, &sink.unborn);
    assert_eq!(unborn.worktree.head, Head::Unborn);
    assert!(
        unborn
            .worktree
            .notes
            .iter()
            .any(|note| note.contains(git::NOTE_UNBORN)),
        "a worktree that never had a commit has no logs/HEAD: {:?}",
        unborn.worktree.notes
    );
    assert_ne!(unborn.verdict, Verdict::Safe);
    // The same porcelain record shape, two different facts, told apart only by
    // the worktree's own reflog.
    assert_ne!(broken.worktree.notes, unborn.worktree.notes);
}

#[test]
fn the_main_checkout_is_blocked_whatever_else_is_true_of_it() {
    let sink = kitchen_sink("main");
    let main = at(&sink.inventory, &sink.fixture.repo);

    assert!(main.worktree.is_main);
    assert_eq!(main.verdict, Verdict::Blocked);
    assert!(main.reason.contains("main checkout"), "{}", main.reason);
    assert!(
        !sink
            .inventory
            .safe()
            .any(|candidate| candidate.worktree.is_main),
        "the main checkout must never be preselectable"
    );
}

// ---------------------------------------------------------------------------
// The negatives
// ---------------------------------------------------------------------------

/// The rule that carries the whole product: dirt beats every death signal.
#[test]
fn a_dirty_worktree_is_never_safe_even_when_merged_and_gone() {
    pin_git_env();
    let fixture = Fixture::new("dirty-safe-shaped");
    let path = fixture.safe_worktree("safe");
    // Exactly the safe shape, plus one file nobody has committed.
    fixture.write(&path, "scratch.txt", "unsaved work\n");

    let inventory = scan(&fixture.repo);
    let candidate = at(&inventory, &path);

    assert_eq!(candidate.merged, Merged::Into("origin/main".into()));
    assert!(candidate.upstream.gone);
    assert!(candidate.is(Class::Merged) && candidate.is(Class::GoneUpstream));
    assert_row(
        candidate,
        Verdict::Review,
        &[Class::Dirty, Class::Merged, Class::GoneUpstream],
    );
    assert_eq!(inventory.safe().count(), 0);
}

/// A repository whose only branch is `trunk`, with no remote: `origin/HEAD` is
/// unset and none of the default guesses resolve, so the merged question cannot
/// be asked at all. That must degrade to `Unknown`, never to "not merged", and
/// nothing in the repository may be safe.
#[test]
fn a_repo_with_no_integration_ref_produces_no_safe_rows() {
    pin_git_env();
    let fixture = Fixture::new("no-integration");
    let repo = fixture.no_integration_repo("orphaned");
    let worktree = fixture.root().join("wt-orphaned-topic");
    let worktree_arg = worktree.to_string_lossy().into_owned();
    fixture.git(
        &repo,
        &["worktree", "add", "-q", &worktree_arg, "-b", "topic"],
    );

    assert_eq!(
        git::integration_ref(&repo, None, TIMEOUT).expect("resolve"),
        None,
        "neither origin/HEAD nor any default guess may resolve here"
    );

    let inventory = scan(&repo);
    assert!(
        inventory
            .notes
            .iter()
            .any(|note| note.contains(git::NOTE_NO_INTEGRATION_REF)),
        "the scan has to say the question could not be asked: {:?}",
        inventory.notes
    );

    for candidate in &inventory.candidates {
        assert_eq!(
            candidate.merged,
            Merged::Unknown,
            "{} reported {:?} where the question could not be asked",
            candidate.path().display(),
            candidate.merged
        );
        assert!(!candidate.is(Class::Merged));
    }
    assert_eq!(inventory.safe().count(), 0);

    let topic = at(&inventory, &worktree);
    assert_eq!(topic.verdict, Verdict::Keep);
    assert!(
        topic.reason.contains("cannot tell whether it is merged"),
        "an unanswerable question must not be rendered as a negative answer: {}",
        topic.reason
    );
}

/// Worktrees are never grouped or compared across repositories: a second,
/// unrelated repo has a different `--git-common-dir` and is simply not in scope.
#[test]
fn a_foreign_repo_is_never_grouped_with_this_one() {
    pin_git_env();
    let fixture = Fixture::new("foreign");
    let safe = fixture.safe_worktree("safe");
    let foreign = fixture.foreign_repo("elsewhere");

    let ours = git::repo_key(&fixture.repo, TIMEOUT).expect("our key");
    let theirs = git::repo_key(&foreign, TIMEOUT).expect("their key");
    assert_ne!(ours, theirs);

    let inventory = scan(&fixture.repo);
    assert!(inventory.candidates.iter().all(|c| c.worktree.repo == ours));
    assert!(inventory.find(&foreign).is_none());
    assert_eq!(at(&inventory, &safe).worktree.repo, ours);
}

// ---------------------------------------------------------------------------
// The pure rules, driven directly
// ---------------------------------------------------------------------------

/// Real facts read from a real repository, with one fact substituted. herdr's
/// side cannot be built by git, and re-deriving the rest by hand would be
/// testing the fixture rather than the classifier.
fn facts_for(inventory: &Inventory, path: &Path) -> Facts {
    let candidate = at(inventory, path).clone();
    Facts {
        worktree: candidate.worktree,
        dirt: candidate.dirt,
        upstream: candidate.upstream,
        merged: candidate.merged,
        last_commit: candidate.last_commit,
        open_workspace: candidate.open_workspace,
        protected: None,
    }
}

#[test]
fn a_worktree_open_in_a_herdr_workspace_is_blocked() {
    let sink = kitchen_sink("open-in-herdr");
    // The safe shape, which is otherwise the one thing that would be
    // preselected: a workspace holding it open has to be enough on its own.
    let mut facts = facts_for(&sink.inventory, &sink.safe);
    assert_eq!(
        classify::verdict_of(
            &facts,
            &classify::classes_of(&facts, week(2), SystemTime::now())
        ),
        Verdict::Safe
    );

    facts.open_workspace = Some(OpenWorkspace {
        workspace_id: "ws-7".into(),
        label: "review pane".into(),
    });
    let candidate = classify::classify(facts, week(2), SystemTime::now());

    assert_row(
        &candidate,
        Verdict::Blocked,
        &[Class::Merged, Class::GoneUpstream, Class::OpenInHerdr],
    );
    assert!(
        candidate.reason.contains("review pane") && candidate.reason.contains("close"),
        "the row names the unblocking action: {}",
        candidate.reason
    );
    let signals = classify::signals(&candidate, SystemTime::now());
    assert!(
        signals.iter().any(|signal| {
            signal == "open in the herdr workspace review pane; close it to unblock"
        }),
        "{signals:?}"
    );
}

/// The three merged states, pinned side by side. `Unknown` is the one that
/// matters: it must never satisfy a positive condition, even when every other
/// condition for `Safe` is met.
#[test]
fn merged_unknown_never_satisfies_safe() {
    let base = Facts {
        worktree: bare_worktree(),
        dirt: Dirt::default(),
        upstream: Upstream {
            name: Some("refs/remotes/origin/topic".into()),
            gone: true,
            ahead: 0,
            behind: 0,
        },
        merged: Merged::Into("origin/main".into()),
        last_commit: Some(SystemTime::now()),
        open_workspace: None,
        protected: None,
    };
    let now = SystemTime::now();

    // Everything else identical; only the merged state changes.
    let into = classify::classify(base.clone(), week(2), now);
    assert_row(&into, Verdict::Safe, &[Class::Merged, Class::GoneUpstream]);
    assert!(into.reason.contains("merged into origin/main"));

    let no = classify::classify(
        Facts {
            merged: Merged::No("origin/main".into()),
            ..base.clone()
        },
        week(2),
        now,
    );
    assert_row(&no, Verdict::Review, &[Class::GoneUpstream]);
    assert!(
        no.reason.contains("not merged into origin/main"),
        "{}",
        no.reason
    );

    let unknown = classify::classify(
        Facts {
            merged: Merged::Unknown,
            ..base
        },
        week(2),
        now,
    );
    assert_row(&unknown, Verdict::Review, &[Class::GoneUpstream]);
    assert_ne!(
        unknown.verdict,
        Verdict::Safe,
        "an unanswerable question is not a yes"
    );
    assert!(!unknown.is(Class::Merged));
    assert!(
        unknown.reason.contains("cannot tell whether it is merged"),
        "and it must not be worded as a no: {}",
        unknown.reason
    );
}

/// A worktree with no known commit time is not stale. "I do not know when this
/// was last touched" is not "this was last touched a long time ago".
#[test]
fn an_unknown_commit_time_is_not_staleness() {
    let facts = Facts {
        worktree: bare_worktree(),
        dirt: Dirt::default(),
        upstream: Upstream::default(),
        merged: Merged::Unknown,
        last_commit: None,
        open_workspace: None,
        protected: None,
    };
    let candidate = classify::classify(facts, week(2), SystemTime::now());
    assert_row(&candidate, Verdict::Keep, &[]);
    assert!(!candidate.is(Class::Stale));
}

#[test]
fn signals_quote_the_git_facts_that_produced_them() {
    let sink = kitchen_sink("signals");
    let now = SystemTime::now();

    let dirty = classify::signals(at(&sink.inventory, &sink.dirty), now);
    assert!(
        dirty
            .iter()
            .any(|signal| signal == "1 unstaged, 1 untracked at risk"),
        "{dirty:?}"
    );

    let locked = classify::signals(at(&sink.inventory, &sink.locked), now);
    assert!(
        locked.iter().any(|signal| {
            signal.contains("held for demo") && signal.contains("git worktree unlock")
        }),
        "{locked:?}"
    );
    let locked_bare = classify::signals(at(&sink.inventory, &sink.locked_bare), now);
    assert!(
        locked_bare.iter().any(|signal| {
            signal.contains("no reason given")
                && signal.contains("git worktree unlock")
                && !signal.contains("()")
        }),
        "{locked_bare:?}"
    );

    let prunable = classify::signals(at(&sink.inventory, &sink.prunable), now);
    assert!(
        prunable
            .iter()
            .any(|signal| signal.contains("gitdir file points to non-existent location")),
        "{prunable:?}"
    );

    let safe = at(&sink.inventory, &sink.safe);
    let upstream = safe.upstream.name.as_deref().expect("safe upstream");
    let safe_signals = classify::signals(safe, now);
    assert_eq!(
        safe_signals,
        vec![
            format!("upstream {upstream} is gone"),
            "merged into origin/main".to_string(),
        ],
        "signals follow Class significance order"
    );

    let stale = at(&sink.inventory, &sink.stale);
    let age = now
        .duration_since(stale.last_commit.expect("stale tip time"))
        .expect("stale tip precedes now");
    let stale_signals = classify::signals(stale, now);
    assert!(
        stale_signals.iter().any(|signal| {
            signal == &format!("branch tip is {} old", shear::render::human_age(Some(age)))
        }),
        "{stale_signals:?}"
    );

    let main = classify::signals(at(&sink.inventory, &sink.fixture.repo), now);
    assert!(
        main.iter()
            .any(|signal| signal == "main checkout; never a removal candidate"),
        "{main:?}"
    );
}

#[test]
fn dirty_signals_quote_every_kind_of_dirt_in_significance_order() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let candidate = classify::classify(
        Facts {
            worktree: bare_worktree(),
            dirt: Dirt {
                staged: 2,
                unstaged: 3,
                untracked: 4,
                unmerged: 5,
            },
            upstream: Upstream::default(),
            merged: Merged::No("origin/main".into()),
            last_commit: Some(now),
            open_workspace: None,
            protected: None,
        },
        week(2),
        now,
    );

    let signals = classify::signals(&candidate, now);
    assert_eq!(
        signals.first().map(String::as_str),
        Some("2 staged, 3 unstaged, 4 untracked, 5 unmerged at risk")
    );
    assert!(
        signals
            .last()
            .is_some_and(|signal| signal.contains("not clean")),
        "{signals:?}"
    );
}

#[test]
fn protected_signals_quote_pattern_precede_dirt_and_name_the_unblocking_action() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let candidate = classify::classify(
        Facts {
            worktree: bare_worktree(),
            dirt: Dirt {
                unstaged: 1,
                ..Dirt::default()
            },
            upstream: Upstream::default(),
            merged: Merged::No("origin/main".into()),
            last_commit: Some(now),
            open_workspace: None,
            protected: Some("release/*".into()),
        },
        week(2),
        now,
    );

    let signals = classify::signals(&candidate, now);
    let protection = signals.first().expect("protected signal");
    assert!(protection.contains("`release/*`"), "{signals:?}");
    assert!(
        protection.contains("edit or remove that pattern in") && protection.contains("to unblock"),
        "{signals:?}"
    );
    assert_eq!(
        signals.get(1).map(String::as_str),
        Some("1 unstaged at risk"),
        "protection precedes dirt: {signals:?}"
    );
    assert!(
        signals.last().is_some_and(|signal| {
            signal.starts_with("not safe: protected by pattern `release/*`")
                && signal.contains("edit or remove that pattern in")
        }),
        "{signals:?}"
    );
}

#[test]
fn unprotected_signals_do_not_invent_a_protection_reason() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let candidate = classify::classify(
        Facts {
            worktree: bare_worktree(),
            dirt: Dirt::default(),
            upstream: Upstream::default(),
            merged: Merged::No("origin/main".into()),
            last_commit: Some(now),
            open_workspace: None,
            protected: None,
        },
        week(2),
        now,
    );

    let signals = classify::signals(&candidate, now);
    assert!(
        signals
            .iter()
            .all(|signal| !signal.contains("protected by pattern")),
        "{signals:?}"
    );
}

#[test]
fn an_unknown_merge_signal_says_the_question_was_not_askable() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let candidate = classify::classify(
        Facts {
            worktree: bare_worktree(),
            dirt: Dirt::default(),
            upstream: Upstream {
                name: Some("refs/remotes/origin/topic".into()),
                gone: true,
                ahead: 0,
                behind: 0,
            },
            merged: Merged::Unknown,
            last_commit: Some(now),
            open_workspace: None,
            protected: None,
        },
        week(2),
        now,
    );

    let signals = classify::signals(&candidate, now);
    assert!(
        signals.iter().any(
            |signal| signal.contains("merge question could not be asked")
                && signal.contains("no integration ref resolved here")
        ),
        "{signals:?}"
    );
    let rendered = signals.join("\n");
    assert!(!rendered.contains("not merged"), "{rendered}");
    assert!(!rendered.contains("merged into"), "{rendered}");
    assert!(
        signals
            .get(signals.len().saturating_sub(2))
            .is_some_and(|signal| signal.starts_with("merge question could not be asked")),
        "the unknown note comes after class signals and before the safe failure: {signals:?}"
    );
}

#[test]
fn no_commit_time_never_produces_a_stale_or_age_signal() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let candidate = classify::classify(
        Facts {
            worktree: bare_worktree(),
            dirt: Dirt::default(),
            upstream: Upstream::default(),
            merged: Merged::Unknown,
            last_commit: None,
            open_workspace: None,
            protected: None,
        },
        week(2),
        now,
    );

    let signals = classify::signals(&candidate, now);
    assert!(
        signals
            .iter()
            .all(|signal| !signal.contains("old") && !signal.contains("stale")),
        "{signals:?}"
    );
}

#[test]
fn only_non_safe_verdicts_end_with_the_first_failed_safe_condition() {
    let now = SystemTime::UNIX_EPOCH + week(100);
    let safe_facts = Facts {
        worktree: bare_worktree(),
        dirt: Dirt::default(),
        upstream: Upstream {
            name: Some("refs/remotes/origin/topic".into()),
            gone: true,
            ahead: 0,
            behind: 0,
        },
        merged: Merged::Into("origin/main".into()),
        last_commit: Some(now),
        open_workspace: None,
        protected: None,
    };
    let safe = classify::classify(safe_facts.clone(), week(2), now);
    let review = classify::classify(
        Facts {
            merged: Merged::No("origin/main".into()),
            ..safe_facts.clone()
        },
        week(2),
        now,
    );
    let keep = classify::classify(
        Facts {
            upstream: Upstream {
                name: Some("refs/remotes/origin/topic".into()),
                gone: false,
                ahead: 0,
                behind: 0,
            },
            merged: Merged::No("origin/main".into()),
            ..safe_facts
        },
        week(2),
        now,
    );

    let safe_signals = classify::signals(&safe, now);
    assert!(
        safe_signals
            .iter()
            .all(|signal| !signal.starts_with("not safe:")),
        "{safe_signals:?}"
    );
    for candidate in [&review, &keep] {
        let signals = classify::signals(candidate, now);
        assert!(
            signals.last().is_some_and(|signal| {
                signal.starts_with("not safe:") && signal.contains("merged into origin/main")
            }),
            "{:?}: {signals:?}",
            candidate.verdict
        );
    }
}

fn week(n: u64) -> Duration {
    Duration::from_secs(n * 7 * 86_400)
}

/// A minimal non-main worktree record, for the rules that are about the facts
/// around it rather than about the record itself.
fn bare_worktree() -> Worktree {
    Worktree {
        repo: RepoKey("/nowhere/.git".into()),
        repo_root: PathBuf::from("/nowhere"),
        path: PathBuf::from("/nowhere/wt-topic"),
        head: Head::Branch("topic".into()),
        head_oid: Some("1338d9a0776263fed7455760e9e973db9389a29e".into()),
        is_main: false,
        locked: None,
        prunable: None,
        notes: Vec::new(),
    }
}
