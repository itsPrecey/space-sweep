//! What the scanner finds and what the cleaner acts on.

use std::path::PathBuf;

/// How safe an item is to delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Caches and build output. Rebuilt or re-downloaded automatically.
    Safe,
    /// Won't come back on its own. Your decision.
    Review,
    /// Shown so you know where space goes, but not deleted by this app.
    Info,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Safe => "SAFE",
            Tier::Review => "YOUR CALL",
            Tier::Info => "INFO",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Caches,
    Browsers,
    AppLeftovers,
    Projects,
    Wsl,
    Personal,
    System,
    WhereItGoes,
}

impl Group {
    pub const ALL: [Group; 8] = [
        Group::Caches,
        Group::Browsers,
        Group::AppLeftovers,
        Group::Projects,
        Group::Wsl,
        Group::Personal,
        Group::System,
        Group::WhereItGoes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Caches => "Caches and temp files",
            Group::Browsers => "Browser caches",
            Group::AppLeftovers => "App leftovers (old versions, updaters, app caches)",
            Group::Projects => "Project build output",
            Group::Wsl => "Inside WSL (Linux)",
            Group::Personal => "Your files",
            Group::System => "System",
            Group::WhereItGoes => "Where the rest of the space goes (not deletable here)",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Group::Caches => "Tool and app caches. They refill on their own when needed.",
            Group::Browsers => "Only the cache folders. Logins, history and bookmarks are untouched. Close the browser first.",
            Group::AppLeftovers => "Files left behind after apps updated, plus Electron app caches.",
            Group::Projects => "Build folders inside your projects. `target`/`dist` rebuild; `node_modules` and venvs need a reinstall.",
            Group::Wsl => "Frees space inside Linux. Windows only gets it back after the WSL disk is compacted (checkbox at the bottom).",
            Group::Personal => "Things only you can judge. By default these go to the Recycle Bin.",
            Group::System => "Windows files. Some need an admin prompt.",
            Group::WhereItGoes => "The big folders, labelled, so you can see what is using the drive.",
        }
    }
}

/// What deleting an item actually does.
#[derive(Clone, Debug)]
pub enum Action {
    /// Nothing; info row.
    None,
    /// Remove these files or folders entirely.
    Remove(Vec<PathBuf>),
    /// Empty a folder but keep it. Optionally only entries older than N days.
    Contents { dir: PathBuf, older_than_days: Option<u64>, keep: Vec<PathBuf> },
    /// Empty the Windows Recycle Bin.
    EmptyRecycleBin,
    /// `rm -rf` paths inside a WSL distro as the normal user.
    WslRemove { distro: String, paths: Vec<String> },
    /// Run a shell snippet inside a WSL distro as root.
    WslRoot { distro: String, script: String },
    /// `powercfg /h off` (admin prompt).
    DisableHibernation,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: usize,
    pub label: String,
    /// What it is and what happens if it goes.
    pub detail: String,
    /// Location shown under the label.
    pub location: String,
    pub size: u64,
    pub tier: Tier,
    pub group: Group,
    pub action: Action,
    /// Your own files: sent to the Recycle Bin when that option is on.
    pub personal: bool,
    pub needs_admin: bool,
    pub selected: bool,
    /// Result after a delete run.
    pub status: Option<String>,
    pub gone: bool,
}

impl Item {
    pub fn deletable(&self) -> bool {
        !matches!(self.action, Action::None) && self.tier != Tier::Info && !self.gone
    }
}

pub fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

/// Messages from worker threads to the UI.
pub enum Msg {
    Status(String),
    Item(Item),
    ScanDone,
    Log(String),
    ItemResult { id: usize, status: String, gone: bool, freed: u64 },
    WslInfo { reclaimable_hint: String },
    WorkDone,
}
