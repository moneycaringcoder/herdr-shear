//! shear — a worktree janitor for herdr.
//!
//! Verb dispatch only; every verb is implemented in the library crate.

use shear::{config, remove, render, shear as scan, tui, Result};

const USAGE: &str = "\
shear — find the git worktrees that are safe to delete, and delete those

Usage: shear [VERB] [OPTIONS]

Review:
  --list              Print the worktree inventory and exit (default)
  --json              Print the same inventory as JSON and exit
  --review            Interactive review pane: select, confirm, remove

Removal:
  --remove <PATH>     Remove one worktree by path. Repeatable. Refuses anything
                      dirty, locked, or open in herdr unless the matching
                      override below is also given.
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
                      (default: origin/HEAD, then origin/main, main, master)
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

/// Options that take a value, and so must never be mistaken for the verb.
const VALUED: [&str; 4] = ["--repo", "--integration-ref", "--stale-days", "--remove"];

/// The verb is the first argument that is not an option or an option's value, so
/// `shear --stale-days 30 --json` works as readily as `shear --json
/// --stale-days 30`. Ordering that matters is a papercut nobody should have to
/// learn.
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
            continue;
        }
        return arg;
    }
    // `--remove` is a valued option rather than a verb so it can be repeated,
    // so its presence has to select the verb explicitly.
    if args
        .iter()
        .any(|a| a == "--remove" || a.starts_with("--remove="))
    {
        return "--remove";
    }
    "--list"
}

fn run(args: &[String]) -> Result<()> {
    let verb = verb_of(args);
    match verb {
        "--list" => render::run_list(&config::load_with_args(args)?),
        "--json" => scan::run_json(&config::load_with_args(args)?),
        "--review" => tui::run_review(&config::load_with_args(args)?),
        "--remove" => remove::run_remove(&config::load_with_args(args)?, args),
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
    use super::verb_of;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_verb_is_found_whatever_the_order() {
        assert_eq!(verb_of(&args(&["--review"])), "--review");
        assert_eq!(verb_of(&args(&["--json", "--stale-days", "30"])), "--json");
        assert_eq!(verb_of(&args(&["--stale-days", "30", "--json"])), "--json");
        assert_eq!(verb_of(&args(&["--stale-days=30", "--json"])), "--json");
        assert_eq!(
            verb_of(&args(&["--repo", "/tmp/r", "--review"])),
            "--review"
        );
    }

    #[test]
    fn no_arguments_means_a_dry_run_listing() {
        assert_eq!(verb_of(&args(&[])), "--list");
        assert_eq!(verb_of(&args(&["--repo", "/tmp/r"])), "--list");
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
}
