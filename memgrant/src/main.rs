//! The DeadChaps Companion's memory grant, Linux only.
//!
//! Most Linux systems (Yama `ptrace_scope` 1: Ubuntu, Arch, CachyOS) let a
//! program open another's memory only if it started it, and Steam starts
//! Deadlock, not the Companion. This program is the one piece given that
//! right, the `cap_sys_ptrace` capability, set when the .deb installs or
//! when the player allows it in the Companion. It does one thing and exits:
//!
//!   deadchaps-memgrant <pid> <socket fd>
//!
//! opens `/proc/<pid>/mem` read-only and sends the open file down the socket
//! the caller passed in. Linux checks the right when the file is opened, so
//! the caller reads through it without ever holding the right itself.
//!
//! It refuses any process that is not Deadlock or that belongs to another
//! user. Deadlock is `deadlock.exe` or `project8.exe`, matched by the process
//! name or by the program Proton started: the game renames its main thread,
//! and Linux names a process after its main thread.
//!
//! Exit codes: 0 sent, 2 bad arguments, 3 no such process, 4 not Deadlock,
//! 5 another user's, 6 could not open the memory (no right), 7 could not send.

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(match linux::run(&std::env::args().collect::<Vec<_>>()) {
        Ok(()) => 0,
        Err(code) => code,
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("deadchaps-memgrant is for Linux");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;

    /// The game's program names under Proton.
    const GAME_NAMES: [&str; 2] = ["deadlock.exe", "project8.exe"];

    pub fn run(args: &[String]) -> Result<(), i32> {
        let pid: u32 = args.get(1).and_then(|s| s.parse().ok()).filter(|p| *p > 0).ok_or(2)?;
        let socket: i32 = args.get(2).and_then(|s| s.parse().ok()).filter(|f| *f > 2).ok_or(2)?;
        check(pid)?;
        let mem = File::open(format!("/proc/{pid}/mem")).map_err(|_| 6)?;
        // check again: the process could have ended and its id been reused since the first check
        check(pid)?;
        send_fd(socket, mem.as_raw_fd()).map_err(|_| 7)
    }

    /// Passes only for Deadlock, run by the user who asked.
    fn check(pid: u32) -> Result<(), i32> {
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).map_err(|_| 3)?;
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).map_err(|_| 3)?;
        if !is_game(&comm) && !program_is_game(&cmdline) {
            return Err(4);
        }
        let status = fs::read_to_string(format!("/proc/{pid}/status")).map_err(|_| 3)?;
        // SAFETY: getuid cannot fail.
        let caller = unsafe { libc::getuid() };
        if real_uid(&status) != Some(caller) {
            return Err(5);
        }
        Ok(())
    }

    pub fn is_game(comm: &str) -> bool {
        let comm = comm.trim().to_ascii_lowercase();
        GAME_NAMES.contains(&comm.as_str())
    }

    /// Whether the first argument of a `/proc/<pid>/cmdline` (NUL separated)
    /// is the game's program, by Linux or Windows path.
    pub fn program_is_game(cmdline: &[u8]) -> bool {
        let first = cmdline.split(|b| *b == 0).next().unwrap_or_default();
        let first = String::from_utf8_lossy(first);
        first.rsplit(['/', '\\']).next().is_some_and(is_game)
    }

    /// The real uid from `/proc/<pid>/status` ("Uid:\treal\teffective...").
    pub fn real_uid(status: &str) -> Option<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|uid| uid.parse().ok())
    }

    /// Sends `fd` over the unix socket `socket` (SCM_RIGHTS).
    fn send_fd(socket: i32, fd: i32) -> std::io::Result<()> {
        let mut byte = [0_u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        // SAFETY: CMSG_SPACE only computes a size.
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        let mut control = vec![0_u8; space];
        // SAFETY: zeroed msghdr is a valid starting value.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer holds one header and one int, as sized above.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&raw const msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), fd);
        }
        // SAFETY: msg points at live buffers for the call's duration.
        let sent = unsafe { libc::sendmsg(socket, &raw const msg, 0) };
        if sent < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn only_the_game_by_its_proton_names() {
            assert!(is_game("deadlock.exe\n"));
            assert!(is_game("Project8.exe"));
            assert!(!is_game("steam"));
            assert!(!is_game("deadlock.exe.evil"));
            assert!(!is_game("wineserver"));
        }

        #[test]
        fn or_by_the_program_proton_started() {
            assert!(program_is_game(b"Z:\\home\\p\\Deadlock\\game\\bin\\win64\\deadlock.exe\0-novid\0"));
            assert!(program_is_game(b"/x/Deadlock/game/bin/win64/deadlock.exe"));
            assert!(!program_is_game(b"/usr/bin/python3\0/x/deadlock.exe\0"));
            assert!(!program_is_game(b"Z:\\x\\deadlock.exe.evil"));
            assert!(!program_is_game(b""));
        }

        #[test]
        fn the_real_uid_is_read() {
            let status = "Name:\tdeadlock.exe\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n";
            assert_eq!(real_uid(status), Some(1000));
            assert_eq!(real_uid("Name:\tx\n"), None);
        }
    }
}
