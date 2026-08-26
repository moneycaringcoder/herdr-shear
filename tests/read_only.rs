//! Proof of the plugin's central safety claim: reading a repository to decide
//! what could be deleted changes nothing in it.
//!
//! Every other test here checks that shear computes the right answer. These
//! check that computing it costs the user nothing — no index writeback, no
//! stray object, no touched file, no leftover lock. The claim used to live only
//! in the module doc of `git.rs`, and prose does not fail CI.
//!
//! The fingerprint deliberately covers more than the assertions strictly need:
//! the whole common git directory (minus the object store, which is compared by
//! name set), every index with its bytes *and* its mtime, every working-tree
//! file including untracked and ignored ones, every ref and reflog, and any
//! `*.lock`. Anything git writes shows up.
//!
//! Adapted from the same proof in herdr-collide, which has a harder job — it
//! snapshots indexes into a scratch ODB — and therefore a fingerprint worth
//! copying.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use shear::disk;
use shear::git;
use shear::model::{Candidate, Class, Dirt, Head, Inventory, Merged, Size, Verdict};

use fixtures::{pin_git_env, Fixture};

const TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    hash: u64,
    len: u64,
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn stamp(path: &Path) -> FileStamp {
    let bytes = std::fs::read(path).unwrap_or_default();
    FileStamp {
        hash: hash_of(&bytes),
        len: bytes.len() as u64,
    }
}

/// Everything about a repository that a read-only operation must leave alone.
#[derive(Debug)]
struct Fingerprint {
    /// Full bytes of every index file, so a failure can say what changed rather
    /// than only that something did.
    index_bytes: BTreeMap<PathBuf, Vec<u8>>,
    /// mtime of every index file. A stat-cache writeback moves this even when
    /// the contents happen to round-trip identically, so it is asserted
    /// separately rather than trusted as the only signal.
    index_mtimes: BTreeMap<PathBuf, SystemTime>,
    /// Every file in the common git dir (excluding the object store) and in
    /// every working tree, untracked and ignored files included.
    files: BTreeMap<PathBuf, FileStamp>,
    /// Every file in the real object store, by path.
    odb: BTreeSet<PathBuf>,
    /// Refs and worktree admin state as git itself reports them.
    refs: String,
    reflogs: BTreeMap<PathBuf, FileStamp>,
    /// Any `*.lock` present. Excluded from `files` because a lock is transient
    /// by nature; tracked here so leftovers are still caught.
    locks: BTreeSet<PathBuf>,
}

fn walk(
    root: &Path,
    exclude: &[PathBuf],
    files: &mut BTreeMap<PathBuf, FileStamp>,
    locks: &mut BTreeSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if exclude.contains(&path) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, exclude, files, locks),
            Ok(_) => {
                if path.extension().map(|e| e == "lock").unwrap_or(false) {
                    locks.insert(path);
                } else {
                    let s = stamp(&path);
                    files.insert(path, s);
                }
            }
            Err(_) => {}
        }
    }
}

fn collect_paths(root: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_paths(&path, out),
            Ok(_) => {
                out.insert(path);
            }
            Err(_) => {}
        }
    }
}

/// The git dir of one worktree, or `None` when the checkout is not there to be
/// asked — which is the normal state of a prunable worktree.
fn git_dir_of(fixture: &Fixture, worktree: &Path) -> Option<PathBuf> {
    if !worktree.exists() {
        return None;
    }
    fixture
        .try_git(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .ok()
        .map(PathBuf::from)
}

fn fingerprint(fixture: &Fixture, worktrees: &[PathBuf]) -> Fingerprint {
    let common_dir = PathBuf::from(fixture.git(
        &fixture.repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let objects = common_dir.join("objects");

    let mut files = BTreeMap::new();
    let mut locks = BTreeSet::new();
    // The object store is compared as a name set instead of file-by-file, so it
    // is excluded from the byte walk.
    walk(
        &common_dir,
        std::slice::from_ref(&objects),
        &mut files,
        &mut locks,
    );
    for wt in worktrees {
        // `.git` inside a linked worktree is a gitlink file, and inside the main
        // worktree it is the git dir itself; either way it is repository state,
        // already covered by the common-dir walk.
        walk(wt, &[wt.join(".git")], &mut files, &mut locks);
    }

    let mut odb = BTreeSet::new();
    collect_paths(&objects, &mut odb);

    let mut index_bytes = BTreeMap::new();
    let mut index_mtimes = BTreeMap::new();
    for wt in std::iter::once(&fixture.repo).chain(worktrees.iter()) {
        let Some(git_dir) = git_dir_of(fixture, wt) else {
            continue;
        };
        let index = git_dir.join("index");
        if let Ok(bytes) = std::fs::read(&index) {
            index_bytes.insert(index.clone(), bytes);
            if let Ok(mtime) = std::fs::metadata(&index).and_then(|m| m.modified()) {
                index_mtimes.insert(index, mtime);
            }
        }
    }

    let mut refs = fixture.git(
        &fixture.repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(objecttype)",
        ],
    );
    refs.push('\n');
    // Includes `locked` and `prunable`, so an accidental unlock or prune shows
    // up here even though neither writes a ref.
    refs.push_str(&fixture.git(&fixture.repo, &["worktree", "list", "--porcelain"]));

    let mut reflogs = BTreeMap::new();
    let mut reflog_locks = BTreeSet::new();
    walk(
        &common_dir.join("logs"),
        &[],
        &mut reflogs,
        &mut reflog_locks,
    );
    for wt in worktrees {
        if let Some(git_dir) = git_dir_of(fixture, wt) {
            walk(&git_dir.join("logs"), &[], &mut reflogs, &mut reflog_locks);
        }
    }

    Fingerprint {
        index_bytes,
        index_mtimes,
        files,
        odb,
        refs,
        reflogs,
        locks,
    }
}

/// Reports every difference, so one run names all the damage instead of only
/// the first byte that moved.
fn assert_unchanged(before: &Fingerprint, after: &Fingerprint) {
    let mut problems: Vec<String> = Vec::new();

    // 1. Indexes, byte for byte, plus mtime. `--no-optional-locks` exists
    //    entirely to keep this row still.
    for (path, bytes) in &before.index_bytes {
        match after.index_bytes.get(path) {
            None => problems.push(format!("index removed: {}", path.display())),
            Some(now) if now != bytes => problems.push(format!(
                "index rewritten: {} ({} bytes -> {} bytes)",
                path.display(),
                bytes.len(),
                now.len()
            )),
            Some(_) => {}
        }
    }
    for path in after.index_bytes.keys() {
        if !before.index_bytes.contains_key(path) {
            problems.push(format!("index created: {}", path.display()));
        }
    }
    for (path, mtime) in &before.index_mtimes {
        if after.index_mtimes.get(path) != Some(mtime) {
            problems.push(format!(
                "index mtime moved (stat-cache writeback): {}",
                path.display()
            ));
        }
    }

    // 2 and 3. Working trees, refs, reflogs, and the rest of the git dir.
    for (path, was) in &before.files {
        match after.files.get(path) {
            None => problems.push(format!("file removed: {}", path.display())),
            Some(now) if now != was => problems.push(format!("file modified: {}", path.display())),
            Some(_) => {}
        }
    }
    for path in after.files.keys() {
        if !before.files.contains_key(path) {
            problems.push(format!("file created: {}", path.display()));
        }
    }
    if before.refs != after.refs {
        problems.push(format!(
            "refs or worktree admin state changed:\n--- before\n{}\n--- after\n{}",
            before.refs, after.refs
        ));
    }
    for (path, was) in &before.reflogs {
        if after.reflogs.get(path) != Some(was) {
            problems.push(format!("reflog changed: {}", path.display()));
        }
    }
    for path in after.reflogs.keys() {
        if !before.reflogs.contains_key(path) {
            problems.push(format!("reflog created: {}", path.display()));
        }
    }

    // 4. The object store. shear has no reason to write an object at all, which
    //    is exactly why the claim is worth a test: it is the sort of thing a
    //    later `git stash`-shaped convenience would break silently.
    let grew: Vec<&PathBuf> = after.odb.difference(&before.odb).collect();
    if !grew.is_empty() {
        problems.push(format!(
            "{} object(s) leaked into the user's ODB, first few: {:?}",
            grew.len(),
            grew.iter().take(5).collect::<Vec<_>>()
        ));
    }
    if before.odb.len() != after.odb.len() {
        problems.push(format!(
            "object count changed: {} -> {}",
            before.odb.len(),
            after.odb.len()
        ));
    }

    // 5. Locks. Anything new is a leftover.
    let new_locks: Vec<&PathBuf> = after.locks.difference(&before.locks).collect();
    if !new_locks.is_empty() {
        problems.push(format!("lock files left behind: {new_locks:?}"));
    }

    assert!(
        problems.is_empty(),
        "the repository was modified:\n  {}",
        problems.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// The read path under test
// ---------------------------------------------------------------------------

/// A repository with every class shear recognises, plus files whose names
/// exercise the `-z` parsing and ignored files that must survive untouched.
fn kitchen_sink(tag: &str) -> (Fixture, Vec<PathBuf>) {
    pin_git_env();
    let fixture = Fixture::new(tag);

    let merged = fixture.merged_worktree("merged");
    let safe = fixture.safe_worktree("safe");
    let active = fixture.active_worktree("active");
    let dirty = fixture.dirty_worktree("dirty");
    let stale = fixture.stale_worktree("stale", 400);
    let locked = fixture.locked_worktree("locked", "held for demo");
    let locked_bare = fixture.locked_worktree_no_reason("locked2");
    let detached = fixture.detached_worktree("detached");
    let conflicted = fixture.conflicted_worktree("clash");
    let broken = fixture.broken_head_worktree("broken");
    let unborn = fixture.unborn_worktree("unborn");
    // The directory is gone, so this one is deliberately *not* in the walked
    // list; it is still enumerated, statused and sized by the read path.
    fixture.prunable_worktree("prunable");

    // Untracked names with a space, a quote and a literal newline, plus an
    // ignored directory: the files most likely to be disturbed by anything that
    // stages or cleans.
    // Not the safe worktree: an untracked file would make it dirty, and this
    // fixture's whole point is that exactly one row stays preselectable.
    for wt in [&dirty, &active, &conflicted] {
        fixture.tricky_untracked(wt);
    }
    fixture.write(&dirty, ".gitignore", "ignored/\n");
    fixture.write(&dirty, "ignored/build.log", "noise\n");

    let worktrees = vec![
        fixture.repo.clone(),
        merged,
        safe,
        active,
        dirty,
        stale,
        locked,
        locked_bare,
        detached,
        conflicted,
        broken,
        unborn,
    ];
    (fixture, worktrees)
}

/// Everything the plugin reads from a repository, in the order `shear::scan`
/// reads it, driven directly rather than through `scan` so that no herdr socket
/// on the developer's machine can join in and move something itself.
fn run_full_read_path(fixture: &Fixture) -> (Inventory, Vec<String>) {
    let mut notes: Vec<String> = Vec::new();
    let repo = git::repo_at(&fixture.repo, TIMEOUT)
        .expect("repo_at")
        .expect("the fixture is a git repository");
    let key = git::repo_key(&fixture.repo, TIMEOUT).expect("repo_key");
    assert_eq!(repo.key, key);

    let worktrees = git::worktrees(&repo.root, TIMEOUT).expect("worktrees");
    let branches = git::branches(&repo.root, TIMEOUT).expect("branches");
    let integration = git::integration_ref(&repo.root, None, TIMEOUT)
        .expect("integration_ref")
        .expect("the fixture has origin/main");
    let merged = git::merged_branches(&repo.root, &integration, TIMEOUT).expect("merged_branches");

    let mut inventory = Inventory::default();
    for worktree in worktrees {
        let dirt = match git::dirt(&worktree.path, TIMEOUT) {
            Ok(dirt) => dirt,
            Err(err) => {
                notes.push(format!("dirt {}: {err}", worktree.path.display()));
                Dirt::default()
            }
        };
        if worktree.head == Head::Unborn {
            // The reflog discriminator reads a file inside the worktree's own
            // git dir, which is as much a part of the read path as any command.
            let _ = git::has_head_reflog(&worktree.path, TIMEOUT);
        }
        let branch_row = worktree
            .head
            .branch()
            .and_then(|name| branches.iter().find(|row| row.name == name));

        let merged_state = match (worktree.head.branch(), &worktree.head) {
            (Some(branch), _) => {
                if merged.iter().any(|m| m == branch) {
                    Merged::Into(integration.clone())
                } else {
                    Merged::No(integration.clone())
                }
            }
            (None, Head::Detached) => {
                let oid = worktree
                    .head_oid
                    .clone()
                    .expect("a detached HEAD has an oid");
                match git::is_ancestor(&repo.root, &oid, &integration, TIMEOUT) {
                    Ok(true) => Merged::Into(integration.clone()),
                    Ok(false) => Merged::No(integration.clone()),
                    Err(err) => {
                        notes.push(format!("is_ancestor {oid}: {err}"));
                        Merged::Unknown
                    }
                }
            }
            _ => Merged::Unknown,
        };

        inventory.candidates.push(shear::classify::classify(
            shear::classify::Facts {
                upstream: branch_row
                    .map(|row| row.upstream.clone())
                    .unwrap_or_default(),
                last_commit: branch_row.and_then(|row| row.tip),
                open_workspace: None,
                occupants: Vec::new(),
                protected: None,
                merged: merged_state,
                dirt,
                worktree,
            },
            Duration::from_secs(14 * 86_400),
            SystemTime::now(),
        ));
    }
    inventory.repos.push(repo);

    // Sizing walks every checkout, in parallel, on the user's real files. It
    // reads only, and this is where that is proven.
    disk::measure_all(&mut inventory, &AtomicBool::new(false));
    (inventory, notes)
}

/// The read path has to have actually done its job, or "nothing changed" is
/// trivially true of a scan that read nothing.
fn assert_the_scan_was_real(inventory: &Inventory) {
    assert!(
        inventory.candidates.len() >= 12,
        "only {} worktrees scanned",
        inventory.candidates.len()
    );
    let safe: Vec<&Candidate> = inventory.safe().collect();
    assert_eq!(
        safe.len(),
        1,
        "expected exactly the safe worktree: {:?}",
        safe.iter()
            .map(|c| c.path().display().to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        inventory
            .candidates
            .iter()
            .any(|c| c.is(Class::Locked) && c.verdict == Verdict::Blocked),
        "the locked worktrees were not classified"
    );
    assert!(
        inventory
            .candidates
            .iter()
            .any(|c| c.is(Class::Prunable) && c.size == Size::Gone),
        "the prunable worktree was not enumerated, or was sized as if it existed"
    );
    assert!(
        inventory
            .candidates
            .iter()
            .any(|c| c.dirt.unmerged > 0 && c.dirt.untracked >= 3),
        "the conflicted worktree's unmerged and tricky untracked paths were not read"
    );
    assert!(
        inventory
            .candidates
            .iter()
            .any(|c| matches!(c.size, Size::Bytes(bytes) if bytes > 0)),
        "nothing was sized, so the disk walk did not run over the working trees"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_full_read_path_changes_nothing_in_the_repository() {
    let (fixture, worktrees) = kitchen_sink("read-only");

    let before = fingerprint(&fixture, &worktrees);
    assert!(
        before.odb.len() > 5,
        "fixture has no objects to protect: {}",
        before.odb.len()
    );
    assert!(
        !before.index_bytes.is_empty(),
        "fixture has no index to protect"
    );

    let (inventory, notes) = run_full_read_path(&fixture);
    assert!(
        notes.is_empty(),
        "the read path reported problems: {notes:?}"
    );
    assert_the_scan_was_real(&inventory);

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
}

/// A separate process holding `index.lock` is exactly what an agent running
/// `git add` looks like from the outside. Nothing shear does may need that lock,
/// and nothing it does may disturb the holder — including clearing a lock it did
/// not take, which is the "helpful" repair that loses somebody's staged work.
#[test]
fn the_read_path_works_while_another_process_holds_index_lock() {
    let (fixture, worktrees) = kitchen_sink("index-lock");

    let mut holders = LockHolders::default();
    let mut lock_paths = Vec::new();
    for wt in [&fixture.repo, &worktrees[4]] {
        let git_dir =
            PathBuf::from(fixture.git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"]));
        let lock = git_dir.join("index.lock");
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                ": > '{}'; exec sleep 120",
                lock.to_string_lossy().replace('\'', "'\\''")
            ))
            // Never inherit the harness's pipes: a leaked holder would keep
            // stdout open and hang `cargo test` long after the test finished.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock holder");
        holders.0.push(child);
        lock_paths.push(lock);
    }
    // Wait for the locks to actually exist before measuring anything.
    for _ in 0..200 {
        if lock_paths.iter().all(|p| p.exists()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for lock in &lock_paths {
        assert!(lock.exists(), "lock holder never took {}", lock.display());
    }

    let before = fingerprint(&fixture, &worktrees);

    // Four concurrent readers, because a scan racing itself is the shape a
    // review pane and a `--json` run in another pane actually produce.
    let results: Vec<(Inventory, Vec<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| run_full_read_path(&fixture)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a reader thread panicked"))
            .collect()
    });
    for (inventory, notes) in &results {
        assert!(
            notes.is_empty(),
            "a read with index.lock held reported problems: {notes:?}"
        );
        assert_the_scan_was_real(inventory);
    }

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);

    // The holder still owns its locks; shear never stole or cleared them.
    for lock in &lock_paths {
        assert!(
            lock.exists(),
            "shear removed a lock it did not take: {}",
            lock.display()
        );
    }
    drop(holders);
    for lock in &lock_paths {
        let _ = std::fs::remove_file(lock);
    }
}

/// Sizing is the one part of the read path that touches every byte of the
/// working tree, so it gets its own proof — including that it never follows a
/// symlink out of the checkout.
#[test]
fn measuring_disk_usage_reads_and_never_follows_a_symlink() {
    pin_git_env();
    let fixture = Fixture::new("disk-read-only");
    let worktree = fixture.dirty_worktree("dirty");
    fixture.tricky_untracked(&worktree);

    #[cfg(unix)]
    {
        // A symlink to `/` must cost one inode, not the size of the machine.
        std::os::unix::fs::symlink("/", worktree.join("everything"))
            .expect("create a symlink out of the checkout");
    }

    let worktrees = vec![fixture.repo.clone(), worktree.clone()];
    let before = fingerprint(&fixture, &worktrees);

    let measured = disk::measure(&worktree, &AtomicBool::new(false));
    let Size::Bytes(bytes) = measured else {
        panic!("the checkout was not measured: {measured:?}");
    };
    assert!(bytes > 0, "an existing checkout occupies something");
    assert!(
        bytes < 1_000_000_000,
        "{bytes} bytes for a checkout of four files means the symlink was followed"
    );

    // A path that is not there reclaims nothing, and says so differently from
    // "zero bytes".
    assert_eq!(
        disk::measure(
            &fixture.root().join("never-existed"),
            &AtomicBool::new(false)
        ),
        Size::Gone
    );

    // A cancelled walk reports "not measured", never a plausible zero.
    let cancelled = AtomicBool::new(true);
    assert_eq!(disk::measure(&worktree, &cancelled), Size::Pending);

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
}

/// Reaps the lock-holding processes even when an assertion above panics, so a
/// failing test never leaves `sleep` children behind.
#[derive(Default)]
struct LockHolders(Vec<std::process::Child>);

impl Drop for LockHolders {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
