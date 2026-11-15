//! WSL: distros and their disk files, caches inside Linux, and disk compaction.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::model::*;
use crate::scan::next_id;
use crate::win;

#[derive(Clone, Debug)]
pub struct Distro {
    pub name: String,
    pub vhdx: Option<PathBuf>,
}

impl Distro {
    pub fn is_docker(&self) -> bool {
        self.name.to_ascii_lowercase().starts_with("docker-desktop")
    }
}

/// Registered distros and their disk files, from HKCU\...\Lxss.
pub fn distros() -> Vec<Distro> {
    let out = win::hidden("reg")
        .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss", "/s"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = win::decode(&out.stdout);
    let mut list = Vec::new();
    let (mut name, mut base, mut file): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    let mut flush = |name: &mut Option<String>, base: &mut Option<String>, file: &mut Option<String>| {
        if let Some(n) = name.take() {
            let vhdx = base.take().map(|b| {
                let b = b.trim_start_matches(r"\\?\").to_string();
                PathBuf::from(b).join(file.take().unwrap_or_else(|| "ext4.vhdx".into()))
            });
            list.push(Distro { name: n, vhdx: vhdx.filter(|p| p.exists()) });
        }
        *base = None;
        *file = None;
    };
    for line in text.lines() {
        if line.starts_with("HKEY_") {
            flush(&mut name, &mut base, &mut file);
            continue;
        }
        let t = line.trim();
        let Some((key, val)) = t.split_once("REG_SZ") else { continue };
        let (key, val) = (key.trim(), val.trim().to_string());
        match key {
            "DistributionName" => name = Some(val),
            "BasePath" => base = Some(val),
            "VhdFileName" => file = Some(val),
            _ => {}
        }
    }
    flush(&mut name, &mut base, &mut file);
    list
}

/// Every WSL / Docker disk file on this machine, with a display name.
pub fn disk_files() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = distros()
        .into_iter()
        .filter_map(|d| d.vhdx.map(|p| (d.name, p)))
        .collect();
    let local = win::env_path("LOCALAPPDATA").join("Docker").join("wsl");
    for rel in [r"disk\docker_data.vhdx", r"data\ext4.vhdx", r"main\ext4.vhdx", r"distro\ext4.vhdx"] {
        let p = local.join(rel);
        if p.exists() && !v.iter().any(|(_, q)| crate::fsx::paths_eq(q, &p)) {
            v.push(("Docker Desktop".into(), p));
        }
    }
    v
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Run a bash script inside a distro (via stdin), returning stdout.
pub fn run_script(distro: &str, root: bool, script: &str) -> Result<String, String> {
    let mut cmd = win::hidden("wsl.exe");
    cmd.args(["-d", distro]);
    if root {
        cmd.args(["-u", "root"]);
    }
    cmd.args(["-e", "bash", "-s"]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not start wsl.exe: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.replace("\r\n", "\n").as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = win::decode(&out.stdout);
    if out.status.success() {
        Ok(stdout)
    } else {
        let err = win::decode(&out.stderr);
        let msg = if err.trim().is_empty() { stdout } else { err };
        Err(msg.trim().to_string())
    }
}

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

const USER_SCAN: &str = r#"
emit() { local s; s=$(du -sb -- "$2" 2>/dev/null | cut -f1); [ -n "$s" ] && printf '%s\t%s\t%s\n' "$1" "$s" "$2"; }
cd ~ || exit 0
printf 'DF\t%s\t/\n' "$(df -B1 --output=used / | tail -1 | tr -d ' ')"
if [ -d ~/.cache ]; then for p in ~/.cache/* ~/.cache/.[!.]*; do [ -e "$p" ] && emit CACHE "$p"; done; fi
for p in ~/.npm/_cacache ~/.npm/_npx ~/.cargo/registry ~/.cargo/git ~/.local/share/pnpm/store ~/.local/share/Trash ~/.nvm/.cache ~/.bun/install/cache ~/.gradle/caches ~/.vscode-server/data/CachedExtensionVSIXs ~/go/pkg/mod; do
  [ -e "$p" ] && emit KNOWN "$p"
done
for d in ~/.vscode-server/bin ~/.vscode-server/cli/servers ~/.cursor-server/bin ~/.cursor-server/cli/servers ~/.element-server/bin ~/.windsurf-server/bin; do
  [ -d "$d" ] || continue
  ls -1t "$d" 2>/dev/null | tail -n +3 | while read -r v; do [ -d "$d/$v" ] && emit OLDSERVER "$d/$v"; done
done
find ~ -xdev -maxdepth 6 \( -type d \( -name node_modules -o -name target -o -name .next -o -name .turbo -o -name .venv \) -prune -print \) -o \( -name '.*' -prune \) 2>/dev/null | while read -r d; do
  parent=$(dirname -- "$d"); base=$(basename -- "$d")
  case "$base" in
    target) [ -f "$parent/Cargo.toml" ] && emit RUST "$d" ;;
    node_modules) [ -f "$parent/package.json" ] && emit NODE "$d" ;;
    .next|.turbo) emit WEB "$d" ;;
    .venv) [ -f "$d/pyvenv.cfg" ] && emit VENV "$d" ;;
  esac
done
exit 0
"#;

const ROOT_SCAN: &str = r#"
emit() { local s; s=$(du -sb -- "$2" 2>/dev/null | cut -f1); [ -n "$s" ] && printf '%s\t%s\t%s\n' "$1" "$s" "$2"; }
for p in /var/cache/apt/archives /var/log/journal /var/lib/snapd/cache /var/lib/docker; do [ -e "$p" ] && emit ROOT "$p"; done
exit 0
"#;

fn cache_label(name: &str) -> (String, Tier, &'static str) {
    let n = name.trim_start_matches('.');
    let (label, tier, detail): (&str, Tier, &str) = match n {
        "pip" => ("pip cache", Tier::Safe, "Python package downloads. Refills on demand."),
        "ms-playwright" => ("Playwright browsers", Tier::Safe, "Test browsers. Reinstall with `npx playwright install`."),
        "pnpm" => ("pnpm cache", Tier::Safe, "pnpm metadata cache. Refills on its own."),
        "electron" => ("Electron cache", Tier::Safe, "Electron downloads. Fetched again when needed."),
        "electron-builder" => ("electron-builder cache", Tier::Safe, "Packaging tools. Fetched again when needed."),
        "typescript" => ("TypeScript cache", Tier::Safe, "Type definitions. Refills on demand."),
        "go-build" => ("Go build cache", Tier::Safe, "Rebuilt on the next go build."),
        "yarn" => ("Yarn cache", Tier::Safe, "Package downloads. Refills on install."),
        "puppeteer" => ("Puppeteer browsers", Tier::Safe, "Downloaded again on the next install."),
        "node-gyp" => ("node-gyp headers", Tier::Safe, "Fetched again when needed."),
        "huggingface" => ("Hugging Face models", Tier::Review, "Downloaded AI models. Re-downloading can be slow."),
        "torch" => ("PyTorch models", Tier::Review, "Downloaded model weights."),
        "lm-studio" => ("LM Studio models", Tier::Review, "Models you downloaded."),
        _ => ("", Tier::Safe, "Linux cache folder (~/.cache). Programs refill it when needed."),
    };
    let label = if label.is_empty() { format!("Linux cache: {n}") } else { label.to_string() };
    (label, tier, detail)
}

fn known_label(path: &str) -> (&'static str, Tier, &'static str) {
    if path.ends_with("/.npm/_cacache") {
        ("npm cache", Tier::Safe, "Downloaded npm packages. Refills on install.")
    } else if path.ends_with("/.npm/_npx") {
        ("npx cache", Tier::Safe, "Packages fetched by npx. Fetched again when run.")
    } else if path.ends_with("/.cargo/registry") {
        ("Cargo registry", Tier::Safe, "Downloaded Rust crates. Fetched on the next build.")
    } else if path.ends_with("/.cargo/git") {
        ("Cargo git checkouts", Tier::Safe, "Fetched again when needed.")
    } else if path.ends_with("/pnpm/store") {
        ("pnpm store", Tier::Safe, "Shared package store. Projects keep working; pnpm downloads on the next install.")
    } else if path.ends_with("/Trash") {
        ("Linux trash", Tier::Review, "Files deleted from Linux desktop apps.")
    } else if path.ends_with("/.nvm/.cache") {
        ("nvm download cache", Tier::Safe, "Node installers already unpacked.")
    } else if path.ends_with("/.bun/install/cache") {
        ("Bun cache", Tier::Safe, "Package downloads. Refills on install.")
    } else if path.ends_with("/.gradle/caches") {
        ("Gradle caches", Tier::Safe, "Rebuilt on the next build.")
    } else if path.ends_with("CachedExtensionVSIXs") {
        ("VS Code Server extension installers", Tier::Safe, "Copies of installed extensions' installers.")
    } else if path.ends_with("/go/pkg/mod") {
        ("Go module cache", Tier::Review, "Downloaded Go modules. Fetched again on build.")
    } else {
        ("Linux folder", Tier::Review, "")
    }
}

fn short_home(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// Scan every (non-Docker) distro and report items plus the disk-file info rows.
pub fn scan(tx: &Sender<Msg>) {
    let distros = distros();
    let disks = disk_files();
    let mut hint = Vec::new();
    for d in distros.iter().filter(|d| !d.is_docker()) {
        let _ = tx.send(Msg::Status(format!("Scanning inside WSL ({})…", d.name)));
        let mut used: Option<u64> = None;
        match run_script(&d.name, false, USER_SCAN) {
            Ok(out) => {
                let home = run_script(&d.name, false, "printf %s \"$HOME\"").unwrap_or_default();
                for line in out.lines() {
                    let mut parts = line.splitn(3, '\t');
                    let (Some(kind), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
                    let size: u64 = size.trim().parse().unwrap_or(0);
                    if kind == "DF" {
                        used = Some(size);
                        continue;
                    }
                    if size < 10 * 1024 * 1024 {
                        continue;
                    }
                    let short = short_home(path, &home);
                    let parent = short.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
                    let (label, tier, detail) = match kind {
                        "CACHE" => {
                            let name = path.rsplit('/').next().unwrap_or(path);
                            let (l, t, dt) = cache_label(name);
                            (l, t, dt.to_string())
                        }
                        "KNOWN" => {
                            let (l, t, dt) = known_label(path);
                            (l.to_string(), t, dt.to_string())
                        }
                        "OLDSERVER" => (
                            format!("Old editor server build · {}", parent.trim_start_matches("~/")),
                            Tier::Safe,
                            "Older VS Code / Cursor / Element server builds. The newest two are kept.".into(),
                        ),
                        "RUST" => (format!("Rust build output · {parent}"), Tier::Safe, "Rebuilt by the next `cargo build`.".into()),
                        "NODE" => (
                            format!("Node packages · {parent}"),
                            Tier::Review,
                            "Restore with `npm install` / `pnpm install`.".into(),
                        ),
                        "WEB" => (format!("Web build cache · {parent}"), Tier::Safe, "Rebuilt on the next dev or build run.".into()),
                        "VENV" => (format!("Python virtual env · {parent}"), Tier::Review, "Recreate it and reinstall packages.".into()),
                        _ => continue,
                    };
                    let _ = tx.send(Msg::Item(Item {
                        id: next_id(),
                        label,
                        detail,
                        location: format!("{}: {short}", d.name),
                        size,
                        tier,
                        group: Group::Wsl,
                        action: Action::WslRemove { distro: d.name.clone(), paths: vec![path.to_string()] },
                        personal: false,
                        needs_admin: false,
                        selected: false,
                        status: None,
                        gone: false,
                    }));
                }
            }
            Err(e) => {
                let _ = tx.send(Msg::Log(format!("Couldn't scan inside {}: {e}", d.name)));
            }
        }
        if let Ok(out) = run_script(&d.name, true, ROOT_SCAN) {
            for line in out.lines() {
                let mut parts = line.splitn(3, '\t');
                let (Some(_), Some(size), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
                let size: u64 = size.trim().parse().unwrap_or(0);
                let (label, detail, tier, script, size) = match path {
                    "/var/cache/apt/archives" => ("apt package downloads", "Installed .deb files. `apt-get clean` removes them.", Tier::Safe, "apt-get clean", size),
                    "/var/log/journal" => (
                        "Linux system logs (journald)",
                        "Old system logs. Keeps the newest 50 MB.",
                        Tier::Safe,
                        "journalctl --vacuum-size=50M",
                        size.saturating_sub(50 * 1024 * 1024),
                    ),
                    "/var/lib/snapd/cache" => ("Snap download cache", "Snap packages already installed.", Tier::Safe, "rm -rf /var/lib/snapd/cache/*", size),
                    "/var/lib/docker" => (
                        "Docker data inside WSL",
                        "Images, containers and volumes. Clear unused ones with `docker system prune`.",
                        Tier::Info,
                        "",
                        size,
                    ),
                    _ => continue,
                };
                if size < 10 * 1024 * 1024 {
                    continue;
                }
                let action = if script.is_empty() {
                    Action::None
                } else {
                    Action::WslRoot { distro: d.name.clone(), script: script.into() }
                };
                let _ = tx.send(Msg::Item(Item {
                    id: next_id(),
                    label: label.into(),
                    detail: detail.into(),
                    location: format!("{}: {path}", d.name),
                    size,
                    tier,
                    group: Group::Wsl,
                    action,
                    personal: false,
                    needs_admin: false,
                    selected: false,
                    status: None,
                    gone: false,
                }));
            }
        }
        if let (Some(used), Some(vhdx)) = (used, d.vhdx.as_ref()) {
            let file = file_len(vhdx);
            let slack = file.saturating_sub(used);
            hint.push(format!("{}: disk file {} holds {} of data, so about {} could come back", d.name, fmt_size(file), fmt_size(used), fmt_size(slack)));
        }
    }
    for (name, p) in &disks {
        let size = file_len(p);
        let _ = tx.send(Msg::Item(Item {
            id: next_id(),
            label: format!("WSL disk file · {name}"),
            detail: "The Linux disk. It grows but never shrinks on its own. Tick \"Compact WSL disk\" at the bottom to shrink it.".into(),
            location: p.display().to_string(),
            size,
            tier: Tier::Info,
            group: Group::Wsl,
            action: Action::None,
            personal: false,
            needs_admin: false,
            selected: false,
            status: None,
            gone: false,
        }));
    }
    let _ = tx.send(Msg::WslInfo { reclaimable_hint: hint.join("; ") });
}

fn running_distros() -> Vec<String> {
    match win::hidden("wsl.exe").args(["-l", "--running", "-q"]).output() {
        Ok(o) => win::decode(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.contains(' '))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Programs that currently hold a WSL connection open (the parents of `wsl.exe` clients).
pub fn wsl_users() -> Vec<String> {
    let procs = win::processes();
    let me = std::process::id();
    let name_of = |pid: u32| procs.iter().find(|p| p.0 == pid).map(|p| (p.1, p.2.clone()));
    let mut users: Vec<String> = Vec::new();
    for (pid, ppid, name) in &procs {
        if !name.eq_ignore_ascii_case("wsl.exe") {
            continue;
        }
        let _ = pid;
        // Walk up past nested wsl.exe processes to the program that started them.
        let mut parent = *ppid;
        let mut found = None;
        for _ in 0..8 {
            match name_of(parent) {
                Some((pp, n)) if n.eq_ignore_ascii_case("wsl.exe") => parent = pp,
                Some((_, n)) => {
                    found = Some(n);
                    break;
                }
                None => break,
            }
        }
        if parent == me {
            continue;
        }
        if let Some(n) = found {
            let friendly = friendly_app(&n);
            if !users.contains(&friendly) {
                users.push(friendly);
            }
        }
    }
    if procs.iter().any(|p| p.2.eq_ignore_ascii_case("Docker Desktop.exe")) {
        users.push("Docker Desktop".into());
    }
    users
}

fn friendly_app(exe: &str) -> String {
    match exe.to_ascii_lowercase().as_str() {
        "claude.exe" => "the Claude desktop app (its connection to WSL sessions)".into(),
        "code.exe" | "code - insiders.exe" => "VS Code (a window connected to WSL)".into(),
        "cursor.exe" => "Cursor (a window connected to WSL)".into(),
        "windowsterminal.exe" | "openconsole.exe" => "a Windows Terminal tab running WSL".into(),
        "explorer.exe" => "File Explorer (a \\\\wsl$ folder or WSL shortcut)".into(),
        "powershell.exe" | "pwsh.exe" | "cmd.exe" | "bash.exe" => format!("a {exe} window running WSL"),
        other => other.trim_end_matches(".exe").to_string(),
    }
}

fn shutdown_and_wait(log: &dyn Fn(String)) -> bool {
    let _ = win::hidden("wsl.exe").arg("--shutdown").output();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if running_distros().is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log("WSL started again right after shutdown.".into());
    false
}

const ATTEMPTS: usize = 3;

fn work_dir() -> PathBuf {
    std::env::temp_dir().join("space-sweep").join("compact")
}

fn wait_for(path: &Path, done: &std::sync::mpsc::Receiver<Result<u32, String>>, limit: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if path.exists() {
            return Ok(());
        }
        match done.try_recv() {
            Ok(Ok(_)) => return Err("the admin helper stopped early".into()),
            Ok(Err(e)) => return Err(e),
            Err(_) => {}
        }
        if start.elapsed() > limit {
            return Err("timed out waiting for the admin helper".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Trim, then compact every WSL disk file. The admin helper starts first and waits with
/// diskpart ready; only then is WSL shut down, so nothing can restart it before diskpart
/// takes the disk. Returns bytes given back to Windows.
pub fn compact_all(log: &dyn Fn(String)) -> u64 {
    let mut disks = disk_files();
    if disks.is_empty() {
        log("No WSL disk files found, so there's nothing to compact.".into());
        return 0;
    }
    // Ubuntu (and other distros) first: those are the ones other apps restart.
    disks.sort_by_key(|(n, _)| n == "Docker Desktop");
    for d in distros().iter().filter(|d| !d.is_docker()) {
        log(format!("Marking free space inside {} (fstrim)…", d.name));
        match run_script(&d.name, true, "fstrim -v / 2>&1 || true") {
            Ok(out) => {
                let line = out.lines().last().unwrap_or("").trim().to_string();
                if !line.is_empty() {
                    log(format!("  {line}"));
                }
            }
            Err(e) => log(format!("  fstrim failed: {e}")),
        }
    }
    let users = wsl_users();
    if !users.is_empty() {
        log(format!(
            "Using WSL right now: {}. It will reconnect after compaction; while the disk is being compacted it can't reach WSL.",
            users.join(", ")
        ));
    }

    let work = work_dir();
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = std::fs::create_dir_all(&work) {
        log(format!("Couldn't create {}: {e}", work.display()));
        return 0;
    }
    let list: String = disks.iter().map(|(_, p)| format!("{}\n", p.display())).collect();
    if let Err(e) = std::fs::write(work.join("disks.txt"), list) {
        log(format!("Couldn't write the disk list: {e}"));
        return 0;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log(format!("Couldn't find this program's path: {e}"));
            return 0;
        }
    };
    let before: Vec<u64> = disks.iter().map(|(_, p)| file_len(p)).collect();

    log("Approve the admin prompt. WSL shuts down only after that.".into());
    let (dtx, drx) = std::sync::mpsc::channel();
    let params = format!("--compact-helper \"{}\"", work.display());
    let exe_s = exe.display().to_string();
    std::thread::spawn(move || {
        let _ = dtx.send(win::run_elevated(&exe_s, &params));
    });

    for attempt in 1..=ATTEMPTS {
        if let Err(e) = wait_for(&work.join(format!("ready-{attempt}")), &drx, Duration::from_secs(180)) {
            log(format!("Compaction skipped: {e}."));
            break;
        }
        log(if attempt == 1 { "Shutting WSL down…".into() } else { format!("Attempt {attempt}: shutting WSL down again…") });
        shutdown_and_wait(log);
        let _ = std::fs::write(work.join(format!("go-{attempt}")), "go");
        log("Compacting. This can take several minutes…".into());
        if let Err(e) = wait_for(&work.join(format!("done-{attempt}")), &drx, Duration::from_secs(3600)) {
            log(format!("Compaction stopped: {e}."));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        let result = std::fs::read_to_string(work.join(format!("done-{attempt}"))).unwrap_or_default();
        let mut in_use = false;
        for line in result.lines() {
            let (status, path) = line.split_once('\t').unwrap_or((line, ""));
            let name = disks
                .iter()
                .find(|(_, p)| p.display().to_string() == path)
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| path.to_string());
            match status {
                "ok" => log(format!("  {name}: compacted")),
                "inuse" => {
                    in_use = true;
                    log(format!("  {name}: still in use by WSL"));
                }
                other => log(format!("  {name}: {other}")),
            }
        }
        if !in_use {
            break;
        }
        let users = wsl_users();
        if attempt == ATTEMPTS {
            log(format!(
                "WSL kept restarting{}. Close it and press \"Compact WSL disk now\" again.",
                if users.is_empty() { String::new() } else { format!(" (started by {})", users.join(", ")) }
            ));
        } else if !users.is_empty() {
            log(format!("WSL was restarted by {}.", users.join(", ")));
        }
    }
    // Let the helper finish (it exits on its own after its last attempt).
    let _ = drx.recv_timeout(Duration::from_secs(30));

    let mut gained = 0;
    for ((name, p), b) in disks.iter().zip(before) {
        let a = file_len(p);
        gained += b.saturating_sub(a);
        log(format!("{name} disk: {} → {} ({} back)", fmt_size(b), fmt_size(a), fmt_size(b.saturating_sub(a))));
    }
    gained
}

/// Runs elevated (`--compact-helper <dir>`). For each attempt: start diskpart with the first
/// pending disk selected, signal ready, wait for go, then attach/compact/detach every pending disk.
pub fn compact_helper(dir: &Path) {
    let disks: Vec<String> = std::fs::read_to_string(dir.join("disks.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect();
    let mut pending = disks.clone();
    let mut all_log = String::new();
    for attempt in 1..=ATTEMPTS {
        if pending.is_empty() {
            break;
        }
        let mut first = Some(spawn_diskpart(&pending[0]));
        let _ = std::fs::write(dir.join(format!("ready-{attempt}")), "ready");
        let go = dir.join(format!("go-{attempt}"));
        let start = Instant::now();
        while !go.exists() {
            if start.elapsed() > Duration::from_secs(180) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let mut results = Vec::new();
        let mut next = Vec::new();
        for disk in &pending {
            let handle = first.take().unwrap_or_else(|| spawn_diskpart(disk));
            let out = handle.map(finish_diskpart).unwrap_or_else(|e| format!("error: {e}"));
            all_log.push_str(&format!("=== attempt {attempt}: {disk}\n{out}\n"));
            let lower = out.to_ascii_lowercase();
            let status = if lower.contains("used by another process") {
                next.push(disk.clone());
                "inuse".to_string()
            } else if lower.contains("successfully compacted") {
                "ok".to_string()
            } else {
                let err = out
                    .lines()
                    .find(|l| l.to_ascii_lowercase().contains("error"))
                    .unwrap_or("diskpart didn't report success")
                    .trim()
                    .to_string();
                err
            };
            results.push(format!("{status}\t{disk}"));
        }
        let _ = std::fs::write(dir.join("helper.log"), &all_log);
        let _ = std::fs::write(dir.join(format!("done-{attempt}")), results.join("\n"));
        pending = next;
    }
}

struct Diskpart {
    child: std::process::Child,
    out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

/// Start diskpart with the disk already selected, so attaching on "go" is instant.
fn spawn_diskpart(disk: &str) -> Result<Diskpart, String> {
    use std::io::Read;
    let mut child = win::hidden("diskpart.exe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start diskpart: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = out.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(format!("select vdisk file=\"{disk}\"\r\n").as_bytes());
        let _ = stdin.flush();
    }
    Ok(Diskpart { child, out, reader })
}

fn finish_diskpart(mut dp: Diskpart) -> String {
    if let Some(mut stdin) = dp.child.stdin.take() {
        let _ = stdin.write_all(b"attach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\nexit\r\n");
        let _ = stdin.flush();
    }
    let _ = dp.child.wait();
    let _ = dp.reader.join();
    let bytes = dp.out.lock().unwrap().clone();
    win::decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a WSL distro. Run with `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn removes_quoted_paths_inside_wsl() {
        let d = distros().into_iter().find(|d| !d.is_docker()).expect("no WSL distro");
        let dir = "/tmp/space sweep test/it's here";
        run_script(&d.name, false, &format!("mkdir -p {q}/sub && head -c 4096 /dev/zero > {q}/sub/f", q = sh_quote(dir))).unwrap();
        let out = run_script(&d.name, false, &format!("[ -f {}/sub/f ] && echo yes", sh_quote(dir))).unwrap();
        assert_eq!(out.trim(), "yes");
        // Same command shape as the cleaner uses.
        run_script(&d.name, false, &format!("rm -rf -- {} 2>&1; echo done", sh_quote("/tmp/space sweep test"))).unwrap();
        let out = run_script(&d.name, false, "[ -e '/tmp/space sweep test' ] && echo still-there || echo gone").unwrap();
        assert_eq!(out.trim(), "gone");
    }

    #[test]
    fn finds_the_ubuntu_disk_file() {
        let disks = disk_files();
        assert!(disks.iter().all(|(_, p)| p.extension().map(|e| e == "vhdx").unwrap_or(false)));
    }

    /// The app/helper hand-off: ready -> go -> done, using a disk path diskpart can't open.
    /// Without admin rights diskpart can't start at all, which the helper must report as an error
    /// (not "in use"), so it stops after one attempt.
    #[test]
    fn helper_handshake_runs_one_attempt() {
        let dir = std::env::temp_dir().join(format!("space-sweep-handshake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disks.txt"), "C:\\nope\\missing.vhdx\n").unwrap();
        let d2 = dir.clone();
        let helper = std::thread::spawn(move || compact_helper(&d2));
        let start = Instant::now();
        while !dir.join("ready-1").exists() {
            assert!(start.elapsed() < Duration::from_secs(20), "helper never got ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::write(dir.join("go-1"), "go").unwrap();
        helper.join().unwrap();
        let done = std::fs::read_to_string(dir.join("done-1")).unwrap();
        assert!(!done.starts_with("inuse"), "{done}");
        assert!(done.ends_with(r"C:\nope\missing.vhdx"), "{done}");
        assert!(!dir.join("ready-2").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Prints which programs hold WSL open right now.
    #[test]
    #[ignore]
    fn reports_wsl_users() {
        let users = wsl_users();
        println!("WSL users: {users:?}");
        assert!(!users.is_empty());
    }
}
