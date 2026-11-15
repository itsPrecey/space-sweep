//! Deletes the selected items, then optionally compacts the WSL disk.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::fsx::{self, DelStats};
use crate::model::*;
use crate::win;
use crate::wsl;

pub struct Options {
    pub recycle_personal: bool,
    pub compact_wsl: bool,
}

fn summarize(stats: &DelStats) -> (String, bool) {
    if stats.failed == 0 {
        (format!("Deleted {}", fmt_size(stats.freed)), true)
    } else {
        let why = stats.first_error.as_deref().map(|e| format!(" First problem: {e}")).unwrap_or_default();
        (
            format!(
                "Deleted {}; {} file{} in use or protected, skipped.{why}",
                fmt_size(stats.freed),
                stats.failed,
                if stats.failed == 1 { " was" } else { "s were" }
            ),
            false,
        )
    }
}

fn targets(action: &Action) -> Vec<PathBuf> {
    match action {
        Action::Remove(p) => p.clone(),
        Action::Contents { dir, older_than_days, keep } => fsx::contents_entries(dir, *older_than_days, keep),
        _ => Vec::new(),
    }
}

pub fn run(mut items: Vec<Item>, opts: Options, tx: Sender<Msg>) {
    let log = |s: String| {
        let _ = tx.send(Msg::Log(s));
    };
    let (free_before, _) = win::disk_space("C:\\");
    let mut recycled = 0u64;
    let mut wsl_touched = false;

    // Empty the bin first, so files this run sends there aren't wiped with it.
    items.sort_by_key(|i| !matches!(i.action, Action::EmptyRecycleBin));

    for item in &items {
        let _ = tx.send(Msg::Status(format!("Deleting: {}", item.label)));
        let (status, gone, freed) = match &item.action {
            Action::None => continue,
            Action::Remove(_) | Action::Contents { .. } => {
                let paths = targets(&item.action);
                if item.personal && opts.recycle_personal {
                    match win::send_to_recycle_bin(&paths) {
                        Ok(()) => {
                            recycled += item.size;
                            ("Moved to the Recycle Bin".to_string(), true, 0)
                        }
                        Err(e) => (format!("Failed: {e}"), false, 0),
                    }
                } else {
                    let mut stats = DelStats::default();
                    for p in &paths {
                        fsx::delete_tree(p, &mut stats);
                    }
                    let (s, all) = summarize(&stats);
                    (s, all, stats.freed)
                }
            }
            Action::EmptyRecycleBin => {
                let size = win::recycle_bin_size();
                match win::empty_recycle_bin() {
                    Ok(()) => (format!("Emptied ({})", fmt_size(size)), true, size),
                    Err(e) => (format!("Failed: {e}"), false, 0),
                }
            }
            Action::WslRemove { distro, paths } => {
                wsl_touched = true;
                let script = format!(
                    "rm -rf -- {} 2>&1; echo done",
                    paths.iter().map(|p| wsl::sh_quote(p)).collect::<Vec<_>>().join(" ")
                );
                match wsl::run_script(distro, false, &script) {
                    Ok(_) => (format!("Deleted {} inside {distro}", fmt_size(item.size)), true, 0),
                    Err(e) => (format!("Failed: {e}"), false, 0),
                }
            }
            Action::WslRoot { distro, script } => {
                wsl_touched = true;
                match wsl::run_script(distro, true, script) {
                    Ok(_) => (format!("Cleared {} inside {distro}", fmt_size(item.size)), true, 0),
                    Err(e) => (format!("Failed: {e}"), false, 0),
                }
            }
            Action::DisableHibernation => match win::run_elevated("powercfg.exe", "/h off") {
                Ok(0) => ("Hibernation turned off".to_string(), true, item.size),
                Ok(code) => (format!("powercfg exited with code {code}"), false, 0),
                Err(e) => (format!("Skipped: {e}"), false, 0),
            },
        };
        log(format!("{}: {status}", item.label));
        let _ = tx.send(Msg::ItemResult { id: item.id, status, gone, freed });
    }

    if opts.compact_wsl {
        if !wsl_touched {
            log("Compacting the WSL disk…".into());
        }
        let _ = tx.send(Msg::Status("Compacting the WSL disk…".into()));
        let back = wsl::compact_all(&|s| {
            let _ = tx.send(Msg::Log(s));
        });
        if back > 0 {
            log(format!("WSL compaction gave {} back to Windows.", fmt_size(back)));
        }
    } else if wsl_touched {
        log("Space freed inside WSL only comes back to Windows after compaction. Tick \"Compact WSL disk\" and run again to get it.".into());
    }

    let (free_after, _) = win::disk_space("C:\\");
    log(format!(
        "C: free space: {} → {} ({} recovered).",
        fmt_size(free_before),
        fmt_size(free_after),
        fmt_size(free_after.saturating_sub(free_before))
    ));
    if recycled > 0 {
        log(format!(
            "{} of your own files are in the Recycle Bin. Empty it (or select Recycle Bin here) to get that space back.",
            fmt_size(recycled)
        ));
    }
    let _ = tx.send(Msg::WorkDone);
}
