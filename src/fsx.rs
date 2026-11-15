//! Folder sizing and deletion that never follows junctions or symlinks.

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;

fn is_reparse(md: &fs::Metadata) -> bool {
    md.file_attributes() & ATTR_REPARSE != 0
}

fn is_dir_attr(md: &fs::Metadata) -> bool {
    md.file_attributes() & ATTR_DIRECTORY != 0
}

/// Total bytes of a file or folder. Links are counted as zero and not followed.
pub fn size_of(path: &Path) -> u64 {
    let Ok(md) = fs::symlink_metadata(path) else { return 0 };
    if is_reparse(&md) {
        return 0;
    }
    if !is_dir_attr(&md) {
        return md.len();
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if is_reparse(&md) {
                continue;
            }
            if is_dir_attr(&md) {
                stack.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

/// Children of `dir` that a Contents action would remove.
pub fn contents_entries(dir: &Path, older_than_days: Option<u64>, keep: &[PathBuf]) -> Vec<PathBuf> {
    let cutoff = older_than_days.map(|d| SystemTime::now() - Duration::from_secs(d * 86_400));
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| {
            let p = e.path();
            if keep.iter().any(|k| paths_eq(k, &p)) {
                return false;
            }
            match cutoff {
                None => true,
                Some(c) => e
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t < c)
                    .unwrap_or(false),
            }
        })
        .map(|e| e.path())
        .collect()
}

pub fn paths_eq(a: &Path, b: &Path) -> bool {
    a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
}

/// True if `child` is `parent` or inside it (case-insensitive).
pub fn is_within(child: &Path, parent: &Path) -> bool {
    let c = child.to_string_lossy().to_lowercase();
    let p = parent.to_string_lossy().to_lowercase();
    let p = p.trim_end_matches('\\');
    c == p || c.starts_with(&format!("{p}\\"))
}

#[derive(Default, Debug)]
pub struct DelStats {
    pub freed: u64,
    pub failed: u64,
    pub first_error: Option<String>,
}

impl DelStats {
    fn fail(&mut self, path: &Path, e: std::io::Error) {
        self.failed += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("{}: {e}", path.display()));
        }
    }
}

fn clear_readonly(path: &Path, md: &fs::Metadata) {
    if md.file_attributes() & ATTR_READONLY != 0 {
        let mut perm = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = fs::set_permissions(path, perm);
    }
}

/// Delete a file or folder tree. Locked files are skipped and counted.
pub fn delete_tree(path: &Path, stats: &mut DelStats) {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => return stats.fail(path, e),
    };
    if is_reparse(&md) {
        // Remove the link itself, never what it points to.
        let r = if is_dir_attr(&md) { fs::remove_dir(path) } else { fs::remove_file(path) };
        if let Err(e) = r {
            stats.fail(path, e);
        }
        return;
    }
    if is_dir_attr(&md) {
        if let Ok(rd) = fs::read_dir(path) {
            for e in rd.flatten() {
                delete_tree(&e.path(), stats);
            }
        }
        if fs::remove_dir(path).is_err() {
            clear_readonly(path, &md);
            if let Err(e) = fs::remove_dir(path) {
                // A folder that still holds locked files; those were already counted.
                if e.kind() != std::io::ErrorKind::DirectoryNotEmpty && stats.first_error.is_none() {
                    stats.first_error = Some(format!("{}: {e}", path.display()));
                }
            }
        }
    } else {
        let len = md.len();
        match fs::remove_file(path) {
            Ok(()) => stats.freed += len,
            Err(_) => {
                clear_readonly(path, &md);
                match fs::remove_file(path) {
                    Ok(()) => stats.freed += len,
                    Err(e) => stats.fail(path, e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("space-sweep-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn deletes_readonly_and_nested() {
        let d = tmp("ro");
        fs::create_dir_all(d.join("a/b/c")).unwrap();
        fs::write(d.join("a/b/c/f.bin"), vec![0u8; 5000]).unwrap();
        fs::write(d.join("a/ro.txt"), b"hello").unwrap();
        let mut p = fs::metadata(d.join("a/ro.txt")).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(d.join("a/ro.txt"), p).unwrap();
        assert_eq!(size_of(&d), 5005);
        let mut s = DelStats::default();
        delete_tree(&d, &mut s);
        assert_eq!(s.freed, 5005);
        assert_eq!(s.failed, 0);
        assert!(!d.exists());
    }

    #[test]
    fn junction_target_survives() {
        let d = tmp("junction");
        let target = tmp("junction-target");
        fs::write(target.join("keep.txt"), b"precious").unwrap();
        let link = d.join("link");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "mklink /J failed");
        // The junction adds nothing to the size and its target is never touched.
        assert_eq!(size_of(&d), 0);
        let mut s = DelStats::default();
        delete_tree(&d, &mut s);
        assert!(!d.exists());
        assert!(target.join("keep.txt").exists());
        fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn contents_respects_keep() {
        let d = tmp("keep");
        fs::write(d.join("x"), b"1").unwrap();
        fs::create_dir(d.join("claude")).unwrap();
        let e = contents_entries(&d, None, &[d.join("claude")]);
        assert_eq!(e, vec![d.join("x")]);
        fs::remove_dir_all(&d).unwrap();
    }
}
