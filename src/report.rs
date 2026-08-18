//! CI-shaped reporting for stale worktrees.
//!
//! `--report`: scan, size, print one JSON document, exit. A dry run by
//! construction — this verb has no path to `remove`.
//!
//! The only functions it can reach are [`crate::shear::scan`],
//! [`crate::disk::measure_all`] and [`crate::render::grouped`]. Adding a removal
//! here would mean importing `remove` or `tui`, which is why this module does
//! not.

use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::model::{Candidate, Class, Inventory, Size, Verdict};
use crate::{disk, render, shear, Result};

/// `--report`: scan, size, print the CI report, exit.
pub fn run_report(config: &Config) -> Result<()> {
    let mut inventory = shear::scan(config)?;
    if config.measure_disk {
        disk::measure_all(&mut inventory, &AtomicBool::new(false));
    }
    println!("{}", serde_json::to_string_pretty(&to_json(&inventory))?);
    Ok(())
}

/// JSON projection of an inventory for CI readers.
///
/// Written by hand rather than derived, so the versioned wire shape is a
/// deliberate choice and adding a field to a model struct cannot silently
/// change the public interface.
pub fn to_json(inventory: &Inventory) -> serde_json::Value {
    project(inventory, SystemTime::now())
}

fn project(inventory: &Inventory, generated_at: SystemTime) -> serde_json::Value {
    use serde_json::json;

    let repositories = render::grouped(inventory)
        .into_iter()
        .map(|(key, rows)| {
            let known_repo = inventory.repo(&key);
            let root = known_repo
                .map(|repo| repo.root.as_path())
                .unwrap_or(rows[0].worktree.repo_root.as_path());
            let name = known_repo.map(|repo| repo.name.clone()).unwrap_or_else(|| {
                root.file_name()
                    .map(|part| part.to_string_lossy().into_owned())
                    .unwrap_or_else(|| key.0.clone())
            });

            let mut safe = 0usize;
            let mut review = 0usize;
            let mut keep = 0usize;
            let mut blocked = 0usize;
            let mut reclaimable_bytes = 0u64;
            let mut reclaimable_unmeasured = 0usize;
            let mut total_bytes = 0u64;
            let mut total_unmeasured = 0usize;

            for candidate in &rows {
                match candidate.verdict {
                    Verdict::Safe => safe += 1,
                    Verdict::Review => review += 1,
                    Verdict::Keep => keep += 1,
                    Verdict::Blocked => blocked += 1,
                }

                match candidate.size {
                    Size::Bytes(bytes) => {
                        total_bytes = total_bytes.saturating_add(bytes);
                        if candidate.verdict == Verdict::Safe {
                            reclaimable_bytes = reclaimable_bytes.saturating_add(bytes);
                        }
                    }
                    Size::Gone => {}
                    Size::Pending | Size::Failed => {
                        total_unmeasured += 1;
                        if candidate.verdict == Verdict::Safe {
                            reclaimable_unmeasured += 1;
                        }
                    }
                }
            }

            let stale = rows
                .iter()
                .filter(|candidate| candidate.is(Class::Stale))
                .map(|candidate| stale_row(candidate, generated_at))
                .collect::<Vec<_>>();

            json!({
                "name": name,
                "root": root.to_string_lossy(),
                "worktree_count": rows.len(),
                "verdicts": {
                    "safe": safe,
                    "review": review,
                    "keep": keep,
                    "blocked": blocked,
                },
                "reclaimable_bytes": reclaimable_bytes,
                "reclaimable_unmeasured": reclaimable_unmeasured,
                "total_bytes": total_bytes,
                "total_unmeasured": total_unmeasured,
                "stale": stale,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "schema_version": 1,
        "generated_at": rfc3339_utc(generated_at),
        "notes": inventory.notes,
        "repositories": repositories,
    })
}

fn stale_row(candidate: &Candidate, generated_at: SystemTime) -> serde_json::Value {
    use serde_json::json;

    json!({
        "path": candidate.worktree.path.to_string_lossy(),
        "branch": candidate.branch(),
        "age_days": candidate.last_commit.and_then(|tip| {
            generated_at
                .duration_since(tip)
                .ok()
                .map(|age| age.as_secs() / 86_400)
        }),
        "classes": candidate.classes.iter().map(|class| class.label()).collect::<Vec<_>>(),
        "bytes": measured_bytes(candidate.size),
        "protected": candidate.is(Class::Protected),
    })
}

fn measured_bytes(size: Size) -> Option<u64> {
    match size {
        Size::Bytes(bytes) => Some(bytes),
        Size::Gone => Some(0),
        Size::Pending | Size::Failed => None,
    }
}

/// RFC 3339 in UTC, kept local so the report's import boundary cannot acquire a
/// path to removal code merely to share the undo log's tiny formatter.
fn rfc3339_utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

// Howard Hinnant's civil-from-days algorithm, exact for the full input range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}
