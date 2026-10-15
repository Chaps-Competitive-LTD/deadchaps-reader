//! Linux: the right to read the game, given once.
//!
//! Most systems (Yama `ptrace_scope` 1) let a program open another's memory
//! only if it started it, and Steam starts Deadlock. The `deadchaps-memgrant`
//! helper (reader/memgrant) is the one piece given that right. This module
//! finds it, says whether it holds the right, and gives it the right with the
//! player's password. Nothing here opens or reads the game.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The helper's file name.
pub const HELPER: &str = "deadchaps-memgrant";
/// The environment variable in which the Companion names its own copy of the
/// helper for the reader it starts.
pub const ENV_HELPER: &str = "DEADCHAPS_MEMGRANT";

/// Whether the game will open for us once it runs: the system lets a
/// program read any of its user's processes (Yama `ptrace_scope` 0), or a
/// helper holds the right. It answers without the game running, so the
/// Companion can ask for the right before the game starts.
pub fn granted() -> bool {
    ptrace_scope() == 0 || helpers().iter().any(|h| has_capability(h))
}

/// Yama's `ptrace_scope`; 0 without Yama, which asks nothing extra.
pub fn ptrace_scope() -> u8 {
    fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether `path` carries file capabilities (the right `setcap` gives it).
fn has_capability(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path and attribute name; a null buffer of
    // size 0 asks only for the value's length.
    let len = unsafe { libc::getxattr(c_path.as_ptr(), c"security.capability".as_ptr(), std::ptr::null_mut(), 0) };
    len > 0
}

/// Where the helper may be, the granted copy first. The .deb installs it in
/// /usr/bin with the right. An AppImage carries one without it, and the
/// player's grant copies it out and gives the copy the right. The reader
/// runs from the Companion's data folder, so the Companion names its own
/// copy in [`ENV_HELPER`] when it starts the reader. The folder beside the
/// running program is also searched, for when the Companion itself asks.
pub fn helpers() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(granted) = granted_path() {
        out.push(granted);
    }
    out.push(PathBuf::from("/usr/bin").join(HELPER));
    if let Some(named) = std::env::var_os(ENV_HELPER).map(PathBuf::from).filter(|p| p.is_absolute()) {
        out.push(named);
    }
    if let Some(beside) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(HELPER))) {
        out.push(beside);
    }
    let mut seen = Vec::new();
    out.into_iter().filter(|p| p.is_file() && !seen.contains(p) && { seen.push(p.clone()); true }).collect()
}

/// Where the granted copy lives: the Companion's data folder.
pub fn granted_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("gg.deadchaps.companion").join("memgrant").join(HELPER))
}

/// Gives the helper the right to open the game's memory, once, with the
/// player's password through polkit. An AppImage's helper is copied out to
/// the data folder first, as a file inside the AppImage cannot hold the
/// right. Blocks until the password prompt is answered.
pub fn grant_access() -> Result<(), String> {
    let source = helpers()
        .into_iter()
        .find(|p| Some(p) != granted_path().as_ref())
        .ok_or("the helper is not beside the app")?;
    let target = granted_path().ok_or("no data folder")?;
    let dir = target.parent().ok_or("no data folder")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // a fresh copy each time, since writing a file clears any capability it held
    let _ = fs::remove_file(&target);
    fs::copy(&source, &target).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let status = Command::new("pkexec")
        .arg("setcap")
        .arg("cap_sys_ptrace=ep")
        .arg(&target)
        .status()
        .map_err(|e| format!("pkexec: {e}"))?;
    if status.success() {
        tracing::info!("Game memory: access granted to {}", target.display());
        Ok(())
    } else {
        Err(format!("the grant was not given (pkexec exited {})", status.code().unwrap_or(-1)))
    }
}
