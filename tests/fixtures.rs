//! Real git repositories, built to order.
//!
//! Every shape here was first built by hand against git 2.53.0 and its output
//! captured before any of it was written in Rust, because a fixture built from
//! an assumption tests the assumption. The captured output lives in
//! `tests/capture/` and `docs/git-plumbing.md` records what each command
//! actually printed.
//!
//! Fixtures may be added freely, but the meaning of an existing one must not
//! change: tests in three files depend on them.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// A repository plus every worktree the tests asked for, removed on drop.
pub struct Fixture {
    /// Main checkout.
    pub repo: PathBuf,
    /// Bare repo standing in for `origin`.
    pub origin: PathBuf,
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A locked worktree does not stop a plain directory removal, and the
        // whole tree is under the scratch root, so this is unconditional.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn scratch_root() -> PathBuf {
    // Honour the harness's scratch directory when there is one, so a test run
    // never litters the user's temp dir with repositories.
    std::env::var_os("SHEAR_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("shear-fixtures")
}

impl Fixture {
    /// An empty repository with one commit on `main`, an `origin` bare remote,
    /// and `main` tracking `origin/main`.
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let root = scratch_root().join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture root");

        let origin = root.join("origin.git");
        let repo = root.join("repo");
        let fixture = Self { repo, origin, root };

        run(
            &fixture.root,
            "git",
            &["init", "-q", "--bare", "origin.git"],
        );
        run(&fixture.root, "git", &["init", "-q", "-b", "main", "repo"]);
        fixture.write(&fixture.repo.clone(), "README.md", "base\n");
        fixture.git(&fixture.repo.clone(), &["add", "-A"]);
        fixture.commit(&fixture.repo.clone(), "base");
        fixture.git(
            &fixture.repo.clone(),
            &["remote", "add", "origin", "../origin.git"],
        );
        fixture.git(
            &fixture.repo.clone(),
            &["push", "-q", "-u", "origin", "main"],
        );
        fixture
    }

    // -----------------------------------------------------------------------
    // The classes
    // -----------------------------------------------------------------------

    /// Clean, merged into `main`, and its `origin/<branch>` deleted so
    /// `%(upstream:track)` reports `[gone]`. This is the only shape that
    /// classifies as `safe`.
    pub fn safe_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        self.git(&self.repo, &["checkout", "-q", "-b", &branch]);
        self.append(&self.repo, "README.md", &format!("{name}\n"));
        self.git(&self.repo, &["add", "-A"]);
        self.commit(&self.repo, name);
        self.git(&self.repo, &["push", "-q", "-u", "origin", &branch]);
        self.git(&self.repo, &["checkout", "-q", "main"]);
        self.git(
            &self.repo,
            &["merge", "-q", "--no-ff", &branch, "-m", "merge"],
        );
        self.git(&self.repo, &["push", "-q", "origin", "main"]);
        self.git(&self.repo, &["push", "-q", "origin", "--delete", &branch]);
        self.git(&self.repo, &["fetch", "-q", "-p", "origin"]);
        self.add_worktree(name, &["--", &branch])
    }

    /// Clean and merged, but its upstream still exists: `review`, never `safe`.
    pub fn merged_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        self.git(&self.repo, &["checkout", "-q", "-b", &branch]);
        self.append(&self.repo, "README.md", &format!("{name}\n"));
        self.git(&self.repo, &["add", "-A"]);
        self.commit(&self.repo, name);
        self.git(&self.repo, &["push", "-q", "-u", "origin", &branch]);
        self.git(&self.repo, &["checkout", "-q", "main"]);
        self.git(
            &self.repo,
            &["merge", "-q", "--no-ff", &branch, "-m", "merge"],
        );
        self.git(&self.repo, &["fetch", "-q", "-p", "origin"]);
        self.add_worktree(name, &["--", &branch])
    }

    /// Unmerged, with a live upstream and a recent commit: `keep`.
    pub fn active_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        self.append(&path, "README.md", &format!("{name}\n"));
        self.git(&path, &["add", "-A"]);
        self.commit(&path, name);
        self.git(&path, &["push", "-q", "-u", "origin", &branch]);
        self.git(&self.repo, &["fetch", "-q", "-p", "origin"]);
        path
    }

    /// Uncommitted changes: a modified tracked file and an untracked one.
    /// Never preselected, whatever else is true of it.
    pub fn dirty_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        self.append(&path, "README.md", "uncommitted\n");
        self.write(&path, "scratch.txt", "not staged anywhere\n");
        path
    }

    /// Clean, unmerged, with no upstream at all, whose tip commit is dated
    /// `days` ago.
    pub fn stale_worktree(&self, name: &str, days: u64) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        let when = format!("{} +0000", unix_seconds_ago(days));
        self.append(&path, "README.md", &format!("{name}\n"));
        self.git(&path, &["add", "-A"]);
        run_env(
            &path,
            "git",
            &["commit", "-q", "-m", name],
            &[
                ("GIT_AUTHOR_DATE", when.as_str()),
                ("GIT_COMMITTER_DATE", when.as_str()),
            ],
        );
        path
    }

    /// `git worktree lock`ed, with a reason. Blocked, always.
    pub fn locked_worktree(&self, name: &str, reason: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        let path_str = path.to_string_lossy().into_owned();
        self.git(
            &self.repo,
            &["worktree", "lock", "--reason", reason, &path_str],
        );
        path
    }

    /// Detached HEAD at the current `main` tip. Has no branch, so it can be
    /// neither merged-by-name nor gone-upstream.
    pub fn detached_worktree(&self, name: &str) -> PathBuf {
        self.add_worktree(name, &["--detach", "HEAD"])
    }

    /// The directory is deleted behind git's back, so
    /// `git worktree list --porcelain` reports `prunable` with the reason
    /// "gitdir file points to non-existent location". Returns the path that no
    /// longer exists.
    pub fn prunable_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        std::fs::remove_dir_all(&path).expect("delete the checkout behind git's back");
        path
    }

    /// A worktree whose branch is deleted underneath it: `HEAD 0000…` with a
    /// non-empty `logs/HEAD`, which is the only thing that distinguishes it from
    /// an unborn worktree.
    pub fn broken_head_worktree(&self, name: &str) -> PathBuf {
        let branch = format!("{name}-branch");
        let path = self.add_worktree(name, &["-b", &branch]);
        // `-D` on a fixture branch inside a scratch directory, not on anything
        // of the user's.
        self.git(&self.repo, &["branch", "-D", &branch]);
        path
    }

    /// A second, unrelated repository, for proving that worktrees are never
    /// compared or grouped across repos.
    pub fn foreign_repo(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("create foreign repo");
        run(&path, "git", &["init", "-q", "-b", "main", "."]);
        self.write(&path, "other.txt", "elsewhere\n");
        self.git(&path, &["add", "-A"]);
        self.commit(&path, "foreign");
        path
    }

    /// Files whose names exercise the `-z` parsing: a newline, a quote, a
    /// space, and a non-UTF-8 byte where the platform allows it.
    pub fn tricky_untracked(&self, worktree: &Path) {
        self.write(worktree, "a file with spaces.txt", "x\n");
        self.write(worktree, "quote\"name.txt", "x\n");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let newline = worktree.join(std::ffi::OsStr::from_bytes(b"new\nline.txt"));
            let _ = std::fs::write(newline, b"x\n");
        }
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    /// `git worktree add <root>/<name> <extra…>`.
    pub fn add_worktree(&self, name: &str, extra: &[&str]) -> PathBuf {
        let path = self.root.join(format!("wt-{name}"));
        let path_str = path.to_string_lossy().into_owned();
        let mut args: Vec<&str> = vec!["worktree", "add", "-q", &path_str];
        args.extend_from_slice(extra);
        self.git(&self.repo, &args);
        path
    }

    pub fn write(&self, worktree: &Path, relative: &str, body: &str) {
        let path = worktree.join(relative);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, body).expect("write fixture file");
    }

    pub fn append(&self, worktree: &Path, relative: &str, body: &str) {
        use std::io::Write;
        let path = worktree.join(relative);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open fixture file");
        file.write_all(body.as_bytes()).expect("append");
    }

    pub fn commit(&self, worktree: &Path, message: &str) {
        self.git(worktree, &["commit", "-q", "--allow-empty", "-m", message]);
    }

    /// Runs git in `worktree` and returns trimmed stdout. Panics with git's own
    /// stderr on failure, because a fixture that half-built is worse than one
    /// that did not build.
    pub fn git(&self, worktree: &Path, args: &[&str]) -> String {
        run(worktree, "git", args)
    }

    /// Runs git and returns `Err(stderr)` instead of panicking, for the tests
    /// that assert git *refuses* something.
    pub fn try_git(&self, worktree: &Path, args: &[&str]) -> Result<String, String> {
        try_run(worktree, "git", args, &[])
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Environment every fixture command runs in.
///
/// The user's own git config is excluded outright: `init.defaultBranch`,
/// `merge.conflictstyle`, a global `core.excludesFile` and a commit-signing
/// setup would each change what a fixture produces, so a green suite on this
/// machine would say nothing about a green suite on CI.
fn base_env() -> Vec<(&'static str, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_SYSTEM", "/dev/null".into()),
        ("GIT_AUTHOR_NAME", "shear fixtures".into()),
        ("GIT_AUTHOR_EMAIL", "fixtures@example.invalid".into()),
        ("GIT_COMMITTER_NAME", "shear fixtures".into()),
        ("GIT_COMMITTER_EMAIL", "fixtures@example.invalid".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
    ]
}

fn run(cwd: &Path, program: &str, args: &[&str]) -> String {
    match try_run(cwd, program, args, &[]) {
        Ok(stdout) => stdout,
        Err(err) => panic!("{program} {args:?} in {} failed: {err}", cwd.display()),
    }
}

fn run_env(cwd: &Path, program: &str, args: &[&str], env: &[(&str, &str)]) -> String {
    match try_run(cwd, program, args, env) {
        Ok(stdout) => stdout,
        Err(err) => panic!("{program} {args:?} in {} failed: {err}", cwd.display()),
    }
}

fn try_run(
    cwd: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<String, String> {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in base_env() {
        command.env(key, value);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|err| format!("could not run {program}: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn unix_seconds_ago(days: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs();
    now.saturating_sub(days.saturating_mul(86_400))
}
