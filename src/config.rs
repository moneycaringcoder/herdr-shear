//! Configuration, plugin identity, and the state/config directories herdr hands
//! us. Read by every other module; changed by none of them.

use std::path::PathBuf;
use std::time::Duration;

use crate::model::{Merged, Upstream};
use crate::Result;

pub const PLUGIN_ID: &str = "moneycaringcoder.shear";

/// Days after which a clean, unmerged branch is called stale. A fortnight is
/// long enough that an ordinary holiday does not condemn a branch.
pub const DEFAULT_STALE_DAYS: u64 = 14;

/// Candidate integration refs, tried in order when the user has not named one
/// and `origin/HEAD` is not set. `origin/HEAD` is checked first and separately,
/// because it is the only one that is actually authoritative.
pub const DEFAULT_BRANCH_GUESSES: [&str; 4] = ["origin/main", "origin/master", "main", "master"];

/// The fact a staleness rule is conditioned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StaleWhen {
    Any,
    Merged,
    Unmerged,
    Gone,
}

impl StaleWhen {
    fn label(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Merged => "merged",
            Self::Unmerged => "unmerged",
            Self::Gone => "gone",
        }
    }

    fn matches(self, merged: &Merged, upstream: &Upstream) -> bool {
        match self {
            Self::Any => true,
            Self::Merged => matches!(merged, Merged::Into(_)),
            Self::Unmerged => matches!(merged, Merged::No(_)),
            Self::Gone => upstream.gone,
        }
    }
}

/// One ordered override of the fallback staleness threshold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct StaleRule {
    pub when: StaleWhen,
    pub days: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Ref a branch must be contained in to count as merged. `None` means
    /// "work it out per repo": `origin/HEAD` if it resolves, else the first of
    /// [`DEFAULT_BRANCH_GUESSES`] that does.
    ///
    /// A repo where none of them resolves gets no merged classification at all,
    /// and the row says so. Reporting "not merged" when the question could not
    /// be asked would be the exact silent-degradation failure this plugin is
    /// built to avoid.
    pub integration_ref: Option<String>,
    pub stale_days: u64,
    /// Ordered staleness overrides. The first matching rule wins.
    pub stale_rules: Vec<StaleRule>,
    /// Patterns that make matching worktrees permanently ineligible for removal.
    pub protect: Vec<String>,
    /// Timeout for any single git invocation, so one slow or wedged repo cannot
    /// stall a scan.
    pub git_timeout: Duration,
    /// Measure disk usage at all. Off makes every scan instant and every size
    /// column read `-`.
    pub measure_disk: bool,
    /// Extra repositories to scan, beyond the ones the herdr session knows
    /// about. Each is a path anywhere inside the repo.
    pub extra_repos: Vec<PathBuf>,
    /// Scan only these repositories, ignoring the session entirely.
    pub only_repos: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            integration_ref: None,
            stale_days: DEFAULT_STALE_DAYS,
            stale_rules: Vec::new(),
            protect: Vec::new(),
            git_timeout: Duration::from_secs(10),
            measure_disk: true,
            extra_repos: Vec::new(),
            only_repos: Vec::new(),
        }
    }
}

impl Config {
    pub fn stale_after(&self) -> Duration {
        Duration::from_secs(self.stale_days.saturating_mul(86_400))
    }

    /// Resolves the first matching policy rule, or the `stale_days` fallback.
    pub fn stale_after_for(&self, merged: &Merged, upstream: &Upstream) -> Duration {
        self.stale_rules
            .iter()
            // `Unknown` deliberately matches neither merged nor unmerged: a
            // question that could not be asked is not an answer.
            // Config values built in code never pass through `load_file`, so keep this check.
            .find(|rule| rule.days > 0 && rule.when.matches(merged, upstream))
            .map(|rule| Duration::from_secs(rule.days.saturating_mul(86_400)))
            .unwrap_or_else(|| self.stale_after())
    }
}
/// Matches a protection pattern against an absolute checkout path.
///
/// `*` matches any run of characters within one path segment, while `**`
/// matches any run including `/`. Every other character is literal. Regex
/// syntax, character classes, brace expansion, escaping, and case-insensitive
/// matching are not supported.
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    pattern_matches_inner(pattern, text, true)
}

/// Matches the same protection pattern language against a branch name.
///
/// Branch names are a single candidate rather than filesystem segments, so
/// both `*` and `**` may consume `/`. No other syntax is supported.
pub fn branch_pattern_matches(pattern: &str, text: &str) -> bool {
    pattern_matches_inner(pattern, text, false)
}

fn pattern_matches_inner(pattern: &str, text: &str, slash_is_separator: bool) -> bool {
    if !pattern.as_bytes().contains(&b'*') {
        return pattern == text;
    }

    // `matched[n]` says whether the pattern consumed so far matches the first
    // `n` bytes. UTF-8 is safe here: literals are compared byte-for-byte and a
    // wildcard accepting an arbitrary run has the same answer at byte and
    // character boundaries.
    let text = text.as_bytes();
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    let pattern = pattern.as_bytes();
    let mut offset = 0;

    while offset < pattern.len() {
        if pattern[offset] == b'*' {
            let crosses_separator = offset + 1 < pattern.len() && pattern[offset + 1] == b'*';
            if crosses_separator {
                offset += 1;
            }
            for index in 1..=text.len() {
                matched[index] = matched[index]
                    || (crosses_separator || !slash_is_separator || text[index - 1] != b'/')
                        && matched[index - 1];
            }
        } else {
            for index in (1..=text.len()).rev() {
                matched[index] = matched[index - 1] && text[index - 1] == pattern[offset];
            }
            matched[0] = false;
        }
        offset += 1;
    }

    matched[text.len()]
}

pub fn load() -> Result<Config> {
    load_with_args(&[])
}

/// Loads the config file, then applies command-line overrides.
pub fn load_with_args(args: &[String]) -> Result<Config> {
    let mut config = load_file();

    if let Some(reference) = value_arg(args, "--integration-ref")? {
        let reference = reference.trim().to_string();
        if reference.is_empty() {
            return Err("--integration-ref needs a non-empty ref".into());
        }
        config.integration_ref = Some(reference);
    }
    if let Some(days) = value_arg(args, "--stale-days")? {
        config.stale_days = days
            .trim()
            .parse::<u64>()
            .map_err(|err| format!("--stale-days {days}: {err}"))?;
    }
    if args.iter().any(|a| a == "--no-size") {
        config.measure_disk = false;
    }
    for path in values_arg(args, "--repo") {
        config.only_repos.push(PathBuf::from(path));
    }
    Ok(config)
}

/// The on-disk form. Every field is optional so a partial file overrides only
/// what it names, and unknown keys are ignored so a newer file does not break an
/// older binary.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileConfig {
    integration_ref: Option<String>,
    stale_days: Option<u64>,
    stale_rules: Option<Vec<StaleRule>>,
    protect: Option<Vec<String>>,
    git_timeout_seconds: Option<u64>,
    measure_disk: Option<bool>,
    extra_repos: Option<Vec<String>>,
}

pub(crate) fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Reads the config file over the defaults. A missing file is the normal case;
/// an unreadable or malformed one is a warning and the defaults, never a hard
/// failure.
fn load_file() -> Config {
    let path = config_file();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("shear: ignoring {}: {err}", path.display());
            }
            return Config::default();
        }
    };
    let file: FileConfig = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("shear: ignoring malformed {}: {err}", path.display());
            return Config::default();
        }
    };

    let mut config = Config::default();
    if let Some(reference) = file.integration_ref.filter(|r| !r.trim().is_empty()) {
        config.integration_ref = Some(reference);
    }
    if let Some(days) = file.stale_days {
        config.stale_days = days;
    }
    if let Some(rules) = file.stale_rules {
        config.stale_rules = rules
            .into_iter()
            .enumerate()
            .filter_map(|(index, rule)| {
                if rule.days == 0 {
                    // Zero would mark virtually every branch stale, so it is
                    // much more likely to be a typo than an intended policy.
                    eprintln!(
                        "shear: ignoring stale_rules[{index}] ({}): days must be greater than zero",
                        rule.when.label()
                    );
                    None
                } else {
                    Some(rule)
                }
            })
            .collect();
    }
    if let Some(patterns) = file.protect {
        config.protect = patterns
            .into_iter()
            .enumerate()
            .filter_map(|(index, pattern)| {
                if pattern.is_empty() {
                    eprintln!("shear: ignoring protect[{index}]: pattern must not be empty");
                    None
                } else {
                    Some(pattern)
                }
            })
            .collect();
    }
    if let Some(seconds) = file.git_timeout_seconds {
        config.git_timeout = Duration::from_secs(seconds.max(1));
    }
    if let Some(measure) = file.measure_disk {
        config.measure_disk = measure;
    }
    if let Some(repos) = file.extra_repos {
        config.extra_repos = repos.into_iter().map(PathBuf::from).collect();
    }
    config
}

/// Value of `--name <VALUE>` or `--name=<VALUE>`, last occurrence winning. A
/// missing value the user typed is a hard error, unlike a malformed config file:
/// they are looking right at it and silently ignoring it would be worse.
fn value_arg(args: &[String], name: &str) -> Result<Option<String>> {
    let flag = format!("{name}=");
    let mut found = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix(&flag) {
            found = Some(value.to_string());
        } else if arg == name {
            found = Some(rest.next().ok_or(format!("{name} needs a value"))?.clone());
        }
    }
    Ok(found)
}

/// Every occurrence of a repeatable `--name <VALUE>`.
fn values_arg(args: &[String], name: &str) -> Vec<String> {
    let flag = format!("{name}=");
    let mut found = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix(&flag) {
            found.push(value.to_string());
        } else if arg == name {
            if let Some(value) = rest.next() {
                found.push(value.clone());
            }
        }
    }
    found
}

pub fn plugin_id() -> String {
    non_empty_env("HERDR_PLUGIN_ID").unwrap_or_else(|| PLUGIN_ID.to_string())
}

/// Where the undo log lives: `~/.local/state/herdr/plugins/<id>/`.
///
/// herdr injects `HERDR_PLUGIN_STATE_DIR` into the commands it spawns and is
/// authoritative when it does, but the fallback has to resolve to the *same*
/// directory, or a removal made from a plugin action would not appear in the
/// undo log a hand-run `--undo-log` reads.
pub fn state_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_STATE_HOME", ".local/state")
                .join("herdr")
                .join("plugins")
                .join(plugin_id())
        })
}

/// Where the config file lives: `~/.config/herdr/plugins/config/<id>/`.
pub fn config_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_CONFIG_HOME", ".config")
                .join("herdr")
                .join("plugins")
                .join("config")
                .join(plugin_id())
        })
}

/// Append-only record of every removal shear has made.
pub fn undo_log() -> PathBuf {
    state_dir().join("removed.jsonl")
}

/// Last measured size per checkout, for the review pane's provisional first
/// frame. One JSON object per line; rewritten whole, never appended forever.
pub fn size_cache() -> PathBuf {
    state_dir().join("sizes.jsonl")
}

/// An XDG base directory. The variable wins when it is set to an absolute path
/// — the spec says a relative one must be ignored — otherwise `$HOME/<relative>`.
fn xdg_dir(variable: &str, relative: &str) -> PathBuf {
    if let Some(base) = non_empty_env(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return base;
    }
    match non_empty_env("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(home) => home.join(relative),
        None => std::env::temp_dir().join("herdr-no-home"),
    }
}

/// herdr injects empty strings for absent context, so empty means unset.
pub fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
