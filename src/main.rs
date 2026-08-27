//! shear — a worktree janitor for herdr.
//!
//! Verb dispatch only; every verb is implemented in the library crate.

use shear::{config, remove, render, report, shear as scan, tui, Result};

const USAGE: &str = "\
shear — find the git worktrees that are safe to delete, and delete those

Usage: shear [VERB] [OPTIONS]

Review:
  --list              Print the worktree inventory and exit (default)
  --json              Print the same inventory as JSON and exit
  --report            Print the CI-shaped stale report; no removal path exists
  --review            Interactive review pane: select, confirm, remove

Removal:
  --remove <PATH>     Remove one worktree by path. Repeatable. Refuses anything
                      dirty, locked, or open in herdr unless the matching
                      override below is also given.
  --restore <id>      Restore the checkout named by a #N from --undo-log.
                      Uses its branch only if it still points at the recorded
                      commit; otherwise restores detached at that commit.
  --force-dirty       Permit removing a worktree with uncommitted changes.
                      Requires --i-understand-<N>-files, where N is the exact
                      number of at-risk files shear reports.
  --close-workspace   Permit closing a herdr workspace that holds a selected
                      worktree open.
  --undo-log          Print every removal shear has made, newest first

Options:
  --repo <PATH>       Scan only this repository. Repeatable. Without it, shear
                      scans every repository the herdr session knows about.
  --integration-ref <REF>
                      Ref a branch must be contained in to count as merged
                      (default: origin/HEAD, then origin/main, origin/master,
                      main, master)
  --stale-days <N>    Age past which a clean branch is called stale (default 14)
  --no-size           Skip disk measurement; every size reads `-`
  --version           Print version and exit
  --help              Show this help

Nothing is ever removed without an explicit selection. Removing a worktree
leaves its branch and every commit on it untouched: only the checkout goes.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args) {
        eprintln!("shear: {err}");
        std::process::exit(1);
    }
}

/// Every verb, named rather than inferred.
///
/// The first version of this worked the other way round — the verb was "the
/// first argument that is not an option or an option's value" — and it broke the
/// moment the removal path grew flags that take no value: `shear --remove X
/// --force-dirty` read `--force-dirty` as the verb and refused it. Listing the
/// verbs is duller and cannot rot as options are added.
const VERBS: [&str; 10] = [
    "--list",
    "--json",
    "--report",
    "--review",
    "--remove",
    "--restore",
    "--undo-log",
    "--version",
    "--help",
    "-h",
];

/// Options that take a value.
const VALUED: [&str; 5] = [
    "--repo",
    "--integration-ref",
    "--stale-days",
    "--remove",
    "--restore",
];

/// Options that take no value.
const FLAGS: [&str; 3] = ["--force-dirty", "--close-workspace", "--no-size"];

/// The acknowledgement for a dirty removal, spelled with the file count inside
/// it: `--i-understand-7-files`. The count is the confirmation, so it cannot be
/// a separate value that a shell history could carry across to a different
/// worktree.
const ACK_PREFIX: &str = "--i-understand-";
const ACK_SUFFIX: &str = "-files";

fn is_acknowledgement(arg: &str) -> bool {
    arg.strip_prefix(ACK_PREFIX)
        .and_then(|rest| rest.strip_suffix(ACK_SUFFIX))
        .is_some_and(|count| !count.is_empty())
}

/// The verb, wherever it appears, so `shear --stale-days 30 --json` works as
/// readily as `shear --json --stale-days 30`. Ordering that matters is a
/// papercut nobody should have to learn.
fn verb_of(args: &[String]) -> &str {
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let name = arg.split('=').next().unwrap_or(arg);
        if VALUED.contains(&name) {
            // `--repo=/x` carries its value; bare `--repo /x` does not.
            skip_value = !arg.contains('=');
            // `--remove` and `--restore` are both verbs and valued options;
            // their values must not be consumed as verbs, so they are picked
            // up after the loop instead.
            continue;
        }
        if VERBS.contains(&name) {
            return VERBS
                .iter()
                .find(|verb| **verb == name)
                .copied()
                .unwrap_or("--list");
        }
    }
    for verb in ["--remove", "--restore"] {
        if args
            .iter()
            .any(|arg| arg == verb || arg.starts_with(&format!("{verb}=")))
        {
            return verb;
        }
    }
    "--list"
}

/// Rejects anything that is not a verb, a known option, or an option's value.
///
/// Without this, a mistyped flag is silently ignored — and a silently ignored
/// `--force-dirty` is the good case. A silently ignored `--repo` would scan the
/// whole session when the user asked for one repository.
fn reject_unknown(args: &[String]) -> Result<()> {
    let has_remove = args
        .iter()
        .any(|arg| arg == "--remove" || arg.starts_with("--remove="));
    let has_restore = args
        .iter()
        .any(|arg| arg == "--restore" || arg.starts_with("--restore="));
    if has_remove && has_restore {
        return Err("--remove and --restore are two different verbs; run one at a time".into());
    }

    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let name = arg.split('=').next().unwrap_or(arg);
        if VALUED.contains(&name) {
            skip_value = !arg.contains('=');
            continue;
        }
        if VERBS.contains(&name) || FLAGS.contains(&name) || is_acknowledgement(arg) {
            continue;
        }
        if arg.starts_with(ACK_PREFIX) {
            return Err(format!(
                "`{arg}` is not a file-count acknowledgement. It must be spelled \
                 --i-understand-<N>-files, with N the exact number of at-risk files shear \
                 reported for that worktree."
            )
            .into());
        }
        return Err(format!("unknown option `{arg}`\n\n{USAGE}").into());
    }
    Ok(())
}

fn run(args: &[String]) -> Result<()> {
    let verb = verb_of(args);
    // `--help` and `--version` answer before anything can be rejected, so a
    // user who has mistyped a flag can still read what the right one is.
    if !matches!(verb, "--help" | "-h" | "--version") {
        reject_unknown(args)?;
    }
    match verb {
        "--list" => render::run_list(&config::load_with_args(args)?),
        "--json" => scan::run_json(&config::load_with_args(args)?),
        "--report" => report::run_report(&config::load_with_args(args)?),
        "--review" => tui::run_review(&config::load_with_args(args)?),
        "--remove" => remove::run_remove(&config::load_with_args(args)?, args),
        "--restore" => remove::run_restore(&config::load_with_args(args)?, args),
        "--undo-log" => remove::run_undo_log(),
        "--version" => {
            println!("shear {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown verb `{other}`\n\n{USAGE}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_acknowledgement, reject_unknown, run, verb_of};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_verb_is_found_whatever_the_order() {
        assert_eq!(verb_of(&args(&["--review"])), "--review");
        assert_eq!(verb_of(&args(&["--report"])), "--report");
        assert_eq!(verb_of(&args(&["--json", "--stale-days", "30"])), "--json");
        assert_eq!(verb_of(&args(&["--stale-days", "30", "--json"])), "--json");
        assert_eq!(verb_of(&args(&["--stale-days=30", "--json"])), "--json");
        assert_eq!(
            verb_of(&args(&["--repo", "/tmp/r", "--review"])),
            "--review"
        );
        assert_eq!(
            verb_of(&args(&["--repo", "/tmp/r", "--report"])),
            "--report"
        );
        assert_eq!(
            verb_of(&args(&["--report", "--json"])),
            "--report",
            "the report verb is distinct from the inventory JSON verb"
        );
    }

    #[test]
    fn no_arguments_means_a_dry_run_listing() {
        assert_eq!(verb_of(&args(&[])), "--list");
        assert_eq!(verb_of(&args(&["--repo", "/tmp/r"])), "--list");
    }

    #[test]
    fn a_missing_repo_value_is_rejected_before_scan_dispatch() {
        for input in [args(&["--repo"]), args(&["--repo", "--list"])] {
            let err = run(&input).expect_err("missing repository scope must stop dispatch");
            assert_eq!(err.to_string(), "--repo needs a value");
        }
    }

    #[test]
    fn an_option_value_is_never_mistaken_for_a_verb() {
        // A branch genuinely called `--json` is absurd, but a path that starts
        // with a dash is not, and either way a value is a value.
        assert_eq!(verb_of(&args(&["--integration-ref", "--json"])), "--list");
    }

    #[test]
    fn remove_selects_its_own_verb_despite_taking_values() {
        assert_eq!(verb_of(&args(&["--remove", "/tmp/wt"])), "--remove");
        assert_eq!(verb_of(&args(&["--remove=/tmp/wt"])), "--remove");
        assert_eq!(
            verb_of(&args(&["--remove", "/tmp/a", "--remove", "/tmp/b"])),
            "--remove"
        );
    }

    #[test]
    fn restore_selects_its_own_verb_and_its_id_is_only_a_value() {
        assert_eq!(verb_of(&args(&["--restore", "3"])), "--restore");
        assert_eq!(verb_of(&args(&["--restore=3"])), "--restore");
        assert_eq!(
            verb_of(&args(&["--restore", "3", "--no-size"])),
            "--restore"
        );
    }

    #[test]
    fn restore_is_found_after_another_valued_option() {
        assert_eq!(
            verb_of(&args(&["--repo", "/x", "--restore", "3"])),
            "--restore"
        );
    }

    #[test]
    fn remove_and_restore_are_rejected_as_two_different_verbs() {
        let err = reject_unknown(&args(&["--remove", "/x", "--restore", "3"]))
            .expect_err("two verbs must be refused");
        assert!(err.to_string().contains("two different verbs"));
        assert!(err.to_string().contains("one at a time"));
    }

    /// The regression test for the bug that made the whole dirty-removal path
    /// unreachable from the command line: a valueless flag was read as the verb,
    /// so `shear --remove X --force-dirty` refused `--force-dirty` as unknown.
    #[test]
    fn a_valueless_flag_is_never_mistaken_for_a_verb() {
        assert_eq!(
            verb_of(&args(&[
                "--remove",
                "/tmp/wt",
                "--force-dirty",
                "--i-understand-3-files",
            ])),
            "--remove"
        );
        assert_eq!(verb_of(&args(&["--no-size", "--json"])), "--json");
        assert_eq!(verb_of(&args(&["--force-dirty"])), "--list");
        assert_eq!(
            verb_of(&args(&["--close-workspace", "--remove", "/tmp/wt"])),
            "--remove"
        );
    }

    #[test]
    fn a_mistyped_option_is_rejected_rather_than_ignored() {
        // A silently ignored `--force-dirty` is the harmless case. A silently
        // ignored `--repo` scans the whole session when one repository was
        // asked for.
        assert!(reject_unknown(&args(&["--json", "--rpeo", "/tmp/r"])).is_err());
        assert!(reject_unknown(&args(&["--json", "--no-sizes"])).is_err());
        assert!(reject_unknown(&args(&["--json", "--no-size"])).is_ok());
        assert!(reject_unknown(&args(&["--repo", "--json"])).is_ok());
    }

    #[test]
    fn a_malformed_acknowledgement_says_what_the_right_shape_is() {
        let err = reject_unknown(&args(&["--remove", "/x", "--i-understand"]))
            .expect_err("not an acknowledgement");
        assert!(
            err.to_string().contains("--i-understand-<N>-files"),
            "the message must name the right shape: {err}"
        );
    }

    #[test]
    fn an_acknowledgement_carries_its_count_in_its_name() {
        assert!(is_acknowledgement("--i-understand-3-files"));
        assert!(is_acknowledgement("--i-understand-0-files"));
        // No count at all is not an acknowledgement, however much it looks like
        // one: the count is the confirmation.
        assert!(!is_acknowledgement("--i-understand--files"));
        assert!(!is_acknowledgement("--i-understand-files"));
    }

    #[test]
    fn help_and_version_answer_even_when_something_else_is_mistyped() {
        // Rejecting first would leave a user who typed a flag wrong with no way
        // to find out what the right one was.
        assert_eq!(verb_of(&args(&["--rpeo", "--help"])), "--help");
        assert_eq!(verb_of(&args(&["--nonsense", "--version"])), "--version");
    }
}
