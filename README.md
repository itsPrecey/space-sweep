# Space Sweep

A Windows app (Rust + egui) that finds what is filling the C: drive, labels each item in plain words, and deletes the ones you tick in one click. A checkbox compacts the WSL disk after the deletion finishes.

Install with `dist\SpaceSweep-Setup-<version>.exe` (per-user by default, no admin prompt; Start Menu shortcut, optional desktop shortcut, uninstall from Settings > Apps), then open **Space Sweep**. It scans on start (about 2 minutes; rows appear as they are measured).

## What it finds

Each row shows what the item is, where it is, its size and a badge:

- **SAFE**: caches and build output that rebuild or re-download on their own.
- **YOUR CALL**: things that won't come back on their own (node_modules, models, Downloads, Recycle Bin, hibernation).
- **INFO**: shown so you can see where the space goes. Never deleted.

Groups:

| Group | Examples |
|---|---|
| Caches and temp files | npm/pnpm/yarn/pip/uv/cargo/gradle caches, Temp (older than 1 day), TypeScript cache, shader caches, crash dumps, `~\.cache` tools, models (YOUR CALL) |
| Browser caches | Chrome/Edge/Brave `Cache`, `Code Cache`, `GPUCache` per profile; Firefox `cache2`. Logins and history stay. |
| App leftovers | Old Squirrel `app-x.y.z` versions, `*.nupkg` update packages, `*-updater` downloads, Electron app caches, old Android Studio and Codex releases |
| Project build output | Under `~\work`, `~\source` and similar folders: Rust `target`, `.next`/`.turbo`, untracked `dist`/`build`/`out`/`release`, `node_modules` and venvs (YOUR CALL) |
| Inside WSL (Linux) | `~/.cache/*`, npm/npx/pnpm/cargo caches, old VS Code/Cursor/Element server builds, project `node_modules`/`target`/`.next`, apt cache, journald logs, snap cache |
| Your files | Downloads, Recycle Bin, files over 500 MB in your folders (models, videos, archives, installers) |
| System | Windows Temp, Windows Update downloads, error reports, memory dumps, hibernation file |
| Where the rest goes | Every big folder in your profile, AppData, Program Files and the C: root, labelled (INFO) |

## Deleting

- Tick rows (or **Select all SAFE**), then press **Delete**. A confirmation shows the split between permanent, Recycle Bin and inside-WSL deletions. Turn off **Ask before deleting** for true one-click.
- Files in use are skipped and reported. Junctions and symlinks are removed as links; their targets are never touched.
- **Send your own files to the Recycle Bin** (on by default) applies to YOUR CALL personal files. Their space comes back once you empty the bin.
- The Recycle Bin is emptied before anything new is sent to it.

## WSL compaction

With **Compact WSL disk when deletion finishes** ticked, the app:

1. runs `fstrim -v /` as root in each distro so freed blocks are marked free,
2. names the programs holding WSL open (for example the Claude desktop app, which keeps a bridge into WSL and reconnects within seconds of a shutdown),
3. starts an elevated copy of itself (`--compact-helper`, one admin prompt) that launches `diskpart` with the disk already selected and waits,
4. only then runs `wsl --shutdown`; the moment WSL is down it signals the helper, which immediately runs `attach vdisk readonly` → `compact vdisk` → `detach vdisk`, so nothing can restart WSL in between,
5. retries up to 3 times if the disk is still in use, and reports each disk's size before and after.

While a disk is being compacted, apps that use WSL can't reach it; they reconnect afterwards. Compaction can't return space trapped by fragmentation; the only fix for that is `wsl --export` / `--import`.

## Build

```bash
cargo build --release
cargo test --release -- --include-ignored   # the ignored tests need a WSL distro
```

Installer (needs [Inno Setup 6](https://jrsoftware.org/isinfo.php)): `powershell -File installer\build.ps1` builds the exe and writes `dist\SpaceSweep-Setup-<version>.exe`.

## Logo and icon

`assets/logo.svg` is the mark traced with LineMorph from the design clip (`assets/logo-source.png`, background flattened to black): `linemorph trace logo-source.png --smoothing 3 --tolerance 0.4 --min-region 4 --min-area 1` scores 95.9% against the source. `python assets/make_icons.py <linemorph.exe>` puts it on a dark rounded tile (`icon.svg`) and renders `icon.ico` (16 to 256 px), `icon-256.png` and the window icon. `build.rs` embeds the icon and version info into the exe with the Windows SDK's `rc.exe`.

Flags: `--report out.tsv` writes a headless scan to a file; `--no-wsl` skips the Linux scan; `--screenshot out.bmp` captures the window after the scan and exits.

## License

MIT. See [LICENSE](LICENSE).
