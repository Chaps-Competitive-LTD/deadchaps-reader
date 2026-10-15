//! Telling the game apart in the process list.
//!
//! Windows names a process after its program, so `deadlock.exe` is enough.
//! On Linux the name the kernel keeps is the main thread's: Source 2 names
//! its threads, and Wine hands a thread's name on to Linux, so under Proton
//! the game need not be called `deadlock.exe` at all. On Linux the program
//! Proton started (`Z:\...\deadlock.exe`, the first argument) counts too.
//! The list holds processes only, never their threads (see [`crate::procs`]):
//! a list with threads would match a different one of the game's threads on
//! each look, and the reader would open the game afresh every few seconds.

use crate::procs::{self, Proc};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Whether `process` is the program `exe`.
pub fn is(process: &Proc, exe: &str) -> bool {
    process.name.eq_ignore_ascii_case(exe) || process.program.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(exe))
}

/// Whether any of `exes` is running: one look at the list.
pub fn any_running(exes: &[&str]) -> bool {
    procs::list().iter().any(|p| exes.iter().any(|exe| is(p, exe)))
}

/// Processes that look like the game but did not match, as "name (program)"
/// with no paths, for the log when the game is not found.
pub fn lookalikes(list: &[Proc]) -> Vec<String> {
    let looks = |s: &str| {
        let s = s.to_ascii_lowercase();
        s.contains("deadlock") || s.contains("project8")
    };
    list.iter()
        .filter(|p| looks(&p.name) || p.program.as_deref().is_some_and(looks))
        .map(|p| format!("{} ({})", p.name, p.program.as_deref().unwrap_or("")))
        .take(8)
        .collect()
}

/// How long one look at the process list answers for. Each look walks
/// every process on the PC, and the callers between them ask about twice a
/// second while the game is closed. Unshared, those looks cost about 2.7% of
/// a core with the game not running.
const PID_FRESH_FOR: Duration = Duration::from_secs(2);
static PID_SEEN: Mutex<Option<(Instant, String, Option<u32>)>> = Mutex::new(None);

/// The running game's process id, when it is running: one look at the
/// process list shared by every caller for `PID_FRESH_FOR`.
pub fn game_pid(exe: &str) -> Option<u32> {
    let mut seen = PID_SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((at, name, pid)) = seen.as_ref()
        && at.elapsed() < PID_FRESH_FOR
        && name.eq_ignore_ascii_case(exe)
    {
        return *pid;
    }
    let list = procs::list();
    let pid = list.iter().find(|p| is(p, exe)).map(|p| p.pid);
    // Linux: log, once a run, anything that looks like the game but did not
    // match, so the log shows what the game is called on that PC.
    #[cfg(target_os = "linux")]
    if pid.is_none() {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let like = lookalikes(&list);
        if !like.is_empty() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("Game memory: no {exe} running, but these look like it: {}", like.join(", "));
        }
    }
    *seen = Some((Instant::now(), exe.to_owned(), pid));
    pid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(name: &str, program: Option<&str>) -> Proc {
        Proc { pid: 1, parent: None, name: name.to_owned(), program: program.map(str::to_owned) }
    }

    #[test]
    fn the_game_by_its_name_or_by_the_program_proton_started() {
        let exe = "deadlock.exe";
        assert!(is(&proc("deadlock.exe", None), exe));
        assert!(is(&proc("Deadlock.exe", None), exe));
        // Linux: the main thread renamed itself, but the first argument still names the game
        assert!(is(&proc("MainThrd", Some("deadlock.exe")), exe));
        assert!(!is(&proc("MainThrd", Some("deadlock.exe.bak")), exe));
        assert!(!is(&proc("python3", Some("python3")), exe));
    }

    #[test]
    fn lookalikes_name_what_might_be_the_game() {
        let list = [proc("MainThrd", Some("Deadlock.exe")), proc("steam", Some("steam")), proc("project8.exe", None)];
        assert_eq!(lookalikes(&list), vec!["MainThrd (Deadlock.exe)".to_owned(), "project8.exe ()".to_owned()]);
    }

    #[test]
    fn no_such_program_is_not_running() {
        assert_eq!(game_pid("no-such-game-here.exe"), None);
        assert!(!any_running(&["no-such-game-here.exe"]));
    }
}
