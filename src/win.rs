//! Small Win32 helpers: free space, Recycle Bin, elevation, hidden child processes.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{
    SHEmptyRecycleBinW, SHFileOperationW, SHQueryRecycleBinW, ShellExecuteExW, SHELLEXECUTEINFOW,
    SHFILEOPSTRUCTW, SHQUERYRBINFO,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_CANCELLED: u32 = 1223;

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// (free, total) bytes for the drive holding `path`.
pub fn disk_space(path: &str) -> (u64, u64) {
    let w = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free);
    }
    (avail, total)
}

pub fn recycle_bin_size() -> u64 {
    unsafe {
        let mut info: SHQUERYRBINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHQUERYRBINFO>() as u32;
        if SHQueryRecycleBinW(null(), &mut info) == 0 {
            info.i64Size.max(0) as u64
        } else {
            0
        }
    }
}

pub fn empty_recycle_bin() -> Result<(), String> {
    // SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND
    let hr = unsafe { SHEmptyRecycleBinW(null_mut(), null(), 0x1 | 0x2 | 0x4) };
    // E_UNEXPECTED (0x8000FFFF) is returned when the bin is already empty.
    if hr == 0 || hr as u32 == 0x8000_FFFF {
        Ok(())
    } else {
        Err(format!("Windows returned error 0x{:08X}", hr as u32))
    }
}

/// Move files or folders to the Recycle Bin without any dialogs.
pub fn send_to_recycle_bin(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    // Double-NUL-terminated list of NUL-separated paths.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.as_os_str().encode_wide());
        from.push(0);
    }
    from.push(0);
    // FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING
    let flags: u16 = 0x0040 | 0x0010 | 0x0004 | 0x0400 | 0x4000;
    unsafe {
        let mut op: SHFILEOPSTRUCTW = std::mem::zeroed();
        op.wFunc = 3; // FO_DELETE
        op.pFrom = from.as_ptr();
        op.fFlags = flags as _;
        let rc = SHFileOperationW(&mut op);
        if rc != 0 {
            return Err(format!("Recycle Bin move failed (code {rc})"));
        }
        if op.fAnyOperationsAborted != 0 {
            return Err("Recycle Bin move was cancelled".into());
        }
    }
    Ok(())
}

/// Run a program through a UAC prompt, hidden, and wait for it. Returns its exit code.
pub fn run_elevated(program: &str, params: &str) -> Result<u32, String> {
    let verb = wide("runas");
    let file = wide(program);
    let args = wide(params);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = 0x0000_0040; // SEE_MASK_NOCLOSEPROCESS
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = args.as_ptr();
        info.nShow = 0; // SW_HIDE
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            if err == ERROR_CANCELLED {
                return Err("the admin prompt was declined".into());
            }
            return Err(format!("could not start an admin prompt (error {err})"));
        }
        if info.hProcess.is_null() {
            return Ok(0);
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(code)
    }
}

/// A `Command` that never flashes a console window.
pub fn hidden(program: &str) -> Command {
    let mut c = Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c.env("WSL_UTF8", "1");
    c
}

/// Decode child output that may be UTF-16LE (wsl.exe) or UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    let nuls = bytes.iter().filter(|b| **b == 0).count();
    if bytes.len() >= 2 && nuls * 3 > bytes.len() {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&u16s).replace('\u{feff}', "")
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Every running process as (pid, parent pid, exe name).
pub fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
}

pub fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}
