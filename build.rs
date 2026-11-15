//! Embeds the app icon and version info into space-sweep.exe using the Windows SDK's rc.exe.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = env::var("RC") {
        return Some(PathBuf::from(p));
    }
    let base = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("x64").join("rc.exe").exists())
        .collect();
    versions.sort();
    versions.pop().map(|v| v.join("x64").join("rc.exe"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico = manifest.join("assets").join("icon.ico");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let v: Vec<u32> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (v[0], v.get(1).copied().unwrap_or(0), v.get(2).copied().unwrap_or(0));
    let rc = format!(
        r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Space Sweep"
      VALUE "ProductName", "Space Sweep"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "space-sweep.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        ico = ico.display().to_string().replace('\\', "\\\\")
    );
    let rc_path = out.join("app.rc");
    let res_path = out.join("app.res");
    std::fs::write(&rc_path, rc).unwrap();
    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=rc.exe not found (install the Windows SDK); the exe gets no icon");
        return;
    };
    let ok = Command::new(rc_exe)
        .args(["/nologo", "/fo"])
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res_path.display());
    } else {
        println!("cargo:warning=rc.exe failed; the exe gets no icon");
    }
}
