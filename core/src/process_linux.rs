//! The running game's memory on Linux, opened read-only.
//!
//! Under Proton the game is the same Windows program, its address space
//! mapped straight into a Linux process, so its heap and every pointer in it
//! read here exactly as they do on Windows. Only opening it differs: the
//! regions come from `/proc/<pid>/maps` and the bytes from `/proc/<pid>/mem`.
//!
//! Most systems (Yama `ptrace_scope` 1) let a program open another's memory
//! only if it started it. Where `/proc/<pid>/mem` will not open for us, the
//! `deadchaps-memgrant` helper (reader/memgrant), which holds the right, opens
//! it and hands the open file over a socket. The right is checked when the
//! file is opened, so reading through it needs nothing more. Without the
//! helper's right the reader reports that access is needed, and the Companion
//! offers to grant it (`grant::grant_access`).

use std::fs::{self, File};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Larger private regions are texture and mesh data, not history or console text.
const MAX_REGION: usize = 1 << 30;

static ACCESS_NEEDED: AtomicBool = AtomicBool::new(false);

pub use crate::grant::granted;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub base: usize,
    pub size: usize,
}

pub struct GameProcess {
    pid: u32,
    mem: File,
}

impl GameProcess {
    pub fn open(pid: u32) -> Option<Self> {
        match File::open(format!("/proc/{pid}/mem")) {
            Ok(mem) => {
                ACCESS_NEEDED.store(false, Ordering::Relaxed);
                return Some(Self { pid, mem });
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(_) => return None,
        }
        for helper in crate::grant::helpers() {
            match through_helper(&helper, pid) {
                Ok(mem) => {
                    ACCESS_NEEDED.store(false, Ordering::Relaxed);
                    tracing::info!("Game memory: opened through {}", helper.display());
                    return Some(Self { pid, mem });
                }
                Err(e) => tracing::debug!("Game memory: {} could not open it: {e}", helper.display()),
            }
        }
        ACCESS_NEEDED.store(true, Ordering::Relaxed);
        None
    }

    /// Readable, private, anonymous regions: where the game keeps its heap.
    pub fn private_regions(&self) -> Vec<Region> {
        fs::read_to_string(format!("/proc/{}/maps", self.pid))
            .map(|maps| parse_maps(&maps))
            .unwrap_or_default()
    }

    /// The private regions too big to read (over `MAX_REGION`), for the log
    /// when something is not found.
    pub fn too_big(&self) -> Vec<Region> {
        fs::read_to_string(format!("/proc/{}/maps", self.pid))
            .map(|maps| too_big_in(&maps))
            .unwrap_or_default()
    }

    /// The game's mappings by kind, for the log: what the reader takes and
    /// what it leaves out (too big, file-backed, shared, unreadable), so a
    /// log shows where the game's memory sits when the reader finds some
    /// objects but not others. Names are file names only, with no folders, so
    /// the log carries no home path.
    pub fn layout(&self) -> String {
        let Ok(maps) = fs::read_to_string(format!("/proc/{}/maps", self.pid)) else {
            return "maps unreadable".to_owned();
        };
        layout(&maps)
    }

    /// Reads up to `buf.len()` bytes at `addr`. Returns how many were read;
    /// 0 when the memory is gone.
    pub fn read(&self, addr: usize, buf: &mut [u8]) -> usize {
        self.mem.read_at(buf, addr as u64).unwrap_or(0)
    }
}

/// Whether the last open failed for want of the right to read the game.
pub fn access_needed() -> bool {
    ACCESS_NEEDED.load(Ordering::Relaxed)
}

/// `GameProcess::layout` on the text of a maps file.
pub fn layout(maps: &str) -> String {
    // (count, bytes) per kind
    let mut kinds: [(&str, usize, usize); 6] = [
        ("read", 0, 0),
        ("too big", 0, 0),
        ("file-backed", 0, 0),
        ("shared", 0, 0),
        ("special", 0, 0),
        ("unreadable", 0, 0),
    ];
    let mut big: Vec<(usize, String)> = Vec::new();
    for line in maps.lines() {
        let mut parts = line.split_whitespace();
        let (Some(range), Some(perms), Some(_), Some(_), Some(inode)) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let name = parts.collect::<Vec<_>>().join(" ");
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::from_str_radix(start, 16), usize::from_str_radix(end, 16)) else {
            continue;
        };
        let size = end.saturating_sub(start);
        let perms = perms.as_bytes();
        let kind = if perms.len() < 4 || perms[0] != b'r' {
            5
        } else if perms[3] != b'p' {
            3
        } else if inode != "0" {
            2
        } else if matches!(name.as_str(), "[vvar]" | "[vvar_vclock]" | "[vdso]" | "[vsyscall]") {
            4
        } else if size > MAX_REGION {
            1
        } else {
            0
        };
        kinds[kind].1 += 1;
        kinds[kind].2 += size;
        if kind != 0 && kind != 5 && size >= 64 << 20 {
            let short = name.rsplit('/').next().unwrap_or("").to_owned();
            big.push((size, format!("{} {} MB {}", ["", "too big", "file", "shared", "special", ""][kind], size >> 20, short)));
        }
    }
    big.sort_by(|a, b| b.0.cmp(&a.0));
    let summary: Vec<String> = kinds
        .iter()
        .map(|(label, count, bytes)| format!("{label} {count} ({} MB)", bytes >> 20))
        .collect();
    let largest: Vec<String> = big.into_iter().take(6).map(|(_, s)| s.trim().to_owned()).collect();
    format!("{}; largest left out: [{}]", summary.join(", "), largest.join("; "))
}

/// The private regions in `/proc/<pid>/maps`: readable, private (`p`) and
/// not backed by a file, with the kernel's own pages left out. This matches
/// Windows' private committed memory: Wine allocates the game's heap as
/// anonymous mappings and maps its modules from files.
/// Readable, private, anonymous mappings over `MAX_REGION`.
fn too_big_in(maps: &str) -> Vec<Region> {
    maps.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (range, perms) = (parts.next()?, parts.next()?.as_bytes());
            let inode = parts.nth(2)?;
            let (start, end) = range.split_once('-')?;
            let (start, end) = (usize::from_str_radix(start, 16).ok()?, usize::from_str_radix(end, 16).ok()?);
            let size = end.saturating_sub(start);
            (perms.len() >= 4 && perms[0] == b'r' && perms[3] == b'p' && inode == "0" && size > MAX_REGION)
                .then_some(Region { base: start, size })
        })
        .collect()
}

pub fn parse_maps(maps: &str) -> Vec<Region> {
    let mut out = Vec::new();
    for line in maps.lines() {
        let mut parts = line.split_whitespace();
        let (Some(range), Some(perms), Some(_offset), Some(_dev), Some(inode)) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let name = parts.next().unwrap_or("");
        let perms = perms.as_bytes();
        if perms.len() < 4 || perms[0] != b'r' || perms[3] != b'p' || inode != "0" {
            continue;
        }
        if matches!(name, "[vvar]" | "[vvar_vclock]" | "[vdso]" | "[vsyscall]") {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::from_str_radix(start, 16), usize::from_str_radix(end, 16)) else {
            continue;
        };
        let size = end.saturating_sub(start);
        if size > 0 && size <= MAX_REGION {
            out.push(Region { base: start, size });
        }
    }
    out
}

/// Runs the helper for `pid` and takes the open memory file it sends.
fn through_helper(helper: &Path, pid: u32) -> Result<File, String> {
    let mut pair = [0 as RawFd; 2];
    // SAFETY: socketpair fills the two ints. Ours is close-on-exec; the
    // helper's end is inherited by the helper, which is how it gets it.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let (ours, theirs) = (pair[0], pair[1]);
    // SAFETY: setting close-on-exec on a descriptor we own.
    unsafe { libc::fcntl(ours, libc::F_SETFD, libc::FD_CLOEXEC) };
    let status = Command::new(helper).arg(pid.to_string()).arg(theirs.to_string()).status();
    // SAFETY: closing our copy of the helper's end, which we own.
    unsafe { libc::close(theirs) };
    let received = match status {
        Ok(s) if s.success() => receive_fd(ours),
        Ok(s) => Err(format!("exited {}", s.code().unwrap_or(-1))),
        Err(e) => Err(e.to_string()),
    };
    // SAFETY: closing the socket we own.
    unsafe { libc::close(ours) };
    // SAFETY: the descriptor came from SCM_RIGHTS and is ours alone now.
    received.map(|fd| unsafe { File::from_raw_fd(fd) })
}

fn receive_fd(socket: RawFd) -> Result<RawFd, String> {
    let mut byte = [0_u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0_u8; space];
    // SAFETY: a zeroed msghdr is a valid starting value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: msg points at live buffers for the call's duration.
    let got = unsafe { libc::recvmsg(socket, &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
    if got < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: the header pointers stay inside `control`, filled by recvmsg.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&raw const msg);
        if header.is_null() || (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err("no descriptor came".to_owned());
        }
        Ok(std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_games_heap_regions_and_nothing_else() {
        let maps = "\
140000000-140001000 r--p 00000000 103:02 1234 /home/p/.steam/steamapps/common/Deadlock/game/bin/win64/deadlock.exe
7f0000000000-7f0000400000 rw-p 00000000 00:00 0
7f0000400000-7f0000800000 ---p 00000000 00:00 0
7f0000800000-7f0000900000 r-xp 00000000 103:02 99 /usr/lib/wine/x86_64-windows/ntdll.dll
7f0000900000-7f0000a00000 rw-s 00000000 00:05 77 /memfd:wine-shm
55d000000000-55d000021000 rw-p 00000000 00:00 0                          [heap]
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
7ffd00100000-7ffd00102000 r--p 00000000 00:00 0                          [vvar]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
";
        let regions = parse_maps(maps);
        assert_eq!(
            regions,
            vec![
                Region { base: 0x7f00_0000_0000, size: 0x40_0000 },
                Region { base: 0x55d0_0000_0000, size: 0x2_1000 },
                Region { base: 0x7ffd_0000_0000, size: 0x2_1000 },
            ]
        );
    }

    #[test]
    fn the_layout_counts_what_is_read_and_what_is_left_out() {
        let maps = "\
140000000-140001000 r--p 00000000 103:02 1234 /home/p/Deadlock/game/bin/win64/deadlock.exe
7f0000000000-7f0000400000 rw-p 00000000 00:00 0
7f0000400000-7f0000800000 ---p 00000000 00:00 0
7e0000000000-7e0050000000 rw-p 00000000 00:00 0
7f0000900000-7f0008900000 rw-s 00000000 00:05 77 /memfd:wine-shm (deleted)
7ffd00100000-7ffd00102000 r--p 00000000 00:00 0                          [vvar]
";
        let text = layout(maps);
        assert!(text.starts_with("read 1 (4 MB), too big 1 (1280 MB), file-backed 1 (0 MB), shared 1 (128 MB), special 1 (0 MB), unreadable 1 (4 MB)"), "{text}");
        assert!(text.ends_with("largest left out: [too big 1280 MB; shared 128 MB memfd:wine-shm (deleted)]"), "{text}");
        assert!(!text.contains("/home/"), "{text}");
    }

    #[test]
    fn a_region_over_a_gigabyte_is_left_out() {
        let maps = "7f0000000000-7f0080000000 rw-p 00000000 00:00 0\n";
        assert!(parse_maps(maps).is_empty());
    }
}
