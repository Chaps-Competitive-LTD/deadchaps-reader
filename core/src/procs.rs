//! The running programs, by name: what the Companion and the reader need to
//! know whether Deadlock or another program they watch for is running, and
//! nothing more.
//!
//! Windows lists them from a Toolhelp snapshot, which gives each process's
//! id, parent and program name without opening any of them. A program's full
//! path is asked with a handle that can only query (`exe_path`). Linux reads
//! the same from `/proc`. Nothing here can read another program's memory.
//! That is why this module exists instead of a general process library:
//! those carry, in their Windows builds, a reader for other processes'
//! command lines, which puts `ReadProcessMemory` among the imports of any
//! program that links them.

use std::path::PathBuf;

/// One running program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub parent: Option<u32>,
    /// The program's file name as the system lists it (`deadlock.exe`). On
    /// Linux this is the kernel's name for the process, which is its main
    /// thread's and can differ (see [`crate::game_process`]).
    pub name: String,
    /// Linux: the file name of the program the process was started as, taken
    /// from the first argument, by Linux or Windows path. None on Windows,
    /// where `name` is already that.
    pub program: Option<String>,
}

/// Every running program.
#[cfg(windows)]
pub fn list() -> Vec<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    // SAFETY: a snapshot of the process list; the handle is closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return out;
    }
    // SAFETY: a zeroed entry with its size set is what Process32FirstW takes.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `snap` is a valid snapshot and `entry` a valid, sized entry.
    let mut more = unsafe { Process32FirstW(snap, &raw mut entry) } != 0;
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        out.push(Proc {
            pid: entry.th32ProcessID,
            parent: Some(entry.th32ParentProcessID).filter(|p| *p != 0),
            name: String::from_utf16_lossy(&entry.szExeFile[..len]),
            program: None,
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &raw mut entry) } != 0;
    }
    // SAFETY: closing the snapshot we opened.
    unsafe { CloseHandle(snap) };
    out
}

/// Every running program. The top level of `/proc` lists processes only, a
/// process's threads sitting under its own `task/`, so no thread is ever
/// taken for a process.
#[cfg(target_os = "linux")]
pub fn list() -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (name, parent) = parse_stat(&stat)?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            Some(Proc { pid, parent, name, program: first_program(&cmdline) })
        })
        .collect()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn list() -> Vec<Proc> {
    Vec::new()
}

/// The name and parent from `/proc/<pid>/stat`: "pid (name) state ppid ...",
/// where the name may hold spaces and brackets, so it runs to the LAST ')'.
pub fn parse_stat(stat: &str) -> Option<(String, Option<u32>)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_owned();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let parent = rest.next()?.parse::<u32>().ok().filter(|p| *p != 0);
    Some((name, parent))
}

/// The file name of a command line's first argument (NUL separated), by
/// Linux or Windows path.
pub fn first_program(cmdline: &[u8]) -> Option<String> {
    let first = cmdline.split(|b| *b == 0).next().filter(|f| !f.is_empty())?;
    let first = String::from_utf8_lossy(first);
    first.rsplit(['/', '\\']).next().map(str::to_owned).filter(|s| !s.is_empty())
}

/// A program's full path, asked with a handle that can only query.
#[cfg(windows)]
pub fn exe_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    // SAFETY: a query-only handle, closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0_u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` u16s; the call writes at most that many.
    let ok = unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &raw mut len) } != 0;
    // SAFETY: closing the handle we opened.
    unsafe { CloseHandle(handle) };
    ok.then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
}

#[cfg(target_os = "linux")]
pub fn exe_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn exe_path(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_program_is_in_the_list_with_its_own_name() {
        let me = std::process::id();
        let found = list().into_iter().find(|p| p.pid == me).expect("the test is running");
        let exe = std::env::current_exe().unwrap();
        let file = exe.file_name().unwrap().to_string_lossy();
        // Linux keeps 15 bytes of a name
        assert!(file.starts_with(&found.name[..found.name.len().min(15)]), "{found:?} vs {file}");
        assert_eq!(exe_path(me).map(|p| p.file_name().unwrap().to_owned()), Some(exe.file_name().unwrap().to_owned()));
    }

    #[test]
    fn a_stat_line_with_spaces_and_brackets_in_the_name() {
        assert_eq!(parse_stat("1234 (deadlock.exe) S 1 1234"), Some(("deadlock.exe".to_owned(), Some(1))));
        assert_eq!(parse_stat("99 (Web Content (x)) R 42 99"), Some(("Web Content (x)".to_owned(), Some(42))));
        assert_eq!(parse_stat("1 (init) S 0 1"), Some(("init".to_owned(), None)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn the_first_argument_names_the_program() {
        assert_eq!(first_program(b"Z:\\x\\Deadlock\\game\\bin\\win64\\deadlock.exe\0-novid\0").as_deref(), Some("deadlock.exe"));
        assert_eq!(first_program(b"/usr/bin/python3\0x.py\0").as_deref(), Some("python3"));
        assert_eq!(first_program(b""), None);
    }
}
