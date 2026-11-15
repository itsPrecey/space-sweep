//! Finds what is using space and labels it.

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

use rayon::prelude::*;

use crate::fsx;
use crate::model::*;
use crate::win;

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
pub fn next_id() -> usize {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * MB;
const MIN_ITEM: u64 = 10 * MB;
const MIN_INFO: u64 = 300 * MB;
const LARGE_FILE: u64 = 500 * MB;

pub struct Env {
    pub user: PathBuf,
    pub local: PathBuf,
    pub roaming: PathBuf,
    pub temp: PathBuf,
    pub locallow: PathBuf,
}

impl Env {
    pub fn new() -> Env {
        let user = win::env_path("USERPROFILE");
        Env {
            local: win::env_path("LOCALAPPDATA"),
            roaming: win::env_path("APPDATA"),
            temp: win::env_path("TEMP"),
            locallow: user.join("AppData").join("LocalLow"),
            user,
        }
    }
}

enum Measure {
    Paths(Vec<PathBuf>),
    Contents { dir: PathBuf, older: Option<u64>, keep: Vec<PathBuf> },
    Fixed(u64),
}

pub struct Cand {
    label: String,
    detail: String,
    location: String,
    tier: Tier,
    group: Group,
    action: Action,
    personal: bool,
    needs_admin: bool,
    measure: Measure,
}

impl Cand {
    fn remove(label: impl Into<String>, detail: impl Into<String>, tier: Tier, group: Group, paths: Vec<PathBuf>) -> Cand {
        let location = describe_paths(&paths);
        Cand {
            label: label.into(),
            detail: detail.into(),
            location,
            tier,
            group,
            action: Action::Remove(paths.clone()),
            personal: false,
            needs_admin: false,
            measure: Measure::Paths(paths),
        }
    }

    fn contents(
        label: impl Into<String>,
        detail: impl Into<String>,
        tier: Tier,
        group: Group,
        dir: PathBuf,
        older: Option<u64>,
        keep: Vec<PathBuf>,
    ) -> Cand {
        Cand {
            label: label.into(),
            detail: detail.into(),
            location: dir.display().to_string(),
            tier,
            group,
            action: Action::Contents { dir: dir.clone(), older_than_days: older, keep: keep.clone() },
            personal: false,
            needs_admin: false,
            measure: Measure::Contents { dir, older, keep },
        }
    }

    fn fixed(label: impl Into<String>, detail: impl Into<String>, location: impl Into<String>, tier: Tier, group: Group, action: Action, size: u64) -> Cand {
        Cand {
            label: label.into(),
            detail: detail.into(),
            location: location.into(),
            tier,
            group,
            action,
            personal: false,
            needs_admin: false,
            measure: Measure::Fixed(size),
        }
    }

    fn personal(mut self) -> Cand {
        self.personal = true;
        self
    }

    fn admin(mut self) -> Cand {
        self.needs_admin = true;
        self
    }

    fn paths(&self) -> Vec<PathBuf> {
        match &self.measure {
            Measure::Paths(p) => p.clone(),
            Measure::Contents { dir, .. } => vec![dir.clone()],
            Measure::Fixed(_) => Vec::new(),
        }
    }

    fn measure(&self) -> u64 {
        match &self.measure {
            Measure::Paths(p) => p.iter().map(|p| fsx::size_of(p)).sum(),
            Measure::Contents { dir, older, keep } => {
                fsx::contents_entries(dir, *older, keep).iter().map(|p| fsx::size_of(p)).sum()
            }
            Measure::Fixed(s) => *s,
        }
    }

    fn min_size(&self) -> u64 {
        match (&self.action, self.tier) {
            (Action::EmptyRecycleBin, _) => MB,
            (_, Tier::Info) => MIN_INFO,
            _ => MIN_ITEM,
        }
    }

    pub fn into_item(self, size: u64) -> Item {
        Item {
            id: next_id(),
            label: self.label,
            detail: self.detail,
            location: self.location,
            size,
            tier: self.tier,
            group: self.group,
            action: self.action,
            personal: self.personal,
            needs_admin: self.needs_admin,
            selected: false,
            status: None,
            gone: false,
        }
    }
}

fn describe_paths(paths: &[PathBuf]) -> String {
    match paths.len() {
        0 => String::new(),
        1 => paths[0].display().to_string(),
        n => {
            let parent = common_parent(paths);
            format!("{} ({n} places)", parent.display())
        }
    }
}

fn common_parent(paths: &[PathBuf]) -> PathBuf {
    let mut base = paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
    while !paths.iter().all(|p| p.starts_with(&base)) {
        match base.parent() {
            Some(p) => base = p.to_path_buf(),
            None => break,
        }
    }
    base
}

fn is_dir(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.is_dir() && m.file_attributes() & 0x400 == 0)
        .unwrap_or(false)
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(p) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.metadata().map(|m| m.is_dir() && m.file_attributes() & 0x400 == 0).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| win::exists(p)).collect()
}

// ---------------------------------------------------------------- catalog

fn known_caches(e: &Env, out: &mut Vec<Cand>) {
    let (u, l) = (&e.user, &e.local);
    let safe: Vec<(&str, PathBuf, &str)> = vec![
        ("npm cache", l.join("npm-cache"), "Downloaded npm packages. Refills on the next install."),
        ("pnpm store", l.join("pnpm").join("store"), "pnpm's shared package store. Projects keep working; pnpm downloads again on the next install."),
        ("pnpm cache", l.join("pnpm-cache"), "pnpm metadata cache. Refills on its own."),
        ("Yarn cache", l.join("Yarn").join("Cache"), "Downloaded Yarn packages. Refills on the next install."),
        ("pip download cache", l.join("pip").join("Cache"), "Python package downloads. Refills on the next pip install."),
        ("uv cache", l.join("uv").join("cache"), "Python packages cached by uv. Refills on demand."),
        ("Cargo registry (Rust crate downloads)", u.join(".cargo").join("registry"), "Downloaded Rust crates. The next cargo build downloads what it needs."),
        ("Cargo git checkouts", u.join(".cargo").join("git"), "Git dependencies for Rust projects. Fetched again when needed."),
        ("Gradle caches", u.join(".gradle").join("caches"), "Android/Java build cache. Rebuilt on the next Gradle build."),
        ("NuGet packages (.NET)", u.join(".nuget").join("packages"), "Downloaded .NET packages. Restored on the next build."),
        ("Electron download cache", l.join("electron").join("Cache"), "Electron runtimes downloaded by builds. Fetched again when needed."),
        ("electron-builder cache", l.join("electron-builder").join("Cache"), "Packaging tools for Electron apps. Fetched again when needed."),
        ("node-gyp headers", l.join("node-gyp").join("Cache"), "Node headers for native modules. Fetched again when needed."),
        ("Playwright test browsers", l.join("ms-playwright"), "Browsers for Playwright tests. Reinstall with `npx playwright install`."),
        ("Squirrel installer temp", l.join("SquirrelTemp"), "Leftovers from app installs and updates."),
        ("TypeScript type cache (VS Code)", l.join("Microsoft").join("TypeScript"), "Type definitions VS Code downloads. Refills on demand."),
        ("App crash dumps", l.join("CrashDumps"), "Memory dumps from crashed apps. Only useful for debugging those crashes."),
        ("DirectX shader cache", l.join("D3DSCache"), "Compiled graphics shaders. Rebuilt as games and apps run."),
        ("NVIDIA shader cache", l.join("NVIDIA").join("DXCache"), "Compiled graphics shaders. Rebuilt automatically."),
        ("NVIDIA OpenGL cache", l.join("NVIDIA").join("GLCache"), "Compiled graphics shaders. Rebuilt automatically."),
        ("AMD shader cache", l.join("AMD").join("DxCache"), "Compiled graphics shaders. Rebuilt automatically."),
        ("Intel shader cache", e.locallow.join("Intel").join("ShaderCache"), "Compiled graphics shaders. Rebuilt automatically."),
    ];
    for (label, path, detail) in safe {
        if win::exists(&path) {
            out.push(Cand::remove(label, detail, Tier::Safe, Group::Caches, vec![path]));
        }
    }

    let review: Vec<(&str, PathBuf, &str)> = vec![
        ("Hugging Face model cache", u.join(".cache").join("huggingface"), "Downloaded AI models and datasets. Re-downloading can be slow and large."),
        ("PyTorch model cache", u.join(".cache").join("torch"), "Downloaded model weights. Re-downloaded when a script needs them."),
        ("LM Studio models (old location)", u.join(".cache").join("lm-studio").join("models"), "Models you downloaded in LM Studio."),
        ("LM Studio models", u.join(".lmstudio").join("models"), "Models you downloaded in LM Studio."),
        ("Ollama models", u.join(".ollama").join("models"), "Models pulled with `ollama pull`."),
        ("New Outlook offline mail", l.join("Microsoft").join("Olk"), "Outlook's local copy of your mail. It syncs back down from the server."),
    ];
    for (label, path, detail) in review {
        if win::exists(&path) {
            out.push(Cand::remove(label, detail, Tier::Review, Group::Caches, vec![path]));
        }
    }

    // Other tool caches under ~/.cache (Windows side).
    let skip = ["huggingface", "torch", "lm-studio"];
    for d in subdirs(&u.join(".cache")) {
        let n = name_of(&d);
        if skip.contains(&n.as_str()) {
            continue;
        }
        out.push(Cand::remove(
            format!("Tool cache: {n}"),
            "Cache folder used by command-line tools. Refilled on demand.",
            Tier::Safe,
            Group::Caches,
            vec![d],
        ));
    }

    // Temp, keeping Claude's own working folder.
    out.push(Cand::contents(
        "Your Temp folder (older than 1 day)",
        "Leftover temporary files. Anything changed in the last day, or still in use, is kept.",
        Tier::Safe,
        Group::Caches,
        e.temp.clone(),
        Some(1),
        vec![e.temp.join("claude"), e.temp.join("space-sweep")],
    ));
}

fn browsers(e: &Env, out: &mut Vec<Cand>) {
    let chromium = [
        ("Chrome", e.local.join("Google").join("Chrome").join("User Data")),
        ("Edge", e.local.join("Microsoft").join("Edge").join("User Data")),
        ("Brave", e.local.join("BraveSoftware").join("Brave-Browser").join("User Data")),
    ];
    let per_profile = ["Cache", "Code Cache", "GPUCache", "DawnCache", "DawnGraphiteCache", "DawnWebGPUCache"];
    for (name, ud) in chromium {
        if !is_dir(&ud) {
            continue;
        }
        let profiles: Vec<PathBuf> = subdirs(&ud)
            .into_iter()
            .filter(|p| {
                let n = name_of(p);
                n == "Default" || n.starts_with("Profile ") || n == "Guest Profile"
            })
            .collect();
        let mut paths: Vec<PathBuf> = Vec::new();
        for p in &profiles {
            paths.extend(existing(per_profile.iter().map(|c| p.join(c))));
        }
        paths.extend(existing(["ShaderCache", "GrShaderCache", "GraphiteDawnCache"].iter().map(|c| ud.join(c))));
        if !paths.is_empty() {
            out.push(Cand::remove(
                format!("{name} cache ({} profile{})", profiles.len(), if profiles.len() == 1 { "" } else { "s" }),
                format!("Only cache folders. Logins, history and bookmarks stay. Close {name} first for a full clean."),
                Tier::Safe,
                Group::Browsers,
                paths,
            ));
        }
    }
    let ff = e.local.join("Mozilla").join("Firefox").join("Profiles");
    let mut paths = Vec::new();
    for p in subdirs(&ff) {
        paths.extend(existing(["cache2", "startupCache", "jumpListCache", "thumbnails"].iter().map(|c| p.join(c))));
    }
    if !paths.is_empty() {
        out.push(Cand::remove(
            "Firefox cache",
            "Only the cache. Logins, history and bookmarks live elsewhere and stay. Close Firefox first.",
            Tier::Safe,
            Group::Browsers,
            paths,
        ));
    }
}

fn pretty_app(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "anthropicclaude" | "claude" => "Claude desktop".into(),
        "code" => "VS Code".into(),
        "code - insiders" => "VS Code Insiders".into(),
        "cursor" => "Cursor".into(),
        "discord" => "Discord".into(),
        "slack" => "Slack".into(),
        "postman" => "Postman".into(),
        "figma" => "Figma".into(),
        "githubdesktop" => "GitHub Desktop".into(),
        "microsoft teams" | "teams" => "Microsoft Teams".into(),
        "whatsapp" => "WhatsApp".into(),
        "notion" => "Notion".into(),
        "obsidian" => "Obsidian".into(),
        "zoom" => "Zoom".into(),
        _ => name.to_string(),
    }
}

fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(0))
        .collect()
}

fn modified(p: &Path) -> SystemTime {
    fs::symlink_metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn app_leftovers(e: &Env, out: &mut Vec<Cand>) {
    // Squirrel-installed apps keep old `app-x.y.z` folders and update packages.
    for app in subdirs(&e.local) {
        let app_name = pretty_app(&name_of(&app));
        let mut versions: Vec<PathBuf> = subdirs(&app).into_iter().filter(|p| name_of(p).starts_with("app-")).collect();
        if versions.len() >= 2 {
            versions.sort_by_key(|p| version_key(&name_of(p)));
            let newest = name_of(versions.last().unwrap());
            for old in &versions[..versions.len() - 1] {
                out.push(Cand::remove(
                    format!("Old {app_name} version ({})", name_of(old)),
                    format!("Left behind after {app_name} updated to {newest}. The app only runs the newest one."),
                    Tier::Safe,
                    Group::AppLeftovers,
                    vec![old.clone()],
                ));
            }
        }
        let pkgs = app.join("packages");
        if is_dir(&pkgs) && !versions.is_empty() {
            let nupkgs: Vec<PathBuf> = fs::read_dir(&pkgs)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().map(|x| x.eq_ignore_ascii_case("nupkg")).unwrap_or(false))
                        .collect()
                })
                .unwrap_or_default();
            if !nupkgs.is_empty() {
                out.push(Cand::remove(
                    format!("{app_name} update packages"),
                    "Installers downloaded for past updates. The installed app doesn't need them.",
                    Tier::Safe,
                    Group::AppLeftovers,
                    nupkgs,
                ));
            }
        }
        // electron-updater download folders.
        let n = name_of(&app);
        if n.to_ascii_lowercase().ends_with("-updater") {
            let base = pretty_app(n.trim_end_matches("-updater").trim_end_matches("-Updater"));
            out.push(Cand::contents(
                format!("Update downloads: {base}"),
                "Installers for updates that were already applied.",
                Tier::Safe,
                Group::AppLeftovers,
                app.clone(),
                None,
                Vec::new(),
            ));
        }
    }

    // Electron app caches in Roaming and Local.
    let cache_dirs = [
        "Cache", "Code Cache", "GPUCache", "DawnCache", "DawnGraphiteCache", "DawnWebGPUCache", "Crashpad",
        "CachedData", "CachedExtensionVSIXs", "ShaderCache", "GrShaderCache",
    ];
    let skip = ["google", "microsoft", "mozilla", "bravesoftware", "packages", "temp", "programs"];
    for base in [&e.roaming, &e.local] {
        for app in subdirs(base) {
            let n = name_of(&app);
            if skip.contains(&n.to_ascii_lowercase().as_str()) {
                continue;
            }
            if !(win::exists(&app.join("GPUCache")) || win::exists(&app.join("Code Cache"))) {
                continue;
            }
            let mut paths = existing(cache_dirs.iter().map(|c| app.join(c)));
            for part in subdirs(&app.join("Partitions")) {
                paths.extend(existing(["Cache", "Code Cache", "GPUCache"].iter().map(|c| part.join(c))));
            }
            if !paths.is_empty() {
                let pretty = pretty_app(&n);
                out.push(Cand::remove(
                    format!("{pretty} app cache"),
                    format!("Electron cache, rebuilt automatically. Close {pretty} first for a full clean."),
                    Tier::Safe,
                    Group::AppLeftovers,
                    paths,
                ));
            }
        }
    }

    // Old Android Studio caches: keep the newest.
    let mut studios: Vec<PathBuf> = subdirs(&e.local.join("Google"))
        .into_iter()
        .filter(|p| name_of(p).starts_with("AndroidStudio"))
        .collect();
    if studios.len() >= 2 {
        studios.sort_by_key(|p| version_key(&name_of(p)));
        for old in &studios[..studios.len() - 1] {
            out.push(Cand::remove(
                format!("Old Android Studio cache ({})", name_of(old)),
                "Cache from an Android Studio version you've since upgraded from.",
                Tier::Safe,
                Group::AppLeftovers,
                vec![old.clone()],
            ));
        }
    }

    // Old Codex CLI releases: keep the newest.
    let mut rel = subdirs(&e.user.join(".codex").join("packages").join("standalone").join("releases"));
    if rel.len() >= 2 {
        rel.sort_by_key(|p| std::cmp::Reverse(modified(p)));
        for old in &rel[1..] {
            out.push(Cand::remove(
                format!("Old Codex CLI release ({})", name_of(old)),
                "A previous Codex version kept after an update.",
                Tier::Safe,
                Group::AppLeftovers,
                vec![old.clone()],
            ));
        }
    }
}

fn system(out: &mut Vec<Cand>) {
    out.push(
        Cand::contents(
            "Windows Temp (older than 1 day)",
            "System temporary files. Files in use are skipped.",
            Tier::Safe,
            Group::System,
            PathBuf::from(r"C:\Windows\Temp"),
            Some(1),
            Vec::new(),
        )
        .admin(),
    );
    out.push(
        Cand::contents(
            "Windows Update downloads",
            "Update files that are already installed. Windows downloads again if it needs them.",
            Tier::Safe,
            Group::System,
            PathBuf::from(r"C:\Windows\SoftwareDistribution\Download"),
            None,
            Vec::new(),
        )
        .admin(),
    );
    let wer = existing([
        PathBuf::from(r"C:\ProgramData\Microsoft\Windows\WER\ReportArchive"),
        PathBuf::from(r"C:\ProgramData\Microsoft\Windows\WER\ReportQueue"),
    ]);
    if !wer.is_empty() {
        out.push(Cand::remove("Windows error reports", "Crash reports already handled by Windows.", Tier::Safe, Group::System, wer).admin());
    }
    let dumps = existing([
        PathBuf::from(r"C:\Windows\MEMORY.DMP"),
        PathBuf::from(r"C:\Windows\Minidump"),
        PathBuf::from(r"C:\Windows\LiveKernelReports"),
    ]);
    if !dumps.is_empty() {
        out.push(Cand::remove("System memory dumps", "Dumps from past blue screens and driver hangs.", Tier::Safe, Group::System, dumps).admin());
    }

    // hiberfil / pagefile: read from the directory listing (they are locked).
    let mut hiber = 0;
    let mut page = 0;
    let mut swap = 0;
    if let Ok(rd) = fs::read_dir(r"C:\") {
        for e in rd.flatten() {
            let n = name_of(&e.path()).to_ascii_lowercase();
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            match n.as_str() {
                "hiberfil.sys" => hiber = len,
                "pagefile.sys" => page = len,
                "swapfile.sys" => swap = len,
                _ => {}
            }
        }
    }
    if hiber > 0 {
        out.push(
            Cand::fixed(
                "Hibernation file",
                "Turns off Hibernate and Fast Startup. Sleep still works. Needs an admin prompt.",
                r"C:\hiberfil.sys",
                Tier::Review,
                Group::System,
                Action::DisableHibernation,
                hiber,
            )
            .admin(),
        );
    }
    if page + swap > 0 {
        out.push(Cand::fixed(
            "Page file (virtual memory)",
            "Windows needs this. Its size is set in System > Advanced > Performance > Virtual memory.",
            r"C:\pagefile.sys",
            Tier::Info,
            Group::WhereItGoes,
            Action::None,
            page + swap,
        ));
    }
    if win::exists(Path::new(r"C:\Windows.old")) {
        out.push(Cand::remove(
            "Previous Windows install (Windows.old)",
            "Remove it with Disk Cleanup > Clean up system files > Previous Windows installation(s).",
            Tier::Info,
            Group::WhereItGoes,
            vec![PathBuf::from(r"C:\Windows.old")],
        ));
    }
}

fn personal(e: &Env, out: &mut Vec<Cand>) {
    let dl = e.user.join("Downloads");
    let count = fs::read_dir(&dl).map(|r| r.count()).unwrap_or(0);
    if count > 0 {
        out.push(
            Cand::contents(
                format!("Downloads folder ({count} items)"),
                "Everything in Downloads. Look through it first if you're not sure.",
                Tier::Review,
                Group::Personal,
                dl,
                None,
                Vec::new(),
            )
            .personal(),
        );
    }
    out.push(Cand::fixed(
        "Recycle Bin",
        "Files you already deleted. Emptying it is permanent.",
        "All drives",
        Tier::Review,
        Group::Personal,
        Action::EmptyRecycleBin,
        win::recycle_bin_size(),
    ));
}

// ---------------------------------------------------------------- projects

pub fn project_roots(e: &Env) -> Vec<PathBuf> {
    ["work", "source", "projects", "dev", "code", "repos", "src", r"Documents\GitHub"]
        .iter()
        .map(|r| e.user.join(r))
        .filter(|p| is_dir(p))
        .collect()
}

fn git_tracked(dir: &Path, name: &str) -> bool {
    match win::hidden("git").arg("-C").arg(dir).args(["ls-files", "--", name]).output() {
        Ok(o) => o.status.success() && !o.stdout.is_empty(),
        Err(_) => false,
    }
}

fn projects(e: &Env, out: &mut Vec<Cand>) {
    for root in project_roots(e) {
        walk_projects(&root, &root, 0, out);
    }
}

fn walk_projects(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Cand>) {
    if depth > 6 {
        return;
    }
    let has = |f: &str| dir.join(f).exists();
    let project = dir.strip_prefix(root).map(|p| p.display().to_string()).unwrap_or_default();
    let project = if project.is_empty() { name_of(dir) } else { project };
    for p in subdirs(dir) {
        let name = name_of(&p);
        let push = |out: &mut Vec<Cand>, label: &str, detail: &str, tier: Tier| {
            out.push(Cand::remove(format!("{label} · {project}"), detail, tier, Group::Projects, vec![p.clone()]));
        };
        match name.as_str() {
            ".git" => {}
            "node_modules" => {
                if has("package.json") {
                    push(out, "Node packages", "Installed packages. Restore with `npm install` / `pnpm install`. Good to clear for projects you aren't working on.", Tier::Review);
                }
            }
            "target" if has("Cargo.toml") => {
                push(out, "Rust build output", "Compiler output. Rebuilt by the next `cargo build`, which takes a while.", Tier::Safe)
            }
            "target" if has("pom.xml") => push(out, "Maven build output", "Rebuilt by `mvn package`.", Tier::Safe),
            ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular" | ".expo" => {
                push(out, "Web build cache", "Framework build cache. Rebuilt on the next dev or build run.", Tier::Safe)
            }
            "dist" | "out" | "build" | "release" | "dist-electron" => {
                let is_project = ["package.json", "build.gradle", "build.gradle.kts", "pyproject.toml", "setup.py", "CMakeLists.txt"]
                    .iter()
                    .any(|f| has(f));
                if is_project && !git_tracked(dir, &name) {
                    push(out, &format!("Build output ({name})"), "Build output that git doesn't track. Rebuilt by the project's build command.", Tier::Safe);
                } else if !is_project {
                    walk_projects(root, &p, depth + 1, out);
                }
            }
            ".venv" | "venv" | "env" if p.join("pyvenv.cfg").exists() => {
                push(out, "Python virtual env", "Recreate it and reinstall the packages if you need it again.", Tier::Review)
            }
            ".gradle" if has("build.gradle") || has("build.gradle.kts") || has("settings.gradle") || has("settings.gradle.kts") => {
                push(out, "Gradle project cache", "Rebuilt on the next Gradle build.", Tier::Safe)
            }
            n if n.starts_with('.') => {}
            _ => walk_projects(root, &p, depth + 1, out),
        }
    }
}

// ---------------------------------------------------------------- breakdown

fn describe_folder(area: &str, name: &str) -> (String, String) {
    let n = name.to_ascii_lowercase();
    let known: Option<&str> = match (area, n.as_str()) {
        ("user", "work") => Some("Your projects"),
        ("user", "downloads") => Some("Downloads"),
        ("user", "documents") => Some("Documents"),
        ("user", "desktop") => Some("Desktop"),
        ("user", "pictures") => Some("Pictures"),
        ("user", "videos") => Some("Videos"),
        ("user", "music") => Some("Music"),
        ("user", "onedrive") => Some("OneDrive (synced files)"),
        ("user", ".cargo") => Some("Rust: cargo tools and crate cache"),
        ("user", ".rustup") => Some("Rust toolchains"),
        ("user", ".gradle") => Some("Gradle (Android/Java builds)"),
        ("user", ".android") => Some("Android emulator images and settings"),
        ("user", ".nuget") => Some(".NET packages"),
        ("user", ".vscode") => Some("VS Code extensions"),
        ("user", ".cursor") => Some("Cursor extensions"),
        ("user", ".claude") => Some("Claude Code data (sessions, worktrees)"),
        ("user", ".cache") => Some("Tool caches"),
        ("user", ".ollama") => Some("Ollama models"),
        ("user", ".lmstudio") => Some("LM Studio models and runtime"),
        ("user", ".codex") => Some("Codex CLI"),
        ("user", ".docker") => Some("Docker settings"),
        ("user", ".conda") | ("user", "anaconda3") | ("user", "miniconda3") => Some("Conda Python environments"),
        ("local", "packages") => Some("Store apps and WSL disks"),
        ("local", "google") => Some("Chrome and other Google apps"),
        ("local", "microsoft") => Some("Microsoft apps (Edge, Outlook, Teams, OneDrive)"),
        ("local", "programs") => Some("Apps installed just for you (VS Code, Cursor and others)"),
        ("local", "docker") => Some("Docker Desktop (its WSL disks)"),
        ("local", "wsl") => Some("WSL disks"),
        ("local", "temp") => Some("Temp files"),
        ("local", "android") => Some("Android SDK"),
        ("local", "jetbrains") => Some("JetBrains IDE caches"),
        ("local", "anthropicclaude") => Some("Claude desktop app (installed versions)"),
        ("local", "mozilla") => Some("Firefox cache"),
        ("local", "bravesoftware") => Some("Brave browser"),
        ("local", "npm-cache") => Some("npm cache"),
        ("local", "pnpm") => Some("pnpm store"),
        ("local", "ms-playwright") => Some("Playwright test browsers"),
        ("local", "nvidia") => Some("NVIDIA caches"),
        ("local", "discord") => Some("Discord (installed versions)"),
        ("local", "postman") => Some("Postman (installed versions)"),
        ("local", "figma") => Some("Figma (installed versions)"),
        ("roaming", "mozilla") => Some("Firefox profiles (bookmarks, logins, site data)"),
        ("roaming", "code") => Some("VS Code data"),
        ("roaming", "cursor") => Some("Cursor data"),
        ("roaming", "claude") => Some("Claude desktop app data"),
        ("roaming", "npm") => Some("Global npm packages"),
        ("roaming", "python") => Some("pip --user packages"),
        ("roaming", "postman") => Some("Postman workspace data"),
        ("roaming", "figma") => Some("Figma app data"),
        ("roaming", "slack") => Some("Slack data"),
        ("roaming", "discord") => Some("Discord data"),
        ("roaming", "zoom") => Some("Zoom data"),
        ("roaming", "microsoft") => Some("Microsoft app data"),
        ("roaming", "nvm") => Some("Node versions installed by nvm"),
        ("roaming", "adobe") => Some("Adobe app data"),
        ("system", "program files") => Some("Installed programs"),
        ("system", "program files (x86)") => Some("Installed programs (32-bit)"),
        ("system", "programdata") => Some("Shared app data"),
        _ => None,
    };
    let prefix = match area {
        "local" => r"AppData\Local\",
        "roaming" => r"AppData\Roaming\",
        "locallow" => r"AppData\LocalLow\",
        _ => "",
    };
    if area == "drive" {
        return (format!(r"C:\{name}"), "Folder at the top of the C: drive, usually a tool installed there.".into());
    }
    match known {
        Some(k) => (k.to_string(), format!("{prefix}{name}")),
        None => (format!("{prefix}{name}"), "Not a known cache. Check what the folder belongs to before touching it.".into()),
    }
}

struct Walked {
    size: u64,
    large: Vec<(PathBuf, u64, SystemTime)>,
}

/// Size a tree and, optionally, collect large files outside .git and node_modules.
fn walk(root: &Path, collect_large: bool) -> Walked {
    let mut w = Walked { size: 0, large: Vec::new() };
    let mut stack = vec![(root.to_path_buf(), collect_large)];
    while let Some((dir, collect)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.file_attributes() & 0x400 != 0 {
                continue;
            }
            if md.is_dir() {
                let n = e.file_name();
                let n = n.to_string_lossy();
                let keep = collect && n != ".git" && n != "node_modules" && !n.starts_with('.');
                stack.push((e.path(), keep));
            } else {
                let len = md.len();
                w.size += len;
                if collect && len >= LARGE_FILE {
                    w.large.push((e.path(), len, md.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
    }
    w
}

fn large_file_kind(p: &Path) -> &'static str {
    let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "gguf" | "safetensors" | "nano" | "onnx" | "pt" | "pth" | "ckpt" | "bin" | "mlmodel" => "AI model file",
        "iso" | "img" | "vhd" | "vmdk" | "qcow2" | "wim" => "Disk image",
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "xz" | "zst" => "Archive",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => "Video",
        "exe" | "msi" | "msix" | "appx" | "dmg" | "apk" | "aab" => "Installer or app package",
        "pst" | "ost" => "Outlook mail data",
        "dmp" => "Crash dump",
        "log" => "Log file",
        "db" | "sqlite" => "Database",
        _ => "Large file",
    }
}

fn age(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => {
            let days = d.as_secs() / 86_400;
            match days {
                0 => "changed today".into(),
                1 => "changed yesterday".into(),
                n => format!("changed {n} days ago"),
            }
        }
        Err(_) => String::new(),
    }
}

/// Measure the big folders and find large files. Returns (info rows, large files, measured total).
fn breakdown(e: &Env, tx: &Sender<Msg>) -> (Vec<Cand>, Vec<(PathBuf, u64, SystemTime)>, u64) {
    let mut roots: Vec<(String, PathBuf, bool)> = Vec::new();
    let mut loose_files = 0u64;
    if let Ok(rd) = fs::read_dir(&e.user) {
        for d in rd.flatten() {
            let Ok(md) = d.metadata() else { continue };
            if md.file_attributes() & 0x400 != 0 {
                continue;
            }
            let n = d.file_name().to_string_lossy().into_owned();
            if md.is_dir() {
                if n.eq_ignore_ascii_case("AppData") {
                    continue;
                }
                let user_folder = !n.starts_with('.');
                roots.push(("user".into(), d.path(), user_folder));
            } else {
                loose_files += md.len();
            }
        }
    }
    for (area, base) in [("local", &e.local), ("roaming", &e.roaming), ("locallow", &e.locallow)] {
        for d in subdirs(base) {
            roots.push((area.into(), d, false));
        }
        if let Ok(rd) = fs::read_dir(base) {
            loose_files += rd.flatten().filter_map(|d| d.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum::<u64>();
        }
    }
    for p in [r"C:\Program Files", r"C:\Program Files (x86)", r"C:\ProgramData"] {
        roots.push(("system".into(), PathBuf::from(p), false));
    }
    // Other folders at the top of C: (tools installed there, old installs and so on).
    let skip_root = [
        "windows", "users", "program files", "program files (x86)", "programdata", "$recycle.bin",
        "system volume information", "recovery", "$winreagent", "config.msi", "documents and settings",
        "msocache", "perflogs", "$windows.~bt", "$windows.~ws", "windows.old", "$sysreset", "onedrivetemp",
    ];
    for d in subdirs(Path::new(r"C:\")) {
        if !skip_root.contains(&name_of(&d).to_ascii_lowercase().as_str()) {
            roots.push(("drive".into(), d, false));
        }
    }

    let done = AtomicUsize::new(0);
    let total = roots.len();
    let results: Vec<(String, PathBuf, Walked)> = roots
        .into_par_iter()
        .map(|(area, p, collect)| {
            let w = walk(&p, collect);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 10 == 0 || n == total {
                let _ = tx.send(Msg::Status(format!("Measuring folders… {n} of {total}")));
            }
            (area, p, w)
        })
        .collect();

    let mut rows = Vec::new();
    let mut large = Vec::new();
    let mut measured = loose_files;
    for (area, p, w) in results {
        measured += w.size;
        large.extend(w.large);
        let (label, detail) = describe_folder(&area, &name_of(&p));
        let loc = p.display().to_string();
        rows.push(Cand::fixed(label, detail, loc, Tier::Info, Group::WhereItGoes, Action::None, w.size));
    }
    (rows, large, measured)
}

// ---------------------------------------------------------------- driver

pub fn scan(tx: Sender<Msg>, include_wsl: bool) {
    let e = Env::new();
    let _ = tx.send(Msg::Status("Listing known caches…".into()));
    let mut cands = Vec::new();
    known_caches(&e, &mut cands);
    browsers(&e, &mut cands);
    app_leftovers(&e, &mut cands);
    system(&mut cands);
    personal(&e, &mut cands);
    let _ = tx.send(Msg::Status("Looking for build folders in your projects…".into()));
    projects(&e, &mut cands);

    let wsl = include_wsl.then(|| {
        let tx = tx.clone();
        std::thread::spawn(move || crate::wsl::scan(&tx))
    });

    let covered: Vec<PathBuf> = cands.iter().flat_map(|c| c.paths()).collect();
    let _ = tx.send(Msg::Status(format!("Measuring {} places…", cands.len())));
    let (_, (rows, large, measured)) = rayon::join(
        || {
            cands.into_par_iter().for_each_with(tx.clone(), |tx, c| {
                let size = c.measure();
                if size >= c.min_size() {
                    let _ = tx.send(Msg::Item(c.into_item(size)));
                }
            })
        },
        || breakdown(&e, &tx),
    );

    for c in rows {
        if let Measure::Fixed(s) = c.measure {
            if s >= MIN_INFO {
                let _ = tx.send(Msg::Item(c.into_item(s)));
            }
        }
    }

    for (p, len, t) in large {
        if covered.iter().any(|c| fsx::is_within(&p, c)) {
            continue;
        }
        let kind = large_file_kind(&p);
        let parent = p
            .parent()
            .and_then(|d| d.strip_prefix(&e.user).ok())
            .map(|d| format!("~\\{}", d.display()))
            .unwrap_or_default();
        let c = Cand::remove(
            format!("{kind}: {}", name_of(&p)),
            format!("In {parent}, {}.", age(t)),
            Tier::Review,
            Group::Personal,
            vec![p.clone()],
        )
        .personal();
        let _ = tx.send(Msg::Item(c.into_item(len)));
    }

    // Whatever the measured folders don't explain is Windows itself and other system files.
    let (free, total) = win::disk_space("C:\\");
    let used = total.saturating_sub(free);
    let sys_files: u64 = fs::read_dir(r"C:\")
        .map(|rd| {
            rd.flatten()
                .filter(|d| {
                    let n = name_of(&d.path()).to_ascii_lowercase();
                    n == "hiberfil.sys" || n == "pagefile.sys" || n == "swapfile.sys"
                })
                .filter_map(|d| d.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    let other = used
        .saturating_sub(measured)
        .saturating_sub(sys_files)
        .saturating_sub(win::recycle_bin_size());
    if other > GB {
        let c = Cand::fixed(
            r"Windows itself (C:\Windows) and other system files",
            "Windows, restore points, other user accounts and system folders. Not measured folder by folder.",
            r"C:\",
            Tier::Info,
            Group::WhereItGoes,
            Action::None,
            other,
        );
        let _ = tx.send(Msg::Item(c.into_item(other)));
    }

    if let Some(h) = wsl {
        let _ = tx.send(Msg::Status("Waiting for the WSL scan…".into()));
        let _ = h.join();
    }
    let _ = tx.send(Msg::ScanDone);
}
