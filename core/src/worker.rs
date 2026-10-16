//! The thread that watches the running game. It decides when to run a full
//! pass over the game's memory, when a cheaper read will do (the kept pieces,
//! the players' known addresses, a sweep for players alone), and turns what
//! it reads into the [`MemoryView`]. See the [crate] documentation for what
//! is read and why.

use super::friends_presence;
use super::view::{
    EndedMatch, GameEnd, KEEP_ENDED, MemoryView, Objectives, load_ended, match_leftover, party_shaped,
};
use crate::game_process::game_pid;
use super::players::{self, MatchPlayer, Offsets};
use super::process::{GameProcess, Region};
use super::{
    HistoryEntry, MatchEnd, RECORD_SIZE, find_history, find_match_ends, find_match_starts,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Memory is read in pieces this big. Each overlaps the last by a player
/// object's size, more than a history record or a console line, so nothing
/// split across two pieces is missed.
const CHUNK: usize = 4 << 20;
const OVERLAP: usize = players::OBJECT_BYTES + 8;
const _: () = assert!(OVERLAP > RECORD_SIZE * 2 + 128);

/// How long an account seen in the lobby stays on the watch list. While it
/// does, a pass that turns down that account's object logs why.
const WATCH_FOR: Duration = Duration::from_secs(3600);
/// A full pass pauses this long after every so many bytes, so it never takes
/// a whole core away from the game.
const THROTTLE_EVERY: usize = 64 << 20;
const THROTTLE_PAUSE: Duration = Duration::from_millis(40);
/// How often the kept pieces (those that held history, the console or the
/// rich presence) are read again.
const WATCH_EVERY: Duration = Duration::from_secs(20);
/// How often the match's players are read again where the last pass found
/// them. That is a few kilobytes, cheap enough to run every few seconds so a
/// kill shows soon after it happens.
const PLAYERS_EVERY: Duration = Duration::from_secs(5);
/// How often the process list is checked while the game is not running or
/// will not open.
const GAME_CHECK_EVERY: Duration = Duration::from_secs(15);
/// How often the friend records the last pass found are read again. Each
/// costs two small reads, and Steam updates the minutes in a friend's rich
/// presence once a minute.
const PRESENCE_EVERY: Duration = Duration::from_secs(20);
/// The most friend records followed. Each friend has one live record; the
/// rest are freed copies.
const MAX_FRIEND_RECORDS: usize = 4096;
/// A full pass reads the whole process (8 to 10 GB, four to seven seconds of
/// CPU), so it runs when something happens rather than on a clock: the game
/// attached, the queue started or stopped, the console named a match, a
/// match ended, the kept pieces lost their records. Between those events it
/// runs this often out of a match, because a match opened in game leaves
/// Valve's reply on the heap and a refetched history list lands somewhere
/// new ...
const IDLE_PASS_EVERY: Duration = Duration::from_secs(600);
/// ... and this often in one, where the kept pieces carry everything.
const MATCH_PASS_EVERY: Duration = Duration::from_secs(1800);
/// A pass something asked for (a match ended, the pieces lost their records)
/// still waits this long after the last one ...
const MIN_PASS_GAP: Duration = Duration::from_secs(30);
/// ... and on a PC where the last pass took longer than `SLOW_PASS`, that
/// long times `PASS_REST_RATIO`, so the reader reads at most a fifth of the
/// time. A pass over 14 GB has taken 40 to 52 s on a PC short of memory, and
/// with only `MIN_PASS_GAP` between them the reader read the game for five
/// minutes almost without a break, which the player felt as the game
/// slowing down.
const SLOW_PASS: Duration = Duration::from_secs(20);
const PASS_REST_RATIO: u32 = 4;
/// A game read while it is still starting (about 2 MB in 18 regions at
/// launch, with nothing in it) is read again soon rather than after the idle
/// interval. A game that has loaded is many gigabytes.
const STILL_LOADING_BYTES: usize = 1 << 30;
/// The most history entries the view keeps.
const KEEP_ENTRIES: usize = 30;
/// How many history lists the view carries: the local player's, and enough
/// of the others seen to tell them apart.
const KEEP_RUNS: usize = 12;

pub type OnChange = Box<dyn Fn() + Send + 'static>;
/// A replay address found in the game's memory, once each.
pub type OnSalts = Box<dyn Fn(&str) + Send + 'static>;
pub type SteamAccount = Box<dyn Fn() -> Option<String> + Send + 'static>;
/// Whether Valve's CDN has the metadata file at a URL, answered with a HEAD
/// request. It ties one of Valve's replies to its match when the game's
/// history cannot. The program running the reader makes the request, so the
/// reader itself never uses the network.
pub type ValveHas = Box<dyn Fn(&str) -> bool + Send + 'static>;

/// What the reader asks of whoever runs it, and tells them.
pub struct Hooks {
    /// The local player's Steam account id, when known.
    pub steam_account: SteamAccount,
    /// Called whenever the view changed.
    pub on_change: OnChange,
    /// Called with each replay address found, once each.
    pub on_salts: OnSalts,
    /// See `ValveHas`.
    pub valve_has: ValveHas,
}

#[derive(Debug, Clone, Copy)]
struct Control {
    enabled: bool,
    allowed: bool,
    stop: bool,
    generation: u64,
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    view: Mutex<MemoryView>,
    /// Where the ended matches are kept between runs.
    ended_path: PathBuf,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn change(&self, apply: impl FnOnce(&mut Control)) {
        let mut control = lock(&self.control);
        apply(&mut control);
        control.generation = control.generation.wrapping_add(1);
        drop(control);
        self.wake.notify_all();
    }

    /// Waits up to `timeout`, or until a setting changes.
    fn wait(&self, timeout: Duration) {
        let guard = lock(&self.control);
        let seen = guard.generation;
        let _unused = self
            .wake
            .wait_timeout_while(guard, timeout, |c| c.generation == seen && !c.stop)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

pub struct GameMemory {
    shared: Arc<Shared>,
}

impl GameMemory {
    pub fn start(
        enabled: bool,
        allowed: bool,
        exe: String,
        ended_path: PathBuf,
        hooks: Hooks,
    ) -> Self {
        let ended = load_ended(&ended_path, unix_now());
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                enabled,
                allowed,
                stop: false,
                generation: 0,
            }),
            wake: Condvar::new(),
            view: Mutex::new(MemoryView {
                enabled,
                allowed,
                ended,
                ..MemoryView::default()
            }),
            ended_path,
        });
        let worker = Arc::clone(&shared);
        if let Err(error) = thread::Builder::new()
            .name("game-memory".to_owned())
            .spawn(move || run(&worker, &exe, &hooks))
        {
            warn!("Game memory: could not start: {error}");
        }
        Self { shared }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.shared.change(|c| c.enabled = enabled);
    }

    pub fn set_allowed(&self, allowed: bool) {
        self.shared.change(|c| c.allowed = allowed);
    }

    pub fn stop(&self) {
        self.shared.change(|c| c.stop = true);
    }

    /// Adds a match that ended without an end line in memory, for example
    /// because the game restarted and lost its console, identified instead by
    /// deadlock-api's live lookup. It is listed and saved like any other ended
    /// match. Returns false when it is already listed.
    pub fn add_ended(&self, ended: EndedMatch) -> bool {
        let mut view = lock(&self.shared.view);
        if view.ended.iter().any(|m| m.match_id == ended.match_id) {
            return false;
        }
        view.ended.insert(0, ended);
        view.ended.truncate(KEEP_ENDED);
        crate::store::save(&self.shared.ended_path, &view.ended);
        true
    }

    pub fn view(&self) -> MemoryView {
        lock(&self.shared.view).clone()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Puts the calling thread below normal priority.
#[cfg(target_os = "linux")]
fn below_normal_priority() {
    // SAFETY: setpriority takes no pointers. With PRIO_PROCESS and 0 it
    // applies to the calling thread, since Linux keeps niceness per thread.
    let ok = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 5) };
    if ok != 0 {
        warn!("Game memory: could not lower the reader's priority");
    }
}

/// CPU this thread has used, from the thread's own CPU clock, so a pass logs
/// what it cost rather than only how long it took.
#[cfg(target_os = "linux")]
fn thread_cpu() -> Duration {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec out-pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut now) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(u64::try_from(now.tv_sec).unwrap_or(0), u32::try_from(now.tv_nsec).unwrap_or(0))
}

/// Puts the calling thread below normal priority.
#[cfg(windows)]
fn below_normal_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    // SAFETY: the current thread's pseudo handle needs no closing, and the
    // priority is one of the documented values.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    if ok == 0 {
        warn!("Game memory: could not lower the reader's priority");
    }
}

/// A read of the whole game in Windows' background mode, for as long as the
/// value lives: the thread's disk and memory priority drop as well as its
/// CPU priority. Reading a part of the game that Windows has moved out of
/// memory brings it back from the disk, and in background mode that waits
/// behind the game's own reads.
#[cfg(windows)]
struct Background(bool);

#[cfg(windows)]
impl Background {
    fn begin() -> Self {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
        // SAFETY: the current thread's pseudo handle, and a documented mode
        // that only the thread itself may set.
        Self(unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) } != 0)
    }

    #[cfg(test)]
    fn on(&self) -> bool {
        self.0
    }
}

#[cfg(windows)]
impl Drop for Background {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_END};
        if self.0 {
            // SAFETY: as in `begin`; ending the mode restores the thread's
            // priority as it was before.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END) };
        }
    }
}

/// On Linux the thread's disk priority drops to the idle class instead, the
/// part of background mode Linux has, and comes back as it was after.
#[cfg(target_os = "linux")]
struct Background(Option<libc::c_long>);

#[cfg(target_os = "linux")]
const IOPRIO_WHO_PROCESS: libc::c_long = 1;

#[cfg(target_os = "linux")]
impl Background {
    fn begin() -> Self {
        const IDLE: libc::c_long = 3 << 13;
        // SAFETY: ioprio_get and ioprio_set take plain integers; who 0 is the
        // calling thread.
        let before = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) };
        let set = unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IDLE) } == 0;
        Self((set && before >= 0).then_some(before))
    }

    #[cfg(test)]
    fn on(&self) -> bool {
        self.0.is_some()
    }
}

#[cfg(target_os = "linux")]
impl Drop for Background {
    fn drop(&mut self) {
        if let Some(before) = self.0 {
            // SAFETY: as in `begin`.
            unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, before) };
        }
    }
}

/// CPU this thread has used, user and kernel time together, so a pass logs
/// what it cost rather than only how long it took.
#[cfg(windows)]
fn thread_cpu() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: four valid FILETIME out-pointers; the current thread's handle
    // needs no closing.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Duration::ZERO;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // FILETIME counts 100-nanosecond ticks.
    Duration::from_nanos(
        ticks(kernel)
            .saturating_add(ticks(user))
            .saturating_mul(100),
    )
}

/// What one read of a set of pieces found.
#[derive(Default)]
struct Found {
    entries: Vec<HistoryEntry>,
    ends: Vec<MatchEnd>,
    starts: Vec<MatchEnd>,
    /// The pieces that held any of it (the kept pieces), to read again next
    /// time.
    hot: Vec<Region>,
    /// The whole regions those pieces sat in, logged to show where the game
    /// keeps them.
    hit_regions: Vec<Region>,
    bytes: usize,
    /// Time spent copying memory out of the game; `search_time` is the time
    /// spent searching the copies.
    read_time: Duration,
    search_time: Duration,
    /// The match's players and where each one sat, so the next read goes
    /// straight to them instead of over the whole game again.
    players: Vec<(usize, MatchPlayer)>,
    /// Objects for watched accounts that failed the limits, and why.
    rejected: Vec<players::Rejected>,
    /// The text around the game's `steam_display` rich presence keys.
    presence: Vec<String>,
    /// Replay addresses, whole, once each.
    salts_urls: Vec<String>,
    /// Valve's replies to the client asking for a match's metadata.
    replies: Vec<super::salts_probe::Reply>,
    /// The lobby object for the match the console named.
    lobby_hits: Vec<super::salts_probe::LobbyHit>,
    /// Each history list found, kept apart: the game holds other players'
    /// histories too, and only a comparison with the account's known history
    /// can tell which list is this account's.
    runs: Vec<Vec<HistoryEntry>>,
    /// Where the game's rules object sits, for the objective timers.
    rules: Vec<usize>,
    /// Why the places holding the minimap bounds were not the rules object,
    /// a few of them, for the log when a pass finds none.
    rules_misses: Vec<String>,
    /// The game's Steam friend records (a full pass only).
    friend_records: Vec<usize>,
}

/// What a full pass looks for besides the history, the console and the
/// players: the accounts whose objects are worth a log line when turned
/// away, and the lobby (lobby id, match id) whose modes are still unknown.
#[derive(Default)]
struct Watching<'a> {
    accounts: &'a [u32],
    lobby: Option<(u64, u64)>,
}

/// Reads `regions` piece by piece and searches each piece for everything
/// [`Found`] holds. `throttle` marks a full pass: it pauses every
/// `THROTTLE_EVERY` bytes and also runs the searches only a full pass makes
/// (the rules object, friend records, Valve's replies, the lobby object).
fn read_pieces(
    game: &GameProcess,
    regions: &[Region],
    throttle: bool,
    our_account: Option<u32>,
    watching: &Watching<'_>,
    found: &mut Found,
) {
    let watch = watching.accounts;
    let watch_lobby = watching.lobby;
    let now = u32::try_from(unix_now()).unwrap_or(u32::MAX);
    let mut buf = vec![0_u8; CHUNK];
    let mut since_pause = 0_usize;
    for region in regions {
        let mut off = 0_usize;
        while off < region.size {
            let want = CHUNK.min(region.size - off);
            let reading = Instant::now();
            let got = game.read(region.base + off, &mut buf[..want]);
            found.read_time += reading.elapsed();
            if got >= RECORD_SIZE {
                let piece = &buf[..got];
                let searching = Instant::now();
                let history = find_history(piece, now);
                let ends = find_match_ends(piece);
                let starts = find_match_starts(piece);
                // The match's players, when the local account is known.
                let here = our_account.map_or_else(Vec::new, |account| {
                    players::find_players_watching(
                        piece,
                        Offsets::CURRENT,
                        account,
                        watch,
                        &mut found.rejected,
                    )
                });
                if found.presence.len() < super::presence_probe::AT_MOST {
                    found
                        .presence
                        .extend(super::presence_probe::snippets(piece, our_account));
                }
                for url in super::salts_probe::urls(piece) {
                    if !found.salts_urls.contains(&url) {
                        found.salts_urls.push(url);
                    }
                }
                if throttle && found.rules.len() < 4 {
                    let hits = super::rules::find(piece, region.base + off);
                    if hits.is_empty() && found.rules_misses.len() < 4 {
                        found.rules_misses.extend(super::rules::misses(piece).into_iter().take(4));
                    }
                    found.rules.extend(hits);
                }
                if throttle && found.friend_records.len() < MAX_FRIEND_RECORDS {
                    found.friend_records.extend(
                        friends_presence::find_records(piece, region.base + off)
                            .into_iter()
                            .map(|(at, _)| at),
                    );
                }
                if throttle {
                    // Valve's replies can sit anywhere on the heap, so only
                    // a full pass looks for them.
                    for reply in super::salts_probe::replies(piece, region.base + off) {
                        if !found.replies.contains(&reply) {
                            found.replies.push(reply);
                        }
                    }
                    if let Some((lobby_id, match_id)) = watch_lobby
                        && found.lobby_hits.len() < 3
                    {
                        found.lobby_hits.extend(super::salts_probe::find_lobby(
                            piece,
                            region.base + off,
                            lobby_id,
                            match_id,
                        ));
                    }
                }
                found.search_time += searching.elapsed();
                found.players.extend(
                    here.into_iter()
                        .map(|(at, player)| (region.base + off + at, player)),
                );
                if !history.is_empty()
                    || !ends.is_empty()
                    || super::presence_probe::has_presence(piece)
                {
                    if !found.hit_regions.contains(region) {
                        found.hit_regions.push(*region);
                    }
                    found.hot.push(Region {
                        base: region.base + off,
                        size: got,
                    });
                }
                // Records back to back form one player's history list. A run
                // needs at least two records to say whose it is.
                found.runs.extend(
                    super::runs_of(&history)
                        .into_iter()
                        .filter(|run| run.len() >= 2),
                );
                found.entries.extend(history.into_iter().map(|(_, e)| e));
                found.ends.extend(ends);
                found.starts.extend(starts);
            }
            found.bytes += got;
            since_pause += got;
            if throttle && since_pause >= THROTTLE_EVERY {
                since_pause = 0;
                thread::sleep(THROTTLE_PAUSE);
            }
            if want <= OVERLAP || off + want >= region.size {
                break;
            }
            off += want - OVERLAP;
        }
    }
}

/// Newest first, one entry per match, at most `KEEP_ENTRIES`.
fn merge_entries(entries: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut by_match: HashMap<u64, HistoryEntry> = HashMap::new();
    for entry in entries {
        by_match.entry(entry.match_id).or_insert(entry);
    }
    let mut out: Vec<HistoryEntry> = by_match.into_values().collect();
    out.sort_by(|a, b| {
        b.start_time
            .cmp(&a.start_time)
            .then(b.match_id.cmp(&a.match_id))
    });
    out.truncate(KEEP_ENTRIES);
    out
}

/// Whether the quick re-read of the kept pieces asks for a full pass: only
/// when those pieces held the match history or an end line at the last pass
/// and hold neither now, which means the game freed or moved them. Pieces
/// kept for other text (the rich presence, a replay address) never held
/// either, so finding nothing in them means nothing; treating it as a loss
/// runs a full pass about every 40 s, 8 GB and 5 s of CPU each, and stops
/// the player re-reads for the 10 s each pass takes.
///
/// Never in a match: the game rewrites the buffer the history sits in while
/// one is played, so every re-read finds it gone. Asking then runs a full
/// pass about every 48 s for the whole match, 11 to 18 s of CPU each, which
/// players feel as the game freezing. The history cannot change until the
/// match ends, and the end asks for a pass of its own.
fn watch_wants_pass(held_history: bool, entries_now: usize, ends_now: usize, in_match: bool) -> bool {
    !in_match && held_history && entries_now == 0 && ends_now == 0
}

/// How long after the game is attached the reader keeps looking for the
/// local player, until it finds them once.
const SETTLE_FOR: Duration = Duration::from_secs(600);

/// How many passes in one match may look for the rules object (the source
/// of the match timers) when the last one did not find it, a pass every
/// `MIN_PASS_GAP` ...
const RULES_TRIES: u32 = 4;
/// ... and after those, one pass this often for as long as the match lasts
/// without it. The four tries have missed in a match that followed a replay
/// watched in the same game session, which left the timers gone for the rest
/// of the match.
const RULES_RETRY_EVERY: Duration = Duration::from_secs(300);

/// Whether a pass is wanted to find the rules object: seated in a match
/// without it, and not yet out of tries. Only a full pass finds the object.
/// A match's first pass can come before the object exists, or the object
/// can move, and in a match the scheduled passes are 30 minutes apart, so
/// without these tries the timers could be missing for most of the match.
fn rules_pass_wanted(seated: bool, held: bool, tries: u32, since_pass: Option<Duration>) -> bool {
    seated && !held && (tries < RULES_TRIES || since_pass.is_none_or(|gap| gap >= RULES_RETRY_EVERY))
}

/// How many passes in one match may look for players missing from the
/// lobby, a pass every `MIN_PASS_GAP`.
const SHORT_TRIES: u32 = 4;
/// What a full pass costs, counted in sweeps. A measured pass over about
/// 9 GB took about 10 s of CPU; a sweep of the same memory costs about 2.7 s,
/// the pass's 2.2 s of reading plus about 54 ms a gigabyte of searching for
/// players. The budget for finding missing players is `SHORT_TRIES` full
/// passes, and each sweep spends a quarter of one.
const SWEEPS_PER_PASS: u32 = 4;

/// The tries spent on missing players, `spent` counted in sweeps; a pass
/// partly paid for by sweeps counts whole, so the budget is never exceeded.
fn short_tries(spent: u32) -> u32 {
    spent.div_ceil(SWEEPS_PER_PASS)
}

/// How many seats of the running match were not read. Seats say the mode:
/// a 6v6 seats team 2 in 1 to 6 and team 3 in 7 to 12, Street Brawl seats
/// them in 1 to 4 and 5 to 8 (as read from the live game). While the seats
/// read could belong to either mode, both modes have a seat missing, so the
/// 6v6 count is given.
fn missing_seats(players: &[MatchPlayer]) -> usize {
    let mut seats: Vec<(u8, u8)> = players.iter().filter(|p| p.slot > 0).map(|p| (p.team, p.slot)).collect();
    if seats.is_empty() {
        return 0;
    }
    seats.sort_unstable();
    seats.dedup();
    let brawl = seats.iter().any(|&(team, slot)| team == 3 && slot <= 6);
    let full: usize = if brawl { 8 } else { 12 };
    full.saturating_sub(seats.len())
}

/// Whether a pass is wanted for players missing from the lobby: seated with
/// seats unread, and not yet out of tries. Players are only found by reading
/// the whole game and are then re-read where they were found, so a pass that
/// comes while the match is still loading them would otherwise leave the
/// lobby short for the whole match. A player who left keeps the seat empty
/// for good, so the tries are capped.
fn short_pass_wanted(seated: bool, missing: usize, tries: u32) -> bool {
    seated && missing > 0 && tries < SHORT_TRIES
}

/// Whether a pass is wanted because the game was attached and the local
/// player has not been read yet. The first pass often runs at the main menu
/// a few seconds after launch, with about 1.7 GB mapped and no players. That
/// counts as loaded, so without this the next pass would be the idle one ten
/// minutes later, and the hideout, the party and the queue that follow
/// within a minute would go unseen.
fn settling_pass_wanted(attached_for: Duration, seen_us: bool) -> bool {
    !seen_us && attached_for < SETTLE_FOR
}

/// How long after the last sign of players coming the reader keeps sweeping
/// for them (see `seating_sweeps`).
const SEATING_FOR: Duration = Duration::from_secs(240);
/// For this long after a sign the sweeps come one loop apart, then every
/// `SWEEP_SLOW_GAP`. Across a week of logged matches, the first players of
/// every match except a custom lobby still filling were readable within 51 s
/// of the sign (the queue popping, the game attaching).
const SWEEP_FAST_FOR: Duration = Duration::from_secs(90);
/// The least time from the end of one read of the whole game (a sweep or a
/// full pass) to the start of a sweep: one turn of the loop.
const SWEEP_GAP: Duration = Duration::from_secs(4);
/// The same after the first `SWEEP_FAST_FOR`.
const SWEEP_SLOW_GAP: Duration = Duration::from_secs(25);
/// A sweep pauses this long after every `THROTTLE_EVERY` bytes. A measured
/// full pass over 9.1 GB spent 2.2 s reading and 7.8 s searching, about
/// 70 ms of work per 64 MB, and with its 40 ms rest it takes about 64% of a
/// core while it runs. A sweep searches for players alone, 3.4 ms per 64 MB
/// against the pass's 54 (measured on a synthetic heap shaped like the
/// game's, where all the searches together came within about 1% of the real
/// pass), so it works about 19 ms per 64 MB, and a 10 ms rest keeps it at
/// the same share of a core.
const SWEEP_PAUSE: Duration = Duration::from_millis(10);

/// Whether the reader sweeps for players: a sign came `since` ago that a
/// lobby is about to be seated, and nobody is read, or (in the first
/// `SWEEP_FAST_FOR`) a match is seated with seats unread. The signs are the
/// lobby emptying, the queue stopping, the console naming a match and the
/// game attaching (see `run`).
///
/// Players are only found by reading the whole game, and they become
/// readable at a moment no event marks: the hideout party leaves memory as
/// the queue pops, and the match's players appear some seconds later. A full
/// pass takes 15 to 20 s and waits `MIN_PASS_GAP` after the last, so a match
/// not yet loaded at the first pass would wait out the gap and a second
/// pass, often 45 to 55 s in all. A sweep looks for players alone at a
/// quarter of the cost, so sweeps can run a loop apart until the players
/// turn up. The same holds after a match ends, when the hideout is read
/// again.
fn seating_sweeps(since: Option<Duration>, players_now: usize, seated_short: bool) -> bool {
    since.is_some_and(|gap| {
        gap < SEATING_FOR && (players_now == 0 || (seated_short && gap < SWEEP_FAST_FOR))
    })
}

/// Whether a sweep runs now: sweeping (`seating_sweeps`), and the last read
/// of the whole game, a sweep or a full pass, ended `since_read` ago, at
/// least `SWEEP_GAP` in the first `SWEEP_FAST_FOR` after the sign and
/// `SWEEP_SLOW_GAP` after that. On a slow PC, where the last sweep took
/// (`took`) more than `SLOW_SWEEP`, never sooner than that times
/// `SWEEP_FAST_RATIO` or `SWEEP_SLOW_RATIO`.
///
/// A sweep reads the whole game, about 9.5 GB, in about 6 s on a mid-range
/// PC, so sweeps a fixed 4 s apart keep the reader busy 60% of the time
/// after every match, and on a slow PC, where a sweep takes 15 s or more,
/// almost all of the time, competing with the game. Spaced by what the last
/// sweep took, the reader is busy at most half the time while the next
/// lobby is coming and a fifth of it after. A PC that sweeps within
/// `SLOW_SWEEP` keeps the fixed spacing: replaying a week of logged matches
/// (`seating_replay`) with the proportional spacing everywhere read lobbies
/// up to 7.6 s later.
fn sweep_due(
    since: Option<Duration>,
    players_now: usize,
    seated_short: bool,
    since_read: Option<Duration>,
    took: Option<Duration>,
) -> bool {
    let fast = since.is_some_and(|s| s < SWEEP_FAST_FOR);
    let (floor, ratio) = if fast {
        (SWEEP_GAP, SWEEP_FAST_RATIO)
    } else {
        (SWEEP_SLOW_GAP, SWEEP_SLOW_RATIO)
    };
    let gap = match took {
        Some(took) if took > SLOW_SWEEP => floor.max(took * ratio),
        _ => floor,
    };
    seating_sweeps(since, players_now, seated_short) && since_read.is_none_or(|read| read >= gap)
}

/// The rest after a read of the whole game, as a multiple of how long it
/// took: 1 keeps the reader busy at most half the time, 4 a fifth.
const SWEEP_FAST_RATIO: u32 = 1;
const SWEEP_SLOW_RATIO: u32 = 4;
/// A sweep slower than this marks a slow PC. On a mid-range PC a sweep
/// takes about 6 s.
const SLOW_SWEEP: Duration = Duration::from_secs(8);

/// Whether a full pass asked for only to find players waits: the sweeps are
/// looking (`sweeping`), and it is still the first `SWEEP_FAST_FOR` after
/// the sign. Such a pass takes 15 to 20 s and the sweeps stop for it, so a
/// pass that starts just as the match becomes readable holds the lobby back
/// by several seconds (eight, in a replayed log of a game attached while it
/// was still starting). After the quick sweeps it runs, so a game left at a
/// menu still has its history and friends read within a couple of minutes.
fn player_pass_waits(since: Option<Duration>, sweeping: bool) -> bool {
    sweeping && since.is_some_and(|s| s < SWEEP_FAST_FOR)
}

/// Whether the queue just stopped: it ran at the last read and does not now.
/// A popped queue loads a match; a cancelled one leaves the party, which is
/// read and ends the sweeps.
fn queue_stopped(was: bool, now: bool) -> bool {
    was && !now
}

/// Finds every player object in `regions` and nothing else: the search a
/// full pass makes for players, without the history, the console, the
/// rules, Valve's replies or the friend records. `read` takes an address
/// and a buffer and returns the bytes read; outside tests it reads the
/// game. Pieces overlap as a full pass's do, so an object across two is
/// read whole. `pause` runs after every `THROTTLE_EVERY` bytes. Returns the
/// bytes read.
fn sweep_players(
    read: impl Fn(usize, &mut [u8]) -> usize,
    regions: &[Region],
    our_account: u32,
    mut pause: impl FnMut(),
    found: &mut Vec<(usize, MatchPlayer)>,
) -> usize {
    let mut buf = vec![0_u8; CHUNK];
    let mut bytes = 0_usize;
    let mut since_pause = 0_usize;
    for region in regions {
        let mut off = 0_usize;
        while off < region.size {
            let want = CHUNK.min(region.size - off);
            let got = read(region.base + off, &mut buf[..want]);
            found.extend(
                players::find_players(&buf[..got], Offsets::CURRENT, our_account)
                    .into_iter()
                    .map(|(at, player)| (region.base + off + at, player)),
            );
            bytes += got;
            since_pause += got;
            if since_pause >= THROTTLE_EVERY {
                since_pause = 0;
                pause();
            }
            if want <= OVERLAP || off + want >= region.size {
                break;
            }
            off += want - OVERLAP;
        }
    }
    // Overlapping pieces find an object twice.
    found.sort_by_key(|(at, _)| *at);
    found.dedup_by_key(|(at, _)| *at);
    bytes
}

/// Logs where the player objects a read of the whole game found sat, and how
/// many of those regions held players at the last read that found any. That
/// shows whether reading only those regions while a match loads would be
/// enough to find the new lobby. `before` is replaced by the regions found
/// now.
fn note_player_regions(how: &str, regions: &[Region], spots: &[usize], before: &mut Vec<Region>) {
    let now = regions_holding(regions, spots);
    if now.is_empty() {
        return;
    }
    let again = now
        .iter()
        .filter(|r| before.iter().any(|b| b.base == r.base))
        .count();
    let sizes: Vec<String> = now.iter().map(|r| format!("{} KB", r.size >> 10)).collect();
    info!(
        "Game memory: {how} found {} player objects in {} regions ({}), {again} of them holding players at the last read that found any",
        spots.len(),
        now.len(),
        sizes.join(", ")
    );
    *before = now;
}

/// The regions holding `spots`, once each, by base. `regions` is sorted by
/// base, as the process lists them.
fn regions_holding(regions: &[Region], spots: &[usize]) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    for &spot in spots {
        let i = regions.partition_point(|r| r.base <= spot);
        if let Some(r) = i.checked_sub(1).map(|i| regions[i])
            && spot < r.base + r.size
            && !out.contains(&r)
        {
            out.push(r);
        }
    }
    out.sort_by_key(|r| r.base);
    out
}

/// How long after a match ends the reader keeps looking for Valve's reply
/// for it (the metadata salt), a pass every `MIN_PASS_GAP`.
const REPLY_FOR: Duration = Duration::from_secs(300);

/// Whether the console's match marker asks for a full pass: only for a match
/// id not seen before, whose players and lobby object are new on the heap.
/// Losing the marker and finding the same one again is the game rewriting
/// its console buffer mid-match, not news; treating it as news runs a full
/// pass about every 50 s through the first minutes of a match, 13 to 22 s
/// of CPU each. A match's end asks for its own pass.
fn console_names_new_match(now: Option<u64>, seen: Option<u64>) -> bool {
    now.is_some_and(|id| seen.is_none_or(|seen| id > seen))
}

/// The match that is ending as the lobby empties: the newest the worker
/// knows of. Match ids only rise. The last end line alone is not enough:
/// once this match's start line has left memory, the last end line names
/// the match before it.
fn ending_match(started: Option<u64>, last_started: Option<u64>, last_end: Option<u64>) -> Option<u64> {
    [started, last_started, last_end].into_iter().flatten().max()
}

/// Whether a match is being played: the rules object reads "in progress"
/// (7) for the match the console last named, and no end line has closed it.
/// A rules object with no match named (a replay) is not one.
fn match_running(rules_state: Option<u32>, named: Option<u64>, last_end: Option<u64>) -> bool {
    rules_state == Some(STATE_IN_PROGRESS) && named.is_some_and(|m| last_end.is_none_or(|e| e < m))
}

/// Whether the local player is seated in a match, which drives the passes
/// for missing players and for the rules object, and the timers. Our object
/// off slot 0 is not enough: since the City Never Sleeps update the hideout
/// seats a party from slot 1, and a party of three would otherwise count as
/// a match with nine seats missing. Nor is a player count: a match read down
/// to our object alone is still a match while the rules object says one is
/// running.
fn seated_in_match(players: &[MatchPlayer], match_named: bool, running: bool) -> bool {
    players.iter().any(|p| p.is_us && p.slot > 0)
        && !party_shaped(players, match_named, running)
        && (running || players.len() > 2)
}

/// How long a match's players may be unreadable before the match counts as
/// over without an end line. Mid match the players can stop reading for
/// about a minute and then be found again by a pass (one gap measured
/// 67 s); dropping them at once would report the match as over.
const HOLD_LOBBY_FOR: Duration = Duration::from_secs(120);
/// The rules object's state while a match is played.
const STATE_IN_PROGRESS: u32 = 7;

/// Whether the last read players are kept although none read now. Only in
/// a match (the hideout party leaving memory means the queue popped, see
/// `seating_sweeps`), only while no end line names this match, never
/// once the rules object says the match is not being played, and for at
/// most `HOLD_LOBBY_FOR`: an end whose line is never read still ends.
fn hold_lobby(
    in_match: bool,
    current: Option<u64>,
    last_end: Option<u64>,
    rules_state: Option<u32>,
    gone_for: Duration,
) -> bool {
    in_match
        && current.is_some_and(|c| last_end.is_none_or(|e| e < c))
        && rules_state.is_none_or(|s| s == STATE_IN_PROGRESS)
        && gone_for < HOLD_LOBBY_FOR
}

/// What a player object read as, to tell when it last changed.
type Reading = (u32, u8, u8, u32, u32, u32, u32, u32, u32, Option<u32>);

fn reading_of(p: &MatchPlayer) -> Reading {
    (
        p.account_id,
        p.team,
        p.slot,
        p.hero_id,
        p.level,
        p.net_worth,
        p.kills,
        p.deaths,
        p.assists,
        p.game_time.map(f32::to_bits),
    )
}

/// An object's last reading, when that reading began, and when it was last
/// read at all, by address.
type Seen = HashMap<usize, (Reading, Instant, Instant)>;

/// How long an object no read has found is remembered.
const SEEN_FOR: Duration = Duration::from_secs(600);

/// Notes each object's reading, keeping when it last changed.
fn note_readings(seen: &mut Seen, found: &[(usize, MatchPlayer)], now: Instant) {
    for (at, p) in found {
        let r = reading_of(p);
        match seen.get_mut(at) {
            Some((old, _, last)) if *old == r => *last = now,
            Some(entry) => *entry = (r, now, now),
            None => {
                seen.insert(*at, (r, now, now));
            }
        }
    }
    seen.retain(|_, (_, _, last)| now.duration_since(*last) < SEEN_FOR);
}

/// A lobby this size is a match, where every seat holds one player.
const SEAT_LOBBY_MIN: usize = 10;

/// Two objects in one seat (team and slot) of a match: one is a player, the
/// other an object an earlier match left behind, frozen at its final
/// numbers. The one whose reading changed last is the player.
/// `match_leftover` cannot catch this case: it only knows the roster of the
/// match the console last named, and only once that match's end line is
/// read, so an earlier match's object sitting in a seat of the live match
/// passes it. Without this a seat can show two players for a whole match.
/// Only in a lobby of `SEAT_LOBBY_MIN` or more; a tie (both first read
/// together) keeps both until one moves. Returns how many were dropped.
fn drop_seat_doubles(found: &mut Vec<(usize, MatchPlayer)>, changed: impl Fn(usize) -> Option<Instant>) -> usize {
    if found.len() < SEAT_LOBBY_MIN {
        return 0;
    }
    let mut drop: Vec<usize> = Vec::new();
    for (at, p) in found.iter() {
        if p.slot == 0 {
            continue;
        }
        let mine = changed(*at);
        let newer = found.iter().any(|(other, q)| {
            other != at && q.team == p.team && q.slot == p.slot && changed(*other) > mine
        });
        if newer {
            drop.push(*at);
        }
    }
    found.retain(|(at, _)| !drop.contains(at));
    drop.len()
}

/// Whether the one player read is our own object left behind by a match
/// that is over. When a match ends the other players' objects can leave
/// memory while ours stays, frozen at its final numbers, so the lobby never
/// empties: no end is noticed, no reply looked for, and the hideout party
/// is never read. In a match the game time on our object moves on every
/// read, pauses included (see `rules`), so a lone object whose game time
/// has not moved is a leftover.
fn stale_lone_us(players: &[MatchPlayer], had_match: bool, last_time: Option<f32>) -> bool {
    match players {
        [us] => {
            had_match
                && us.is_us
                && us.game_time.is_some()
                && us.game_time == last_time
        }
        _ => false,
    }
}

/// Each player's (team, slot, lane, entity index, badge) as a log line: the
/// fields that place a player on the game's top bar.
fn top_bar_line(players: &[MatchPlayer]) -> String {
    format!(
        "Game memory: top bar (team, slot, lane, index, badge) {:?}",
        players
            .iter()
            .map(|p| (p.team, p.slot, p.lane, p.entity_index, p.rank_badge))
            .collect::<Vec<_>>()
    )
}

/// When the rejuvenator's credits were banked, on the match clock: the clock
/// at the read where the credits both teams hold left zero, kept while any
/// are held, dropped at zero. Either team's: the game's top bar shows the
/// timer to everyone. A buff already on at the first read has no known start
/// and gets no timer.
fn rejuv_claim(before: Option<u32>, now: u32, at: Option<f32>, clock: f32) -> Option<f32> {
    match (before, now) {
        (_, 0) => None,
        (Some(0), _) => Some(clock),
        _ => at,
    }
}

/// The game's own end of a match: its state goes from 7 (being played) to 8
/// with a winning team set, and the match is known. Anything else, including
/// a first read that finds a match already over, is not an end seen.
fn game_end_of(before: Option<u32>, state: u32, winner: Option<u8>, match_id: Option<u64>) -> Option<GameEnd> {
    match (before, state, winner, match_id) {
        (Some(7), 8, Some(winner), Some(match_id)) => Some(GameEnd { match_id, winner }),
        _ => None,
    }
}

/// The ban list at the rules object `at`, when it reads as one.
fn read_bans(game: &GameProcess, at: usize) -> Option<Vec<u32>> {
    let mut head = [0_u8; super::rules::BANS_BYTES];
    if game.read(at + super::rules::BANS_AT, &mut head) != head.len() {
        return None;
    }
    let (count, elements) = super::rules::parse_bans_header(&head)?;
    if count == 0 {
        return Some(Vec::new());
    }
    let mut ids = vec![0_u8; count * 4];
    (game.read(elements, &mut ids) == ids.len())
        .then(|| super::rules::parse_ban_ids(&ids, count))
        .flatten()
}

/// Whether a pass is wanted to catch Valve's reply for the match that just
/// ended: `waiting` holds that match's id and how long ago it ended, and no
/// reply for it has been read. The reply can arrive up to a minute or so
/// after the end (53 s in one measured match), so a single pass soon after
/// the end can miss it, and the game frees it again before the next
/// scheduled pass ten minutes on. Without its metadata salt, neither
/// DeadChaps nor deadlock-api can fetch the match's metadata from Valve
/// quickly. Never while the next match is on (`match_on`): a lobby that
/// empties and fills again within the window (a dodged or watched match)
/// would otherwise have the reader read the whole game every half minute
/// through the new match's lane phase.
fn reply_pass_wanted(waiting: Option<(u64, Duration)>, replied: &[u64], match_on: bool) -> bool {
    !match_on && waiting.is_some_and(|(match_id, gap)| gap < REPLY_FOR && !replied.contains(&match_id))
}

/// Whether a full pass runs now: the first for a game, then one every
/// `IDLE_PASS_EVERY`, or `MATCH_PASS_EVERY` in a fully read match, or sooner
/// when asked, but never within `pass_rest` of the last, which took `took`.
fn pass_due(since_last: Option<Duration>, asked: bool, in_match: bool, took: Option<Duration>) -> bool {
    let every = if in_match {
        MATCH_PASS_EVERY
    } else {
        IDLE_PASS_EVERY
    };
    since_last.is_none_or(|gap| gap >= every || (asked && gap >= pass_rest(took)))
}

/// The least time between a pass and the next one asked for, after a pass
/// that took `took` (see `SLOW_PASS`).
fn pass_rest(took: Option<Duration>) -> Duration {
    match took {
        Some(took) if took > SLOW_PASS => MIN_PASS_GAP.max(took * PASS_REST_RATIO),
        _ => MIN_PASS_GAP,
    }
}

/// The local player from the last read, to keep the lobby up when our object
/// is gone but the others' are not. The hideout's `kingme` command (souls
/// and max level) leaves the game with no recognisable object for the local
/// player until the hero is swapped, while the party members' objects stay
/// exactly where they were. So while every object still read belongs to the
/// lobby as last shown, ours is kept as it was; a lobby of new faces, or
/// none, is not held.
fn hold_us(
    raw: &[MatchPlayer],
    last_us: Option<&MatchPlayer>,
    previous: &[u32],
) -> Option<MatchPlayer> {
    if raw.is_empty() || raw.iter().any(|p| p.is_us) {
        return None;
    }
    let us = last_us?;
    if !previous.contains(&us.account_id) {
        return None;
    }
    raw.iter()
        .all(|p| previous.contains(&p.account_id))
        .then(|| us.clone())
}

/// Whether an end line's match is listed as ours: not when our player was
/// last read as a spectator (the spectators' team, or no seat). Read with no
/// player at all (the reader attached after the match) it is still listed,
/// by its id.
fn ours_to_list(us: Option<&MatchPlayer>) -> bool {
    us.is_none_or(|p| p.slot > 0 && matches!(p.team, 2 | 3))
}

/// The ended match as a row: the id from the end line, the numbers from our
/// player as last seen. Without a player (the reader attached after the
/// match) the row still carries the id, which is enough to look the match
/// up.
fn ended_match(
    end: MatchEnd,
    us: Option<&MatchPlayer>,
    now: u64,
    account_id: Option<u32>,
    modes: Option<(u32, u32)>,
) -> EndedMatch {
    EndedMatch {
        match_id: end.match_id,
        ended_unix: now,
        hero_id: us.map_or(0, |p| p.hero_id),
        kills: us.map_or(0, |p| p.kills),
        deaths: us.map_or(0, |p| p.deaths),
        assists: us.map_or(0, |p| p.assists),
        net_worth: us.map_or(0, |p| p.net_worth),
        level: us.map_or(0, |p| p.level),
        damage: us.and_then(|p| p.hero_damage),
        won: None,
        account_id,
        modes,
    }
}

fn newest_end(ends: &[MatchEnd]) -> Option<MatchEnd> {
    ends.iter().copied().max_by_key(|e| e.match_id)
}

/// Re-reads the match's players where a pass found them, so their souls and
/// K/D/A keep up between passes for a few kilobytes rather than another pass
/// over the whole game. A player no longer there is dropped, which is how the
/// end of a match empties the lobby. `read` takes an address and a buffer
/// and returns the bytes read; outside tests it reads the game.
fn reread_players(
    read: impl Fn(usize, &mut [u8]) -> usize,
    spots: &[usize],
    our_account: Option<u32>,
) -> Vec<(usize, MatchPlayer)> {
    let Some(account) = our_account else {
        return Vec::new();
    };
    let mut buf = vec![0_u8; players::OBJECT_BYTES];
    let mut out = Vec::new();
    for spot in spots {
        if read(*spot, &mut buf) < players::OBJECT_BYTES {
            continue;
        }
        if let Some(player) = players::player_at(&buf, 0, Offsets::CURRENT, account) {
            out.push((*spot, player));
        }
    }
    out
}

/// Reads each friend record at `spots` and the text it points at, and hands
/// what it found to the tracker: a record gone or no longer shaped like one is
/// handed as gone.
fn read_friend_records(
    read: impl Fn(usize, &mut [u8]) -> usize,
    spots: &[usize],
    tracker: &mut friends_presence::Tracker,
    now: Instant,
    now_unix: u64,
) {
    let mut record = [0_u8; friends_presence::RECORD_BYTES];
    let mut text = [0_u8; friends_presence::TEXT_BYTES];
    for &at in spots {
        let found = (read(at, &mut record) == record.len())
            .then(|| friends_presence::record_at(&record))
            .flatten()
            .and_then(|r| {
                if r.text_at == 0 {
                    return Some((r, String::new()));
                }
                let mut got = read(r.text_at, &mut text);
                if got < text.len() {
                    // A short text at the end of a page with nothing readable
                    // after it: read to the page's end.
                    let to_page = (0x1000 - r.text_at % 0x1000).min(text.len());
                    got = read(r.text_at, &mut text[..to_page]);
                }
                friends_presence::text_of(&text[..got]).map(|t| (r, t.to_owned()))
            });
        tracker.observe(at, found, now, now_unix);
    }
}

/// The match's players on a read that did not read the whole game: every
/// spot the last pass found, read again, plus any player the kept pieces
/// hold somewhere else. The kept pieces are the 4 MB around the history, the
/// console and the rich presence, re-read every `WATCH_EVERY`, and our own
/// object can sit in one of them. A player found there adds to the spots
/// rather than replacing them; otherwise, `WATCH_EVERY` after each pass, the
/// lobby would fall to our object alone until the next pass.
fn players_between_passes(
    read: impl Fn(usize, &mut [u8]) -> usize,
    spots: &[usize],
    in_pieces: Vec<(usize, MatchPlayer)>,
    our_account: Option<u32>,
) -> Vec<(usize, MatchPlayer)> {
    let mut out = reread_players(read, spots, our_account);
    for (at, player) in in_pieces {
        if !out.iter().any(|(spot, _)| *spot == at) {
            out.push((at, player));
        }
    }
    out
}

/// The worker thread's loop, one turn every `PLAYERS_EVERY` or sooner when a
/// setting changes. Each turn finds and opens the game, runs a full pass, a
/// re-read of the kept pieces or a sweep when one is due, re-reads the
/// players, the friend records and the rules object, and publishes what
/// changed.
fn run(
    shared: &Shared,
    exe: &str,
    hooks: &Hooks,
) {
    let Hooks { steam_account, on_change, on_salts, valve_has } = hooks;
    // Below the game: the reader's whole-game reads take seconds of CPU, and
    // on a PC with few cores they must wait for the game, not the game for
    // them, or the game stutters.
    below_normal_priority();
    // Replay addresses handed on this run, so each goes once.
    let mut salts_sent: Vec<String> = Vec::new();
    // Matches whose reply object was found, and the salt hits already logged.
    let mut replied: Vec<u64> = Vec::new();
    let mut hits_logged: Vec<usize> = Vec::new();
    let ended_path: &Path = &shared.ended_path;
    let mut game: Option<(u32, GameProcess)> = None;
    let mut hot: Vec<Region> = Vec::new();
    let mut want_pass = true;
    // A pass asked for only to find players (the game still starting, the
    // queue stopping, our player not read yet): it waits while the sweeps
    // look, which do that for a quarter of the cost, and runs once they stop.
    let mut want_for_players = false;
    // Why the next pass runs, for its log line.
    let mut pass_reason = "the game attached";
    // Whether the last pass kept pieces holding history or an end line.
    let mut hot_held_history = false;
    let mut last_pass: Option<Instant> = None;
    // How long the last full pass took, which spaces the next (`pass_rest`).
    let mut last_pass_took: Option<Duration> = None;
    let mut last_watch: Option<Instant> = None;
    // Where the last pass found the match's players.
    let mut player_spots: Vec<usize> = Vec::new();
    // The local player as last read, kept after the lobby goes, so the match
    // that just ended can carry their numbers.
    let mut last_us: Option<MatchPlayer> = None;
    // The other accounts seen in the lobby lately, and when.
    let mut watched: HashMap<u32, Instant> = HashMap::new();
    // Whether our player is being held from the last read (see `hold_us`).
    let mut holding = false;
    // The last sign that a lobby is about to be seated, when the last sweep
    // ended, and whether the last read was a match with seats unread (see
    // `seating_sweeps`).
    let mut seating_from: Option<Instant> = None;
    let mut last_sweep: Option<Instant> = None;
    // How long the last sweep took, which spaces the next on a slow PC
    // (`sweep_due`).
    let mut last_whole_took: Option<Duration> = None;
    let mut seated_short = false;
    // The regions the players last found sat in, so the log can say whether
    // the next lobby's objects land in the same ones.
    let mut player_regions: Vec<Region> = Vec::new();
    // When a match's players stopped reading while nothing said it ended
    // (see `hold_lobby`).
    let mut players_gone_at: Option<Instant> = None;
    // The rejuvenator credits as last read (team 2's, team 3's), and the
    // match clock the first of them was banked at.
    let mut rejuv_seen: Option<[u32; 2]> = None;
    // The game state and winner as last logged.
    let mut game_state_seen: Option<(u32, Option<u8>)> = None;
    let mut rejuv_at: Option<f32> = None;
    // Whether the lobby last read full was a match (more than a party), for
    // `stale_lone_us`.
    let mut had_match = false;
    // Objects a match left behind, by address and the game time frozen on
    // them: skipped while they still read that way, so a new object at the
    // same address is read as usual.
    let mut leftovers: Vec<(usize, f32)> = Vec::new();
    // The players of the match the console last named, as last read while
    // seated in it, with its id (see `match_leftover`), and how many of
    // them the last read left out, for the log.
    let mut match_roster: Option<(u64, Vec<MatchPlayer>)> = None;
    let mut left_out = 0_usize;
    // Each object's reading and when it changed (see `drop_seat_doubles`),
    // and how many the last read dropped, for the log.
    let mut seen: Seen = HashMap::new();
    let mut seat_doubles = 0_usize;
    // When this game was attached, and whether our player has been read in
    // it since (see `settling_pass_wanted`).
    let mut attached_at = Instant::now();
    let mut seen_us = false;
    // The game's rules object, from the last pass that found one.
    let mut rules_at: Option<usize> = None;
    // Passes this match that looked for the rules object and missed it.
    let mut rules_tries = 0_u32;
    // What this match has spent looking for missing players, in sweeps
    // (see `SWEEPS_PER_PASS`).
    let mut short_spent = 0_u32;
    let mut last_players_line = String::new();
    // The top bar fields (`top_bar_line`) as last logged.
    let mut last_bar_line = String::new();
    // How many players the kept pieces held between passes, and whether ours
    // was one, as last logged since the last pass.
    let mut last_in_pieces = (0_usize, false);
    // The match that just ended and when, until its reply is read (see
    // `reply_pass_wanted`).
    let mut awaiting_reply: Option<(u64, Instant)> = None;
    // The (match, metadata salt) pairs put to Valve when keying a reply the
    // game's history could not key, once per asking (see `key_by_endings`).
    let mut valve_asked: Vec<(u64, u32)> = Vec::new();
    // The last match the console named as begun. `match_started` is read
    // afresh every cycle and goes when the console line leaves memory, often
    // mid match; the match that then ends is still this one.
    let mut last_started: Option<u64> = None;
    // The game's friend records, what each read said, and when they were
    // last read; the counts as last logged.
    let mut friends = friends_presence::Tracker::default();
    let mut friends_read: Option<Instant> = None;
    let mut friends_logged = (0_usize, 0_usize);
    // Whether the wait for the game was logged, and whether it needed the
    // player's permission.
    let mut waiting_logged: Option<bool> = None;
    // The game process that last would not open, logged once.
    let mut open_failed_for: Option<u32> = None;

    let publish = |update: &dyn Fn(&mut MemoryView)| {
        let mut view = lock(&shared.view);
        let before = view.clone();
        update(&mut view);
        let changed = *view != before;
        drop(view);
        if changed {
            on_change();
        }
    };

    loop {
        let control = *lock(&shared.control);
        if control.stop {
            return;
        }
        publish(&|v| {
            v.enabled = control.enabled;
            v.allowed = control.allowed;
        });
        if !(control.enabled && control.allowed) {
            if game.take().is_some() {
                info!("Game memory: off, handle closed");
            }
            hot.clear();
            friends.clear();
            want_pass = true;
            pass_reason = "reader switched off";
            publish(&|v| {
                v.attached = false;
                v.entries.clear();
                v.last_match_end = None;
                v.friends_presence.clear();
            });
            shared.wait(Duration::from_secs(3600));
            continue;
        }

        let Some(pid) = game_pid(exe) else {
            if game.take().is_some() {
                info!("Game memory: {exe} exited");
            }
            hot.clear();
            friends.clear();
            want_pass = true;
            // Linux: whether the game's memory will open once it runs, so
            // the player can be asked for access before the game starts
            // (`process::granted`).
            let needed = !super::process::granted();
            if waiting_logged != Some(needed) {
                waiting_logged = Some(needed);
                if needed {
                    info!("Game memory: waiting for {exe}; reading it will need the player's OK (Allow)");
                } else {
                    info!("Game memory: waiting for {exe}");
                }
            }
            // Without the game nothing read from it is current; the
            // Companion falls back to deadlock-api.
            publish(&|v| {
                v.attached = false;
                v.access_needed = needed;
                v.friends_presence.clear();
            });
            pass_reason = "the game exited";
            shared.wait(GAME_CHECK_EVERY);
            continue;
        };
        if game.as_ref().is_none_or(|(open, _)| *open != pid) {
            match GameProcess::open(pid) {
                Some(process) => {
                    info!("Game memory: attached to {exe} (read-only)");
                    #[cfg(target_os = "linux")]
                    info!("Game memory: the game's memory: {}", process.layout());
                    waiting_logged = None;
                    open_failed_for = None;
                    publish(&|v| v.access_needed = false);
                    game = Some((pid, process));
                    hot.clear();
                    friends.clear();
                    // A new game: its first pass runs now, and the passes
                    // after it come quickly until our player is read.
                    last_pass = None;
                    attached_at = Instant::now();
                    seen_us = false;
                    rules_at = None;
                    // A game opened mid match, or still starting: its
                    // players are swept for until they turn up.
                    seating_from = Some(Instant::now());
                    last_sweep = None;
                }
                None => {
                    // Linux: the game runs but its memory would not open;
                    // the view says so, so the player can be asked to grant
                    // access.
                    let needed = super::process::access_needed();
                    if open_failed_for != Some(pid) {
                        open_failed_for = Some(pid);
                        info!("Game memory: {exe} is running but would not open (the player's OK needed: {needed})");
                    }
                    publish(&|v| {
                        v.attached = false;
                        v.access_needed = needed;
                    });
                    shared.wait(GAME_CHECK_EVERY);
                    continue;
                }
            }
        }
        let Some((_, process)) = game.as_ref() else {
            continue;
        };
        publish(&|v| v.attached = true);

        // Which account is ours, so the match's players can be told apart.
        let our_account = steam_account().and_then(|id| id.parse::<u32>().ok());
        let mut found = Found::default();
        let mut pass_ran = false;
        // A match read short (players gone from memory) keeps the shorter
        // idle cadence: the long one is only for a lobby that is fully read.
        let (in_match, full_match) = {
            let v = lock(&shared.view);
            let in_match = !v.match_players.is_empty() && !v.in_hideout();
            (in_match, in_match && missing_seats(&v.match_players) == 0)
        };
        let players_before = lock(&shared.view).match_players.len();
        let sweeping = our_account.is_some()
            && seating_sweeps(seating_from.map(|at| at.elapsed()), players_before, seated_short);
        // A pass for players that no longer waits does the looking itself:
        // no sweep runs beside it.
        let players_pass_asked =
            want_for_players && !player_pass_waits(seating_from.map(|at| at.elapsed()), sweeping);
        let asked = want_pass || players_pass_asked;
        if pass_due(last_pass.map(|at| at.elapsed()), asked, full_match, last_pass_took) {
            let reason_now = if asked { pass_reason } else { "the schedule" };
            let quiet = Background::begin();
            let started = Instant::now();
            let cpu_before = thread_cpu();
            let regions = process.private_regions();
            // Accounts seen in the lobby lately, ours among them: an object
            // of theirs turned away by the limits is logged with why.
            let mut watch: Vec<u32> = watched.keys().copied().collect();
            if let Some(account) = our_account
                && !watch.contains(&account)
            {
                watch.push(account);
            }
            // The lobby object for the match the console named, for its
            // modes, until they are known.
            let watch_lobby = {
                let view = lock(&shared.view);
                view.match_started
                    .filter(|_| view.match_modes.is_none())
                    .map(|s| (s.lobby_id, s.match_id))
            };
            read_pieces(
                process,
                &regions,
                true,
                our_account,
                &Watching {
                    accounts: &watch,
                    lobby: watch_lobby,
                },
                &mut found,
            );
            let spots: Vec<usize> = found.players.iter().map(|(at, _)| *at).collect();
            note_player_regions("the full pass", &regions, &spots, &mut player_regions);
            for hit in found.lobby_hits.drain(..) {
                if !hits_logged.contains(&hit.at) {
                    hits_logged.push(hit.at);
                    info!(
                        "Game memory: lobby object at {:#x}: match mode {:?}, game mode {:?}, words {:?}",
                        hit.at, hit.match_mode, hit.game_mode, hit.words
                    );
                }
                if let (Some(mm), Some(gm)) = (hit.match_mode, hit.game_mode) {
                    publish(&|v| v.match_modes = Some((mm, gm)));
                }
            }
            // Valve's replies, tied to their matches through the game's
            // history by start time. The salts are handed on as replay
            // addresses (`on_salts`), the same form a Steam cache file gives.
            if !found.replies.is_empty() {
                let known: Vec<HistoryEntry> = {
                    let view = lock(&shared.view);
                    found
                        .entries
                        .iter()
                        .chain(view.entries.iter())
                        .copied()
                        .collect()
                };
                let mut keyed: Vec<(u64, super::salts_probe::Reply, &str)> = Vec::new();
                let mut unkeyed: Vec<super::salts_probe::Reply> = Vec::new();
                for reply in found.replies.drain(..) {
                    match super::salts_probe::match_of(&reply, &known) {
                        Some(match_id) => keyed.push((match_id, reply, "the game's history")),
                        None => unkeyed.push(reply),
                    }
                }
                // A reply no history record ties to a match is logged, and
                // the matches that ended lately are offered as its key, each
                // checked against Valve's CDN (`valve_has`). Dropped, it would
                // leave the match with its replay salt alone.
                if !unkeyed.is_empty() {
                    info!(
                        "Game memory: {} of Valve's replies match no history record ({})",
                        unkeyed.len(),
                        unkeyed
                            .iter()
                            .map(|r| format!(
                                "{:#x} started {}, metadata salt {}",
                                r.at, r.start_time, r.metadata_salt
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    // Every match that ended lately, not only the one being
                    // waited on, which can turn out to be the match before.
                    let now = unix_now();
                    let rest: Vec<super::salts_probe::Ending> = {
                        let view = lock(&shared.view);
                        view.ended
                            .iter()
                            .map(|m| (m.match_id, m.ended_unix))
                            .chain(newest_end(&found.ends).map(|e| (e.match_id, now)))
                            .chain(last_started.map(|id| (id, now)))
                            .map(|(match_id, ended_unix)| super::salts_probe::Ending { match_id, ended_unix })
                            .collect()
                    };
                    let first = awaiting_reply.map(|(match_id, at)| super::salts_probe::Ending {
                        match_id,
                        ended_unix: now.saturating_sub(at.elapsed().as_secs()),
                    });
                    let done: Vec<u64> = replied
                        .iter()
                        .copied()
                        .chain(keyed.iter().map(|(id, _, _)| *id))
                        .collect();
                    let endings = super::salts_probe::endings_to_offer(first, &rest, &done);
                    for (match_id, reply) in
                        super::salts_probe::key_by_endings(&unkeyed, &endings, &mut valve_asked, &**valve_has)
                    {
                        keyed.push((match_id, reply, "a match that just ended, Valve agreeing"));
                    }
                }
                for (match_id, reply, how) in keyed {
                    if !replied.contains(&match_id) {
                        replied.push(match_id);
                    }
                    let url = super::salts_probe::url(match_id, reply.cluster, reply.metadata_salt);
                    if !salts_sent.contains(&url) && !found.salts_urls.contains(&url) {
                        info!(
                            "Game memory: Valve's reply for match {match_id} at {:#x}, keyed by {how}: cluster {}, metadata salt {}, replay salt {}",
                            reply.at, reply.cluster, reply.metadata_salt, reply.replay_salt
                        );
                        found.salts_urls.push(url);
                        // The replay salt is handed on too: replay salts are
                        // rarely available from any other source.
                        if reply.replay_salt != 0 {
                            let dem = super::salts_probe::dem_url(
                                match_id,
                                reply.cluster,
                                reply.replay_salt,
                            );
                            if !salts_sent.contains(&dem) && !found.salts_urls.contains(&dem) {
                                found.salts_urls.push(dem);
                            }
                        }
                    }
                }
            }
            drop(quiet);
            last_pass = Some(Instant::now());
            last_pass_took = Some(started.elapsed());
            pass_ran = true;
            hot_held_history = !found.entries.is_empty() || !found.ends.is_empty();
            if let Some(at) = found.rules.first() {
                if rules_at.is_none() {
                    info!("Game memory: rules object found (the timers' source)");
                }
                rules_at = Some(*at);
            } else if in_match {
                rules_tries += 1;
                let big: Vec<_> = process.too_big().into_iter().map(|r| r.size >> 20).collect();
                info!(
                    "Game memory: no rules object in this pass ({}); the bounds sat in {} place(s){}; regions too big to read: {}",
                    if rules_tries <= RULES_TRIES {
                        format!("{rules_tries} of {RULES_TRIES}")
                    } else {
                        format!("a retry, every {} s after {RULES_TRIES} tries", RULES_RETRY_EVERY.as_secs())
                    },
                    found.rules_misses.len(),
                    if found.rules_misses.is_empty() {
                        String::new()
                    } else {
                        format!(", turned away for: {}", found.rules_misses.join(" | "))
                    },
                    if big.is_empty() {
                        "none".to_owned()
                    } else {
                        big.iter().map(|mb| format!("{mb} MB")).collect::<Vec<_>>().join(", ")
                    }
                );
            }
            // A process still starting has nothing in it yet: look again
            // soon, not in ten minutes.
            want_pass = false;
            want_for_players = found.bytes < STILL_LOADING_BYTES;
            pass_reason = if want_for_players { "the game still loading" } else { "the schedule" };
            // What became of the accounts seen lately that this pass did not
            // list: an object turned away says why, or there was none.
            for account in &watch {
                if found.players.iter().any(|(_, p)| p.account_id == *account) {
                    continue;
                }
                // An id also sits in strings and the friends list; only an
                // object with a name in it is worth a line.
                if let Some(r) = found
                    .rejected
                    .iter()
                    .find(|r| r.account_id == *account && !r.why.contains("name "))
                {
                    tracing::debug!(
                        "Game memory: player object for {account} turned away: {}",
                        r.why
                    );
                }
            }
            // Linux only: when the local player is not read, log how many
            // places held our Steam id and why each was turned away, names
            // included, so a reader that finds the rules and the match end
            // but no players says why.
            if cfg!(target_os = "linux")
                && let Some(ours) = our_account
                && !found.players.iter().any(|(_, p)| p.account_id == ours)
            {
                let mine: Vec<&str> = found
                    .rejected
                    .iter()
                    .filter(|r| r.account_id == ours)
                    .map(|r| r.why.as_str())
                    .collect();
                info!(
                    "Game memory: our player not read: our Steam id in {} place(s) turned away{}",
                    mine.len(),
                    if mine.is_empty() { String::new() } else { format!(", first: {}", mine.iter().take(3).copied().collect::<Vec<_>>().join(" | ")) }
                );
            }
            found.rejected.clear();
            // The rich presence, when it reads differently from last time.
            found.presence.truncate(super::presence_probe::AT_MOST);
            let queueing = super::presence_probe::queueing(&found.presence);
            let now_unix = unix_now();
            let was_queueing = lock(&shared.view).queueing;
            publish(&|v| {
                v.queueing = queueing;
                if queueing {
                    v.queue_seen_unix = Some(now_unix);
                }
            });
            if was_queueing != queueing {
                // A queue popping means players and a lobby object to find
                // (the sweeps look first); one starting, a hideout to read
                // again.
                if queue_stopped(was_queueing, queueing) {
                    want_for_players = true;
                } else {
                    want_pass = true;
                }
                pass_reason = "the queue changed";
                info!("Game memory: queueing {queueing} (Steam rich presence)");
            }
            if queue_stopped(was_queueing, queueing) {
                seating_from = Some(Instant::now());
            }
            found.presence.clear();
            // Replay addresses found: handed on (`on_salts`), once each.
            for url in std::mem::take(&mut found.salts_urls) {
                if !salts_sent.contains(&url) {
                    info!("Game memory: replay address in memory: {url}");
                    on_salts(&url);
                    salts_sent.push(url);
                }
            }
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let mb = (found.bytes >> 20) as u64;
            info!(
                "Game memory: full pass read {mb} MB in {} regions in {ms} ms, {} ms CPU ({} ms reading, {} ms searching), {} history records, {} match ends, {} pieces kept; asked by: {}",
                regions.len(),
                thread_cpu().saturating_sub(cpu_before).as_millis(),
                found.read_time.as_millis(),
                found.search_time.as_millis(),
                found.entries.len(),
                found.ends.len(),
                found.hot.len(),
                reason_now
            );
            if !found.hit_regions.is_empty() {
                let sizes: Vec<String> = found
                    .hit_regions
                    .iter()
                    .map(|r| format!("{} KB", r.size >> 10))
                    .collect();
                let mut all: Vec<usize> = regions.iter().map(|r| r.size).collect();
                all.sort_unstable();
                let median = all.get(all.len() / 2).copied().unwrap_or(0);
                info!(
                    "Game memory: finds sat in regions of {}; the pass read {} regions, median {} KB, largest {} MB",
                    sizes.join(", "),
                    regions.len(),
                    median >> 10,
                    all.last().copied().unwrap_or(0) >> 20
                );
            }
            publish(&|v| {
                v.last_pass_unix = Some(unix_now());
                v.last_pass_ms = Some(ms);
                v.last_pass_mb = Some(mb);
            });
            hot = std::mem::take(&mut found.hot);
            last_watch = Some(Instant::now());
        } else if !hot.is_empty() && last_watch.is_none_or(|at| at.elapsed() >= WATCH_EVERY) {
            read_pieces(
                process,
                &hot,
                false,
                our_account,
                &Watching::default(),
                &mut found,
            );
            last_watch = Some(Instant::now());
            if watch_wants_pass(hot_held_history, found.entries.len(), found.ends.len(), in_match) {
                // The game freed or moved what was read before.
                want_pass = true;
                pass_reason = "the kept history went";
                hot_held_history = false;
            }
            // The queue flag, at most `WATCH_EVERY` behind rather than
            // waiting for a pass: the presence pieces are re-read with the
            // rest.
            if !found.presence.is_empty() {
                found.presence.truncate(super::presence_probe::AT_MOST);
                let queueing = super::presence_probe::queueing(&found.presence);
                let now_unix = unix_now();
                let was_queueing = lock(&shared.view).queueing;
                publish(&|v| {
                    v.queueing = queueing;
                    if queueing {
                        v.queue_seen_unix = Some(now_unix);
                    }
                });
                if was_queueing != queueing {
                    if queue_stopped(was_queueing, queueing) {
                        want_for_players = true;
                    } else {
                        want_pass = true;
                    }
                    pass_reason = "the queue changed";
                    // Logged here too: the kept pieces usually see the queue
                    // pop before a pass does.
                    info!("Game memory: queueing {queueing} (Steam rich presence, kept pieces)");
                }
                if queue_stopped(was_queueing, queueing) {
                    seating_from = Some(Instant::now());
                }
            }
            found.presence.clear();
        }

        // While a lobby is about to be seated, the players alone are looked
        // for over the whole game. A sweep costs a quarter of a full pass's
        // CPU at the same share of a core while it runs (see `SWEEP_PAUSE`),
        // so it can run far more often (`sweep_due`).
        let mut sweep_ran = false;
        if !pass_ran
            && !players_pass_asked
            && let Some(account) = our_account
            && sweep_due(
                seating_from.map(|at| at.elapsed()),
                players_before,
                seated_short,
                last_pass.max(last_sweep).map(|at| at.elapsed()),
                last_whole_took,
            )
        {
            let _quiet = Background::begin();
            let started = Instant::now();
            let cpu_before = thread_cpu();
            let regions = process.private_regions();
            let mut swept = Vec::new();
            let bytes = sweep_players(
                |a, b| process.read(a, b),
                &regions,
                account,
                || thread::sleep(SWEEP_PAUSE),
                &mut swept,
            );
            info!(
                "Game memory: seating sweep read {} MB in {} regions in {} ms, {} ms CPU, {} player objects, {} s after the sign",
                bytes >> 20,
                regions.len(),
                started.elapsed().as_millis(),
                thread_cpu().saturating_sub(cpu_before).as_millis(),
                swept.len(),
                seating_from.map_or(0, |at| at.elapsed().as_secs())
            );
            let spots: Vec<usize> = swept.iter().map(|(at, _)| *at).collect();
            note_player_regions("the sweep", &regions, &spots, &mut player_regions);
            // The sweep read every object there is, those in the kept
            // pieces among them.
            found.players = swept;
            last_whole_took = Some(started.elapsed());
            last_sweep = Some(Instant::now());
            sweep_ran = true;
            if players_before > 0 {
                short_spent += 1;
            }
        }

        // Friends' rich presence: the records a pass found, read again every
        // `PRESENCE_EVERY`. Only a record seen changing is believed.
        if pass_ran {
            let mut spots = std::mem::take(&mut found.friend_records);
            spots.sort_unstable();
            spots.dedup();
            spots.truncate(MAX_FRIEND_RECORDS);
            friends.keep_only(&spots);
            read_friend_records(|a, b| process.read(a, b), &spots, &mut friends, Instant::now(), unix_now());
            friends_read = Some(Instant::now());
        } else if friends_read.is_none_or(|at| at.elapsed() >= PRESENCE_EVERY) {
            let spots = friends.spots();
            read_friend_records(|a, b| process.read(a, b), &spots, &mut friends, Instant::now(), unix_now());
            friends_read = Some(Instant::now());
        }
        let friends_now = friends.live(Instant::now());
        // Counts only: logs can be shared for support, and what friends are
        // playing is their own business.
        let counts = (friends.counts(Instant::now()).0, friends_now.len());
        if counts != friends_logged && (pass_ran || counts.1 != friends_logged.1) {
            info!(
                "Game memory: {} friend records followed, {} friends' presence live",
                counts.0, counts.1
            );
            friends_logged = counts;
        }
        publish(&|v| v.friends_presence.clone_from(&friends_now));

        let entries = merge_entries(std::mem::take(&mut found.entries));
        // A full pass or a sweep read every object there is, so its finds
        // stand alone unless it found none. Any other read starts from the
        // pass's spots.
        if pass_ran {
            last_in_pieces = (0, false);
        }
        if !(pass_ran || sweep_ran) || found.players.is_empty() {
            let in_pieces = std::mem::take(&mut found.players);
            // Logged when it changes, at most once between passes.
            let held = (in_pieces.len(), in_pieces.iter().any(|(_, p)| p.is_us));
            if held.0 > 0 && held != last_in_pieces {
                info!(
                    "Game memory: the kept pieces hold {} player objects (ours among them: {}), the {} spots from the pass read as well",
                    held.0,
                    held.1,
                    player_spots.len()
                );
                last_in_pieces = held;
            }
            found.players =
                players_between_passes(|a, b| process.read(a, b), &player_spots, in_pieces, our_account);
        }
        // Which of a lane's two players the game's top bar shows first.
        for (at, player) in &mut found.players {
            player.entity_index = players::entity_index(|a, b| process.read(a, b), *at);
        }
        found.players.retain(|(at, p)| {
            !leftovers.iter().any(|(gone, time)| gone == at && p.game_time == Some(*time))
        });
        // A finished match's objects, still in memory at their final numbers,
        // once the console's end line says it is over: dropped here, before
        // the merge, so a friend's live hideout object is not swapped for
        // their match self. Not on the game's own end (rules state 7 to 8):
        // the end screen still reads the lobby, frozen, and
        // `MemoryView::party` covers the hideout read before the end line.
        let ended_now = newest_end(&found.ends)
            .map(|e| e.match_id)
            .max(lock(&shared.view).last_match_end.map(|e| e.match_id));
        let read_before = found.players.len();
        found
            .players
            .retain(|(_, p)| !match_leftover(p, match_roster.as_ref(), ended_now));
        let left_now = read_before - found.players.len();
        if left_now != left_out {
            if left_now > 0
                && let Some((id, _)) = &match_roster
            {
                info!("Game memory: {left_now} objects left over from match {id} not read as players");
            }
            left_out = left_now;
        }
        note_readings(&mut seen, &found.players, Instant::now());
        let doubles = drop_seat_doubles(&mut found.players, |at| seen.get(&at).map(|s| s.1));
        if doubles != seat_doubles {
            if doubles > 0 {
                info!("Game memory: {doubles} objects share a seat with a newer one, frozen since an earlier match: not read as players");
            }
            seat_doubles = doubles;
        }
        player_spots = found.players.iter().map(|(at, _)| *at).collect();
        let previous: Vec<u32> = lock(&shared.view)
            .match_players
            .iter()
            .map(|p| p.account_id)
            .collect();
        let mut raw: Vec<MatchPlayer> = found.players.iter().map(|(_, p)| p.clone()).collect();
        if let Some(us) = hold_us(&raw, last_us.as_ref(), &previous) {
            if !holding {
                info!(
                    "Game memory: our object is gone while the lobby stays (kingme?), keeping our last read"
                );
            }
            holding = true;
            raw.push(us);
        } else {
            holding = false;
        }
        let mut match_players = players::merge_players(raw);
        if stale_lone_us(&match_players, had_match, last_us.as_ref().and_then(|u| u.game_time)) {
            info!("Game memory: only our own object is left and its clock has stopped: the match is over");
            if let (Some((at, _)), Some(time)) =
                (found.players.iter().find(|(_, p)| p.is_us), match_players[0].game_time)
            {
                leftovers.push((*at, time));
                if leftovers.len() > 8 {
                    leftovers.remove(0);
                }
            }
            match_players.clear();
        }
        if match_players.len() > 2 {
            had_match = true;
        } else if match_players.is_empty() {
            had_match = false;
            rules_tries = 0;
            short_spent = 0;
        }
        let rules_now = rules_at.and_then(|at| {
            let mut b = vec![0_u8; super::rules::READ_BYTES];
            (process.read(at, &mut b) == super::rules::READ_BYTES)
                .then(|| super::rules::parse(&b))
                .flatten()
        });
        if rules_at.is_some() && rules_now.is_none() {
            // Freed or moved: the next pass finds it again.
            rules_at = None;
        }
        // The draft's bans, read with the rules (a few bytes) and kept per
        // match.
        if let Some(bans) = rules_at.filter(|_| rules_now.is_some()).and_then(|at| read_bans(process, at))
            && !bans.is_empty()
        {
            let (match_id, held) = {
                let view = lock(&shared.view);
                (view.match_started.map(|m| m.match_id).or(last_started), view.match_bans.clone())
            };
            if let Some(match_id) = match_id
                && held.as_ref() != Some(&(match_id, bans.clone()))
            {
                info!("Game memory: bans {bans:?} (match {match_id})");
                publish(&|v| v.match_bans = Some((match_id, bans.clone())));
            }
        }
        if !previous.is_empty() && match_players.is_empty() {
            let (in_match, current, ended) = {
                let view = lock(&shared.view);
                (
                    !view.in_hideout(),
                    view.match_started.map(|m| m.match_id).or(last_started),
                    view.last_match_end.map(|m| m.match_id),
                )
            };
            let first = players_gone_at.is_none();
            let gone_for = players_gone_at.get_or_insert_with(Instant::now).elapsed();
            if hold_lobby(in_match, current, ended, rules_now.map(|r| r.state), gone_for) {
                if first {
                    info!(
                        "Game memory: the players stopped reading mid match (rules state {:?}), keeping the last read for up to {} s",
                        rules_now.map(|r| r.state),
                        HOLD_LOBBY_FOR.as_secs()
                    );
                }
                match_players = lock(&shared.view).match_players.clone();
                want_pass = true;
                pass_reason = "players went";
            }
        } else {
            players_gone_at = None;
        }
        if !previous.is_empty() && match_players.is_empty() {
            players_gone_at = None;
            info!(
                "Game memory: the lobby emptied, sweeping for the next players for up to {} s",
                SEATING_FOR.as_secs()
            );
            seating_from = Some(Instant::now());
            // A match's lobby emptying is that match ending: its reply comes
            // in the next minute or so, and is freed again before long.
            let ended = {
                let view = lock(&shared.view);
                (!view.in_hideout())
                    .then(|| {
                        ending_match(
                            view.match_started.map(|m| m.match_id),
                            last_started,
                            view.last_match_end.map(|m| m.match_id),
                        )
                    })
                    .flatten()
            };
            if let Some(match_id) = ended
                && !replied.contains(&match_id)
            {
                info!(
                    "Game memory: match {match_id} is over, looking for Valve's reply every {} s for {} s",
                    MIN_PASS_GAP.as_secs(),
                    REPLY_FOR.as_secs()
                );
                awaiting_reply = Some((match_id, Instant::now()));
            }
        }
        if match_players.iter().any(|p| p.is_us) {
            seen_us = true;
        }
        // The timers: the rules re-read (a few hundred bytes) with the
        // players, and our controller's game time, while a match is played.
        let (named, running) = {
            let view = lock(&shared.view);
            (
                view.match_started.map(|m| m.match_id),
                match_running(
                    rules_now.map(|r| r.state),
                    view.match_started.map(|m| m.match_id).or(last_started),
                    view.last_match_end.map(|m| m.match_id),
                ),
            )
        };
        let match_named = named.is_some();
        let seated = seated_in_match(&match_players, match_named, running);
        // Only once the console names the match: a loading match is seated
        // before that, and `last_started` then still names the last one.
        if seated && let Some(id) = named {
            match_roster = Some((id, match_players.clone()));
        }
        let waiting = awaiting_reply.map(|(id, at)| (id, at.elapsed()));
        if reply_pass_wanted(waiting, &replied, seated || running) {
            want_pass = true;
            pass_reason = "waiting on Valve's reply";
        } else if waiting.is_some_and(|(_, gap)| gap >= REPLY_FOR)
            && let Some((match_id, _)) = awaiting_reply.take()
            && !replied.contains(&match_id)
        {
            info!("Game memory: no reply for match {match_id} read in {} s", REPLY_FOR.as_secs());
        }
        if rules_pass_wanted(seated, rules_at.is_some(), rules_tries, last_pass.map(|at| at.elapsed())) {
            want_pass = true;
            pass_reason = "the timers' rules object not found yet";
        }
        let missing = missing_seats(&match_players);
        if (pass_ran || sweep_ran) && seated && missing > 0 {
            if pass_ran {
                short_spent += SWEEPS_PER_PASS;
            }
            info!(
                "Game memory: {missing} seats unread after this {} ({} of {SHORT_TRIES} tries spent)",
                if pass_ran { "pass" } else { "sweep" },
                short_tries(short_spent)
            );
        }
        if short_pass_wanted(seated, missing, short_tries(short_spent)) {
            // A pass for players: the sweeps go first while they run.
            want_for_players = true;
            pass_reason = "players missing from the lobby";
        }
        seated_short = seated && missing > 0 && short_spent < SHORT_TRIES * SWEEPS_PER_PASS;
        if settling_pass_wanted(attached_at.elapsed(), seen_us) {
            want_for_players = true;
            pass_reason = "our player not found yet";
        }
        // The game's own end signal, logged and published whether the local
        // player is seated or watching from a spectator slot, so a
        // spectator's reader sees matches end too.
        if let Some(r) = rules_now {
            let now = (r.state, r.winner);
            if game_state_seen != Some(now) {
                let (match_id, us) = {
                    let view = lock(&shared.view);
                    (
                        view.match_started.map(|m| m.match_id).or(last_started),
                        match_players.iter().find(|p| p.is_us).map(|p| (p.team, p.slot)),
                    )
                };
                info!(
                    "Game memory: game state {:?} -> {} winner {:?} (match {match_id:?}, us {}, game start {:.1})",
                    game_state_seen.map(|s| s.0),
                    r.state,
                    r.winner,
                    us.map_or("not seated".to_owned(), |(team, slot)| format!("team {team} slot {slot}")),
                    r.game_start
                );
                if let Some(end) = game_end_of(game_state_seen.map(|s| s.0), r.state, r.winner, match_id) {
                    info!("Game memory: the game ended match {} (winner team {})", end.match_id, end.winner);
                    publish(&|v| v.game_end = Some(end));
                }
                game_state_seen = Some(now);
            }
        }
        let our_time = match_players.iter().find(|p| p.is_us).and_then(|p| p.game_time);
        let clock_now = rules_at.filter(|_| rules_now.is_some()).and_then(|at| {
            let mut b = [0_u8; super::rules::CLOCK_BYTES];
            (process.read(at + super::rules::CLOCK_AT, &mut b) == b.len())
                .then(|| super::rules::parse_clock(&b))
                .flatten()
        });
        if let (true, Some(r), Some(now)) = (seated, rules_now, our_time) {
            let clock = super::rules::match_clock(now, r.game_start, clock_now).0;
            if rejuv_seen != Some(r.rejuv) {
                let team = match_players.iter().find(|p| p.is_us).map(|p| p.team);
                info!(
                    "Game memory: rejuvenator credits (amber, sapphire) {:?} -> {:?} at match clock {clock:.1} (boss kills {}, our team {team:?})",
                    rejuv_seen, r.rejuv, r.boss_kills
                );
            }
            let held_before = rejuv_seen.map(|seen| seen.iter().sum());
            rejuv_at = rejuv_claim(held_before, r.rejuv_held(), rejuv_at, clock);
            rejuv_seen = Some(r.rejuv);
        }
        // A read that misses the rules keeps the rejuvenator timer: the next
        // read that finds them says whether the buff is still on.
        // The timers only while the game says the match is being played: a
        // finished match's rules object stands still at its last clock, and
        // the timers would stay frozen after the match.
        let objectives = match (seated && running, rules_now, our_time) {
            (true, Some(r), Some(now)) => Some(Objectives {
                clock: super::rules::match_clock(now, r.game_start, clock_now).0,
                paused: super::rules::match_clock(now, r.game_start, clock_now).1,
                game_start: r.game_start,
                now,
                boss_kills: r.boss_kills,
                next_boss: r.next_boss,
                rejuv_claimed: rejuv_at,
                read_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            }),
            _ => None,
        };
        for p in match_players.iter().filter(|p| !p.is_us) {
            watched.insert(p.account_id, Instant::now());
        }
        watched.retain(|_, seen| seen.elapsed() < WATCH_FOR);
        // Longest first: a player's own history is the longest list the game
        // holds, and the Companion keeps whichever run matches the account's
        // known history.
        let mut runs = std::mem::take(&mut found.runs);
        runs.sort_by_key(|run| std::cmp::Reverse(run.len()));
        runs.truncate(KEEP_RUNS);
        for run in &mut runs {
            run.truncate(KEEP_ENTRIES);
        }
        if let Some(us) = match_players.iter().find(|p| p.is_us) {
            last_us = Some(us.clone());
        }
        let end = newest_end(&found.ends);
        let start = newest_end(&found.starts);
        let previous_end = lock(&shared.view).last_match_end;
        let mut just_ended: Option<EndedMatch> = None;
        if end.is_some_and(|e| previous_end.is_none_or(|p| e.match_id > p.match_id)) {
            if let Some(e) = end {
                info!("Game memory: match {} ended", e.match_id);
                // The modes as the lobby object held them: the publish below
                // forgets them with the match, so they are read first.
                let modes = lock(&shared.view).match_modes;
                if ours_to_list(last_us.as_ref()) {
                    just_ended = Some(ended_match(e, last_us.as_ref(), unix_now(), our_account, modes));
                } else {
                    info!("Game memory: match {} was watched, not played: not listed", e.match_id);
                }
            }
            // A match that just ended may land in a history list the game
            // builds afterwards, so look again soon.
            want_pass = true;
            pass_reason = "a match ended";
            // Its end line names it for certain, so wait on its reply
            // whatever was being waited on: the lobby emptying can name no
            // match, or the wrong one, and a reply not looked for soon is
            // only found by a later scheduled pass, if at all.
            if let Some(e) = end
                && !replied.contains(&e.match_id)
                && awaiting_reply.is_none_or(|(id, _)| id != e.match_id)
            {
                awaiting_reply = Some((e.match_id, Instant::now()));
            }
        }
        // What the lobby holds, for telling the hideout from a match. Logged
        // when the line itself changes: the players compare unequal on every
        // read (their game time moves), which would log the same line every
        // 5 s.
        let players_line = {
            let us = match_players.iter().find(|p| p.is_us);
            format!(
                "Game memory: {} players read, (team, slot, account, level) {:?}, ours {}",
                match_players.len(),
                match_players
                    .iter()
                    .map(|p| (p.team, p.slot, p.account_id, p.level))
                    .collect::<Vec<_>>(),
                us.map_or("absent".to_owned(), |p| format!(
                    "team {} slot {} hero {} level {} {}/{}/{}",
                    p.team, p.slot, p.hero_id, p.level, p.kills, p.deaths, p.assists
                )),
            )
        };
        if players_line != last_players_line {
            info!("{players_line}");
            last_players_line = players_line;
        }
        // The top bar fields, logged when any of them changes, so a match's
        // log shows which one a game update broke.
        let bar_line = top_bar_line(&match_players);
        if bar_line != last_bar_line {
            info!("{bar_line}");
            last_bar_line = bar_line;
        }
        let steam_id = steam_account();
        publish(&|v| {
            v.objectives = objectives;
            v.match_running = running;
            if !entries.is_empty() {
                // Keep what was already known about older matches when a
                // smaller list is read.
                let mut merged = entries.clone();
                merged.extend(v.entries.iter().copied());
                v.entries = merge_entries(merged);
            }
            if let Some(e) = end
                && v.last_match_end.is_none_or(|p| e.match_id > p.match_id)
            {
                v.last_match_end = Some(e);
            }
            // The newest match begun and not yet destroyed is the one the
            // game is in; after the end line it is over.
            let started =
                start.filter(|s| v.last_match_end.is_none_or(|e| s.match_id > e.match_id));
            if started.map(|s| s.match_id) != v.match_started.map(|s| s.match_id) {
                v.match_modes = None;
            }
            v.match_started = started;
            if v.steam_id != steam_id {
                // A different Steam account: what was read belongs to the last one.
                v.entries = entries.clone();
                v.steam_id.clone_from(&steam_id);
            }
            v.match_players.clone_from(&match_players);
            v.match_roster.clone_from(&match_roster);
            if !runs.is_empty() {
                v.runs.clone_from(&runs);
            }
            if let Some(ended) = just_ended
                && !v.ended.iter().any(|m| m.match_id == ended.match_id)
            {
                v.ended.insert(0, ended);
                v.ended.truncate(KEEP_ENDED);
                crate::store::save(ended_path, &v.ended);
            }
        });
        let started_now = lock(&shared.view).match_started.map(|s| s.match_id);
        if console_names_new_match(started_now, last_started) {
            want_pass = true;
            pass_reason = "the console named a match";
            // Its players are being seated, if they are not read yet.
            seating_from = Some(Instant::now());
        }
        if let Some(id) = started_now {
            last_started = Some(id);
        }
        shared.wait(PLAYERS_EVERY);
    }
}

#[cfg(test)]
mod seating_replay;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::KEEP_ENDED_FOR;

    #[test]
    fn a_seat_read_twice_keeps_the_object_that_moved() {
        // Slot 4 of team 2 holds the last match's player, frozen at level 31,
        // and the new one, whose reading last changed minutes later.
        let t0 = Instant::now();
        let new_at = t0 + Duration::from_secs(260);
        let mut found: Vec<(usize, MatchPlayer)> = (1..=12)
            .map(|s| (0x1000 + s as usize, seated(100 + u32::from(s), if s <= 6 { 2 } else { 3 }, s, s == 12)))
            .collect();
        let mut stale = seated(259_753_003, 2, 4, false);
        stale.level = 31;
        found.push((0x9000, stale));
        let changed = |at: usize| Some(if at == 0x9000 { t0 } else { new_at });
        assert_eq!(drop_seat_doubles(&mut found, changed), 1);
        assert_eq!(found.len(), 12);
        assert!(found.iter().all(|(at, _)| *at != 0x9000), "the frozen one goes");
        assert!(found.iter().any(|(_, p)| p.account_id == 104 && p.slot == 4), "the new one stays");

        // Both first read together: neither known to be older, both kept.
        let mut tie = found.clone();
        tie.push((0x9000, seated(259_753_003, 2, 4, false)));
        assert_eq!(drop_seat_doubles(&mut tie, |_| Some(t0)), 0);
        assert_eq!(tie.len(), 13);

        // A small lobby (a party, a custom filling): never judged.
        let mut party = vec![(1, seated(1, 2, 1, true)), (2, seated(2, 2, 1, false))];
        assert_eq!(drop_seat_doubles(&mut party, |at| Some(if at == 1 { new_at } else { t0 })), 0);

        // Slot 0 (still loading) is no seat.
        let mut loading = found.clone();
        loading.push((0x9100, seated(7, 2, 0, false)));
        loading.push((0x9200, seated(8, 2, 0, false)));
        assert_eq!(drop_seat_doubles(&mut loading, |at| Some(if at == 0x9100 { t0 } else { new_at })), 0);
    }

    #[test]
    fn a_reading_keeps_the_time_it_last_changed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(5);
        let t2 = t0 + Duration::from_secs(10);
        let mut seen: Seen = HashMap::new();
        let mut p = seated(1, 2, 4, false);
        note_readings(&mut seen, &[(0x10, p.clone())], t0);
        note_readings(&mut seen, &[(0x10, p.clone())], t1);
        assert_eq!(seen[&0x10].1, t0, "unchanged: still changed at t0");
        p.net_worth = 50;
        note_readings(&mut seen, &[(0x10, p.clone())], t2);
        assert_eq!(seen[&0x10].1, t2, "moved: changed now");
        // Forgotten once no read has found it for SEEN_FOR.
        note_readings(&mut seen, &[], t2 + SEEN_FOR);
        assert!(seen.is_empty());
    }

    fn ended(match_id: u64, ended_unix: u64) -> EndedMatch {
        EndedMatch {
            match_id,
            ended_unix,
            hero_id: 52,
            kills: 8,
            deaths: 4,
            assists: 6,
            net_worth: 48_000,
            level: 20,
            damage: None,
            won: None,
            account_id: None,
            modes: None,
        }
    }

    /// Saved by one run, read by the next: the recent matches are still
    /// listed, the stale one is not, and the newest is first.
    #[test]
    fn ended_matches_outlive_the_app() {
        let dir = std::env::temp_dir().join(format!("ended-{}", std::process::id()));
        let path = dir.join("ended.json");
        let now = 1_789_300_000;
        let saved = vec![
            ended(105_306_480, now - 3600),
            ended(105_323_120, now - 600),
            ended(100_000_000, now - KEEP_ENDED_FOR - 1),
        ];
        crate::store::save(&path, &saved);
        let loaded = load_ended(&path, now);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            loaded.iter().map(|m| m.match_id).collect::<Vec<_>>(),
            vec![105_323_120, 105_306_480]
        );
        assert_eq!(loaded[1], saved[0]);
        assert!(load_ended(&dir.join("missing.json"), now).is_empty());
    }

    fn entry(match_id: u64, start_time: u32) -> HistoryEntry {
        HistoryEntry {
            match_id,
            hero_id: 7,
            duration_s: 1000,
            start_time,
            winning_team: 0,
            player_team: 0,
            kills: 1,
            deaths: 1,
            assists: 1,
            last_hits: 1,
            denies: 0,
            hero_level: 10,
            net_worth: 1000,
            match_mode: 1,
            game_mode: 1,
            outcome: 1,
        }
    }

    #[test]
    fn copies_of_the_history_merge_newest_first() {
        let merged = merge_entries(vec![
            entry(2, 200),
            entry(3, 300),
            entry(2, 200),
            entry(1, 100),
            entry(3, 300),
        ]);
        assert_eq!(
            merged.iter().map(|e| e.match_id).collect::<Vec<_>>(),
            [3, 2, 1]
        );
        let many: Vec<_> = (0..100).map(|i| entry(i, 1000 + i as u32)).collect();
        assert_eq!(merge_entries(many).len(), KEEP_ENTRIES);
    }

    #[test]
    fn one_look_at_the_process_list_answers_only_for_its_own_name() {
        let me = std::env::current_exe().expect("the test's own exe");
        let mut name = me.file_name().and_then(|n| n.to_str()).expect("a name").to_owned();
        // Linux names a process by its first 15 characters (comm); the
        // game's names, deadlock.exe and project8.exe, are 12.
        if cfg!(target_os = "linux") {
            name.truncate(15);
        }
        assert_eq!(game_pid("no-such-game-here.exe"), None);
        assert_eq!(
            game_pid(&name),
            Some(std::process::id()),
            "another name straight after is looked up, not answered from the last look"
        );
        let started = Instant::now();
        for _ in 0..50 {
            assert_eq!(game_pid(&name), Some(std::process::id()));
        }
        assert!(started.elapsed() < Duration::from_millis(50), "asked again within the window, the list is not walked again");
    }

    #[test]
    fn short_lobbies_are_read_again() {
        let lobby = |seats: &[(u8, u8)]| -> Vec<MatchPlayer> {
            seats.iter().enumerate().map(|(i, &(team, slot))| seated(100 + i as u32, team, slot, i == 0)).collect()
        };
        // A 6v6 read while loading: slots 9 and 12 unread.
        let ten = lobby(&[(2, 1), (2, 2), (2, 3), (2, 4), (2, 5), (2, 6), (3, 7), (3, 8), (3, 10), (3, 11)]);
        assert_eq!(missing_seats(&ten), 2);
        let twelve: Vec<(u8, u8)> = (1..=12).map(|s| (if s <= 6 { 2 } else { 3 }, s)).collect();
        assert_eq!(missing_seats(&lobby(&twelve)), 0);
        // Street Brawl in full, then with slot 2 unread.
        let brawl: Vec<(u8, u8)> = (1..=8).map(|s| (if s <= 4 { 2 } else { 3 }, s)).collect();
        assert_eq!(missing_seats(&lobby(&brawl)), 0, "8 is a full Street Brawl");
        assert_eq!(missing_seats(&lobby(&[(2, 1), (2, 3), (2, 4), (3, 5), (3, 6), (3, 7), (3, 8)])), 1);
        // Seats that fit either mode are short in both.
        assert!(missing_seats(&lobby(&[(2, 1), (2, 2), (2, 3), (2, 4), (3, 7), (3, 8)])) > 0);
        assert_eq!(missing_seats(&lobby(&[(2, 0), (2, 0)])), 0, "the hideout seats nobody");
        assert!(short_pass_wanted(true, 2, 0));
        assert!(short_pass_wanted(true, 2, SHORT_TRIES - 1), "one try left");
        assert!(!short_pass_wanted(true, 2, SHORT_TRIES), "a player who left: no storm");
        assert!(!short_pass_wanted(true, 0, 0), "a full lobby");
        assert!(!short_pass_wanted(false, 2, 0), "not seated");
    }

    #[test]
    fn a_full_pass_runs_first_then_on_events_or_seldom() {
        let secs = Duration::from_secs;
        assert!(pass_due(None, false, false, None), "a new game's first pass");
        assert!(reply_pass_wanted(Some((105_808_982, secs(20))), &[], false), "the match just ended");
        assert!(!reply_pass_wanted(Some((105_808_982, secs(20))), &[105_808_982], false), "its reply is read");
        assert!(!reply_pass_wanted(Some((105_808_982, secs(300))), &[], false), "gave up after five minutes");
        assert!(!reply_pass_wanted(None, &[], false), "no match ended");
        assert!(
            !reply_pass_wanted(Some((117_534_730, secs(225))), &[], true),
            "the next match is on: its lane phase is not read through"
        );
        // Players gone mid match, rules in progress, no end line: held while
        // they are gone (67 s in this case)...
        let s = |n: u64| Duration::from_secs(n);
        assert!(hold_lobby(true, Some(106_959_309), Some(106_953_803), Some(7), s(0)));
        assert!(hold_lobby(true, Some(106_959_309), Some(106_953_803), None, s(67)));
        // ...but not past the cap, not once its end line is read, not when
        // the rules say the match is not being played, not in the hideout,
        // and not with no match known.
        assert!(!hold_lobby(true, Some(106_959_309), Some(106_953_803), Some(7), s(120)));
        assert!(!hold_lobby(true, Some(106_959_309), Some(106_959_309), Some(7), s(0)));
        assert!(!hold_lobby(true, Some(106_959_309), None, Some(8), s(0)));
        assert!(!hold_lobby(false, Some(106_959_309), None, Some(7), s(0)));
        assert!(!hold_lobby(true, None, Some(106_953_803), Some(7), s(0)));
        // The match's start line has left memory by its end, and the last
        // end line is the match before.
        assert_eq!(ending_match(None, Some(106_926_374), Some(106_921_001)), Some(106_926_374));
        assert_eq!(ending_match(Some(7), Some(7), Some(5)), Some(7));
        assert_eq!(ending_match(None, None, None), None);
        let last = Some(secs(40));
        assert!(rules_pass_wanted(true, false, 0, last), "seated, no timers yet");
        assert!(rules_pass_wanted(true, false, RULES_TRIES - 1, last), "one try left");
        assert!(!rules_pass_wanted(true, false, RULES_TRIES, last), "out of quick tries: no storm if it never turns up");
        assert!(
            rules_pass_wanted(true, false, RULES_TRIES + 3, Some(RULES_RETRY_EVERY)),
            "but looked for again every five minutes for the rest of the match"
        );
        assert!(!rules_pass_wanted(true, true, 0, last), "already held");
        assert!(!rules_pass_wanted(false, false, 0, last), "not seated: the hideout or a menu");
        assert!(settling_pass_wanted(secs(8), false), "the main menu, nobody read yet");
        assert!(settling_pass_wanted(secs(599), false), "still looking");
        assert!(!settling_pass_wanted(secs(600), false), "gave up: the idle passes take over");
        assert!(!settling_pass_wanted(secs(30), true), "our player was read");
        assert!(
            !pass_due(Some(secs(120)), false, false, None),
            "two minutes is not a reason"
        );
        assert!(
            pass_due(Some(secs(600)), false, false, None),
            "ten minutes out of a match"
        );
        assert!(
            !pass_due(Some(secs(600)), false, true, None),
            "in a match the hot pieces carry it"
        );
        assert!(pass_due(Some(secs(1800)), false, true, None));
        assert!(!pass_due(Some(secs(29)), true, true, None), "asked, but too soon");
        assert!(
            pass_due(Some(secs(30)), true, true, None),
            "asked: a queue, a match named, a match ended"
        );
        // A mid-range PC's pass (15 to 20 s) keeps the 30 s rest ...
        assert!(pass_due(Some(secs(30)), true, true, Some(secs(18))));
        // ... and a PC short of memory, where a pass took 41 s, rests 164 s.
        assert_eq!(pass_rest(Some(secs(41))), secs(164));
        assert!(!pass_due(Some(secs(31)), true, true, Some(secs(41))), "the next pass of the lane phase storm");
        assert!(pass_due(Some(secs(164)), true, true, Some(secs(41))));
        // The schedule is untouched by the rest.
        assert!(pass_due(Some(secs(1800)), false, true, Some(secs(52))));
    }

    #[test]
    fn sweeps_rest_as_long_as_they_take() {
        let secs = Duration::from_secs;
        // a mid-range PC: a sweep of 9.5 GB takes about 6 s, spaced by the floors
        assert!(sweep_due(Some(secs(20)), 0, false, Some(secs(4)), Some(secs(6))));
        assert!(sweep_due(Some(secs(120)), 0, false, Some(secs(25)), Some(secs(6))));
        // a slow PC: 15 s a sweep, so 15 s apart while the lobby comes...
        assert!(!sweep_due(Some(secs(30)), 0, false, Some(secs(14)), Some(secs(15))));
        assert!(sweep_due(Some(secs(30)), 0, false, Some(secs(15)), Some(secs(15))));
        // ...and a minute apart after: busy a fifth of the time at most
        assert!(!sweep_due(Some(secs(120)), 0, false, Some(secs(59)), Some(secs(15))));
        assert!(sweep_due(Some(secs(120)), 0, false, Some(secs(60)), Some(secs(15))));
        // a quick read keeps the floors
        assert!(!sweep_due(Some(secs(20)), 0, false, Some(secs(3)), Some(secs(1))));
        assert!(!sweep_due(Some(secs(120)), 0, false, Some(secs(24)), Some(secs(1))));
    }

    #[test]
    fn a_lobby_about_to_be_seated_is_swept_for_quickly_then_seldom_then_not() {
        let secs = Duration::from_secs;
        assert!(seating_sweeps(Some(secs(0)), 0, false), "the lobby just emptied");
        assert!(seating_sweeps(Some(secs(239)), 0, false), "still seating");
        assert!(!seating_sweeps(Some(secs(240)), 0, false), "gave up: no match came");
        assert!(!seating_sweeps(Some(secs(10)), 12, false), "the match is seated");
        assert!(!seating_sweeps(Some(secs(10)), 2, false), "a party in the hideout: the queue was cancelled");
        assert!(!seating_sweeps(None, 0, false), "no sign");
        assert!(seating_sweeps(Some(secs(30)), 8, true), "1 Oct 00:02: eight of twelve read while loading");
        assert!(!seating_sweeps(Some(secs(90)), 8, true), "seats still unread after the quick sweeps: the short passes have it");
        // One loop apart at first: a sweep ends, the loop waits 5 s.
        assert!(sweep_due(Some(secs(5)), 0, false, None, None), "the first sweep");
        assert!(sweep_due(Some(secs(20)), 0, false, Some(secs(5)), Some(secs(1))), "a loop after the last");
        assert!(!sweep_due(Some(secs(20)), 0, false, Some(secs(3)), None), "a full pass just ended");
        assert!(!sweep_due(Some(secs(100)), 0, false, Some(secs(5)), None), "after the quick ones, seldom");
        assert!(sweep_due(Some(secs(100)), 0, false, Some(secs(25)), Some(secs(1))));
        assert!(!sweep_due(Some(secs(240)), 0, false, Some(secs(600)), None), "the window is over");
        assert!(queue_stopped(true, false), "popped or cancelled");
        assert!(!queue_stopped(false, false));
        assert!(!queue_stopped(false, true), "a queue starting is not a match coming");
    }

    /// A lobby empty for longer than `SEATING_FOR` (the end screen, the
    /// menus), then a queue that pops: the queue stopping is a sign of its
    /// own, so the sweeps start again rather than waiting for the idle pass.
    #[test]
    fn a_queue_popping_after_the_window_is_swept_for_again() {
        let secs = Duration::from_secs;
        // Emptied 400 s ago, nobody read since: the window is over.
        assert!(!seating_sweeps(Some(secs(400)), 0, false));
        // The queue stops: the sign is now, and the next loop sweeps.
        assert!(queue_stopped(true, false));
        assert!(sweep_due(Some(secs(5)), 0, false, Some(secs(400)), None));
        // The console naming a match nobody is read for does the same.
        assert!(console_names_new_match(Some(112_099_586), Some(112_068_594)));
        assert!(sweep_due(Some(secs(0)), 0, false, Some(secs(100)), None));
    }

    /// Objects found across the 4 MB pieces, in two regions, the pause taken
    /// every 64 MB, and nothing but players returned.
    #[test]
    fn a_sweep_finds_every_player_and_rests_as_a_pass_does() {
        const US: u32 = 1_161_088_555;
        let big = CHUNK * 20 + 4096;
        let small = 64 << 10;
        let mut a = vec![0_u8; big];
        let mut b = vec![0_u8; small];
        // One straddling a piece boundary, one at a region's end, one in the
        // small region, one of them ours.
        let spots_a = [CHUNK - 0x500, CHUNK * 7 + 0x40, big - players::OBJECT_BYTES];
        for (i, &at) in spots_a.iter().enumerate() {
            let o = players::test_object(100 + i as u32, "p", 2, 1 + i as u8, 7, 80);
            a[at..at + o.len()].copy_from_slice(&o);
        }
        let ours = players::test_object(US, "us", 3, 7, 7, 80);
        b[0x100..0x100 + ours.len()].copy_from_slice(&ours);
        let base_a = 0x3100_0000_0000_usize;
        let base_b = 0x3200_0000_0000_usize;
        let regions = [Region { base: base_a, size: big }, Region { base: base_b, size: small }];
        let read = |addr: usize, buf: &mut [u8]| -> usize {
            let (mem, base) = if addr >= base_b { (&b, base_b) } else { (&a, base_a) };
            let off = addr - base;
            let n = buf.len().min(mem.len().saturating_sub(off));
            buf[..n].copy_from_slice(&mem[off..off + n]);
            n
        };
        let mut pauses = 0;
        let mut found = Vec::new();
        let bytes = sweep_players(read, &regions, US, || pauses += 1, &mut found);
        let at: Vec<usize> = found.iter().map(|(at, _)| *at).collect();
        let mut want: Vec<usize> = spots_a.iter().map(|s| base_a + s).collect();
        want.push(base_b + 0x100);
        assert_eq!(at, want, "each object once, where it sits");
        assert!(found.iter().any(|(_, p)| p.is_us && p.team == 3 && p.slot == 7));
        assert!(bytes >= big + small, "every byte read, the overlaps again");
        assert_eq!(pauses, bytes / THROTTLE_EVERY, "a rest every 64 MB");
        // And where they sat, for the log.
        let spots: Vec<usize> = found.iter().map(|(at, _)| *at).collect();
        assert_eq!(regions_holding(&regions, &spots), regions.to_vec());
        assert_eq!(regions_holding(&regions, &[base_b + small]), Vec::new(), "past the end of the last");
        assert_eq!(regions_holding(&regions, &[base_a - 1]), Vec::new(), "before the first");
    }

    fn seated(account_id: u32, team: u8, slot: u8, is_us: bool) -> MatchPlayer {
        MatchPlayer {
            account_id,
            name: format!("p{account_id}"),
            team,
            slot,
            hero_id: 7,
            level: 1,
            net_worth: 0,
            kills: 0,
            deaths: 0,
            assists: 0,
            rank_badge: None,
            lane: None,
            entity_index: None,
            game_time: None,
            is_us,
            ult: None,
            hero_damage: None,
        }
    }

    #[test]
    fn background_mode_is_entered_and_left() {
        let quiet = Background::begin();
        assert!(quiet.on(), "the mode was refused");
        drop(quiet);
        // Windows refuses to enter the mode from inside it, so entering again
        // shows it was left.
        let again = Background::begin();
        assert!(again.on(), "the mode was not left");
    }

    #[test]
    fn no_reply_hunt_through_the_next_matchs_lane_phase() {
        // As read from a player's game: a lobby emptied (its match taken as
        // ended, the console still naming it), then the next match seated
        // 13 objects, a spectator on slot 0 and us on team 2 slot 4, the
        // rules in progress. The reply hunt for the match before ran a full
        // pass of 13 to 14.5 GB every 30 s from here for five minutes.
        let mut lobby = vec![seated(320_370_657, 1, 0, false)];
        for slot in 1..=12_u8 {
            let team = if slot <= 6 { 2 } else { 3 };
            lobby.push(seated(1_000 + u32::from(slot), team, slot, slot == 4));
        }
        let running = match_running(Some(STATE_IN_PROGRESS), Some(117_534_730), Some(117_534_730));
        assert!(!running, "its end line was read, so it does not count as running");
        let seated_now = seated_in_match(&lobby, true, running);
        assert!(seated_now, "seated all the same");
        let waiting = Some((117_534_730, Duration::from_secs(225)));
        assert!(!reply_pass_wanted(waiting, &[], seated_now || running));
        // Back in the hideout, a hunt still in its window goes on.
        let party = vec![seated(318_657_394, 2, 1, true)];
        let seated_after = seated_in_match(&party, false, false);
        assert!(!seated_after);
        assert!(reply_pass_wanted(Some((117_534_730, Duration::from_secs(60))), &[], seated_after));
    }

    #[test]
    fn only_a_match_seen_going_from_played_to_over_with_a_winner_is_an_end() {
        // As read from the live game: the state goes from 7 to 8 and the
        // winner from -1 to 3 in one read.
        assert_eq!(game_end_of(Some(7), 8, Some(3), Some(106_916_922)), Some(GameEnd { match_id: 106_916_922, winner: 3 }));
        assert_eq!(game_end_of(None, 8, Some(3), Some(1)), None, "already over when first read");
        assert_eq!(game_end_of(Some(8), 8, Some(3), Some(1)), None, "no change");
        assert_eq!(game_end_of(Some(7), 8, None, Some(1)), None, "no winner");
        assert_eq!(game_end_of(Some(7), 8, Some(2), None), None, "no match id");
        assert_eq!(game_end_of(Some(6), 7, None, Some(1)), None, "a match starting");
    }

    /// Team 2's credits alone start the timer as team 3's do; credits split
    /// between the teams keep it running until both are spent.
    #[test]
    fn the_rejuvenator_is_timed_whichever_team_banks_it() {
        let held = |credits: [u32; 2]| -> u32 { credits.iter().sum() };
        let mut at = None;
        let mut seen: Option<u32> = None;
        for (credits, clock) in [([0, 0], 1050.0), ([3, 0], 1063.0), ([2, 0], 1120.0), ([0, 0], 1170.0)] {
            at = rejuv_claim(seen, held(credits), at, clock);
            seen = Some(held(credits));
            if (1060.0..1150.0).contains(&clock) {
                assert_eq!(at, Some(1063.0), "Amber's credits time it, at {clock}");
            }
        }
        assert_eq!(at, None, "spent");
        let mut at = None;
        let mut seen: Option<u32> = Some(0);
        for (credits, clock) in [([1, 0], 1400.0), ([1, 2], 1401.0), ([0, 2], 1450.0), ([0, 0], 1580.4)] {
            at = rejuv_claim(seen, held(credits), at, clock);
            seen = Some(held(credits));
            if clock < 1580.0 {
                assert_eq!(at, Some(1400.0), "from the first credit, at {clock}");
            }
        }
        assert_eq!(at, None);
    }

    #[test]
    fn the_rejuvenator_is_timed_from_the_read_it_is_claimed_at() {
        // A first kill as read from the live game: 0, then 1 at 1072.2, 3, 2,
        // and 0 at 1252.3.
        let mut at = None;
        let mut seen = None;
        for (state, clock) in [(0, 1066.2), (1, 1072.2), (3, 1073.2), (2, 1171.2), (0, 1252.3)] {
            at = rejuv_claim(seen, state, at, clock);
            seen = Some(state);
            match clock {
                c if c < 1070.0 => assert_eq!(at, None),
                c if c < 1250.0 => assert_eq!(at, Some(1072.2), "kept through 3 and 2"),
                _ => assert_eq!(at, None, "over"),
            }
        }
        // Already on when first read: no start known, no timer.
        assert_eq!(rejuv_claim(None, 3, None, 500.0), None);
        assert_eq!(rejuv_claim(Some(3), 3, None, 505.0), None);
    }

    #[test]
    fn our_object_left_behind_by_a_match_is_not_a_lobby() {
        let mut us = seated(1_161_088_555, 3, 11, true);
        us.game_time = Some(2204.5);
        // After the others went, ours reads again unchanged.
        assert!(stale_lone_us(std::slice::from_ref(&us), true, Some(2204.5)));
        // Still moving: a real read (others merely moved in memory).
        assert!(!stale_lone_us(std::slice::from_ref(&us), true, Some(2199.5)));
        // The first read after the others went: not yet known.
        assert!(!stale_lone_us(std::slice::from_ref(&us), true, None));
        // Alone in the hideout with no match before: a party of one.
        assert!(!stale_lone_us(std::slice::from_ref(&us), false, Some(2204.5)));
        // No clock on it: never judged.
        us.game_time = None;
        assert!(!stale_lone_us(std::slice::from_ref(&us), true, None));
        // More than one player: never.
        let mut other = seated(2, 2, 1, false);
        other.game_time = Some(2204.5);
        us.game_time = Some(2204.5);
        assert!(!stale_lone_us(&[us, other], true, Some(2204.5)));
    }

    #[test]
    fn kept_pieces_ask_for_a_pass_only_when_they_lose_the_history() {
        assert!(console_names_new_match(Some(107_317_992), None), "the first match this run");
        assert!(console_names_new_match(Some(107_324_229), Some(107_317_992)), "the next match");
        assert!(!console_names_new_match(Some(107_317_992), Some(107_317_992)), "the same marker found again");
        assert!(!console_names_new_match(None, Some(107_317_992)), "the marker lost mid-match");
        assert!(!console_names_new_match(Some(107_000_000), Some(107_317_992)), "an older match's line");
        assert!(watch_wants_pass(true, 0, 0, false), "the history went");
        assert!(!watch_wants_pass(true, 12, 0, false), "still there");
        assert!(!watch_wants_pass(true, 0, 1, false), "an end line is still there");
        assert!(
            !watch_wants_pass(true, 0, 0, true),
            "in a match the buffer is rewritten, not freed: no pass every 48 s"
        );
        assert!(
            !watch_wants_pass(false, 0, 0, false),
            "pieces that never held any (16 Sep 2026: a pass every 40 s)"
        );
    }

    /// Hideout reads as seen in the live game, and a match's seating.
    #[test]
    fn the_hideout_is_everyone_on_one_team_at_slot_zero() {
        let mut view = MemoryView::default();
        assert!(!view.in_hideout(), "nothing read");
        view.match_players = vec![seated(106_448_300, 2, 0, true)];
        assert!(view.in_hideout(), "alone");
        view.match_players = vec![
            seated(1_161_088_555, 2, 0, false),
            seated(106_448_300, 2, 0, true),
        ];
        assert!(view.in_hideout(), "a friend in the party");
        // Duelling the party in the hideout moves ours to team 3 at slot 0;
        // that is still the hideout.
        view.match_players = vec![
            seated(123_762_552, 2, 0, false),
            seated(259_283_165, 2, 0, false),
            seated(1_161_088_555, 3, 0, true),
        ];
        assert!(view.in_hideout(), "a duel in the hideout is not a match");
        view.match_players = vec![
            seated(1_161_088_555, 2, 0, false),
            seated(106_448_300, 2, 0, true),
        ];
        assert_eq!(
            view.party()
                .iter()
                .map(|p| p.account_id)
                .collect::<Vec<_>>(),
            vec![106_448_300, 1_161_088_555],
            "ours first"
        );
        view.match_players = (0..12u32)
            .map(|i| {
                seated(
                    1000 + i,
                    1 + u8::try_from(i / 6).unwrap(),
                    u8::try_from(i % 6).unwrap(),
                    i == 3,
                )
            })
            .collect();
        assert!(
            !view.in_hideout(),
            "a match seats players in slots on two teams"
        );
        assert!(view.party().is_empty());
        view.match_players = vec![seated(1_161_088_555, 2, 0, false)];
        assert!(!view.in_hideout(), "someone else's lobby is not ours");
        let mut levelled = seated(106_448_300, 2, 0, true);
        levelled.level = 27;
        view.match_players = vec![levelled];
        assert!(
            view.in_hideout() && !view.in_sandbox(),
            "the level says nothing about the Sandbox (14:37 on 13 Sep: level 27, in the hideout)"
        );
    }

    fn at(account_id: u32, team: u8, slot: u8, level: u32, is_us: bool) -> MatchPlayer {
        MatchPlayer {
            level,
            ..seated(account_id, team, slot, is_us)
        }
    }

    /// The game adds its end-of-match numbers after the last seated read, so
    /// a comparison of exact numbers misses these objects and the hideout
    /// reads as a match short of players, its timers frozen.
    #[test]
    fn a_finished_matchs_objects_with_end_numbers_are_leftovers_too() {
        const US: u32 = 1_161_088_555;
        const FRIEND: u32 = 123_762_552;
        let last_seated = vec![
            at(355_076_822, 2, 1, 34, false),
            at(105_977_830, 2, 2, 34, false),
            at(104_421_621, 2, 6, 34, false),
            at(FRIEND, 3, 10, 39, false),
            at(US, 3, 8, 41, true),
        ];
        let roster = (114_227_425_u64, last_seated);
        let mut after_end = vec![
            at(355_076_822, 2, 1, 35, false),
            at(105_977_830, 2, 2, 35, false),
            at(104_421_621, 2, 6, 35, false),
            at(US, 3, 8, 42, true),
        ];
        for p in &mut after_end {
            p.net_worth += 1_200;
        }
        let hideout = [at(FRIEND, 2, 3, 1, false), at(US, 2, 4, 1, true)];
        let read: Vec<MatchPlayer> = after_end.iter().chain(hideout.iter()).cloned().collect();
        let kept: Vec<(u32, u8)> = read
            .iter()
            .filter(|p| !match_leftover(p, Some(&roster), Some(114_227_425)))
            .map(|p| (p.account_id, p.slot))
            .collect();
        assert_eq!(kept, vec![(FRIEND, 3), (US, 4)], "only the hideout party is read");
        // before the end line nothing is dropped: a live match keeps every seat
        assert!(read.iter().all(|p| !match_leftover(p, Some(&roster), None)));
        // and the next match's objects start at level 1, so they never match
        assert!(!match_leftover(&at(355_076_822, 2, 1, 1, false), Some(&roster), Some(114_227_425)));
    }

    /// A finished match as the reader logged it: the last seated read (eight
    /// left, the game over), then, with the end line, six read in the
    /// hideout. Only ours is new; the other five are the match's objects,
    /// one of them a friend who played the match with us, and are not the
    /// party.
    #[test]
    fn a_finished_matchs_objects_are_not_the_party() {
        const US: u32 = 106_448_300;
        let last_seated = vec![
            at(480_212_775, 2, 2, 31, false),
            at(209_326_337, 2, 3, 31, false),
            at(1_851_333_336, 2, 4, 30, false),
            at(32_957_432, 2, 6, 30, false),
            at(1_478_654_968, 3, 7, 33, false),
            at(923_322_541, 3, 8, 32, false),
            at(259_283_165, 3, 10, 39, false),
            at(US, 3, 11, 32, true),
        ];
        let roster = (108_967_377_u64, last_seated);
        let hideout = vec![
            at(480_212_775, 2, 2, 31, false),
            at(209_326_337, 2, 3, 31, false),
            at(US, 2, 4, 1, true),
            at(32_957_432, 2, 6, 30, false),
            at(1_478_654_968, 3, 7, 33, false),
            at(259_283_165, 3, 10, 39, false),
        ];
        let kept: Vec<u32> = hideout
            .iter()
            .filter(|p| !match_leftover(p, Some(&roster), Some(108_967_377)))
            .map(|p| p.account_id)
            .collect();
        assert_eq!(kept, vec![US], "only our new object is read");
        // Later, the friend's own hideout object: level 1, a new seat.
        let friend_back = at(259_283_165, 2, 3, 1, false);
        assert!(!match_leftover(&friend_back, Some(&roster), Some(108_967_377)));
        // Before the end line names the match over, nothing is left out: the
        // end screen still shows the lobby, and a player who has not moved
        // between two reads of a live match is not a leftover.
        assert!(
            hideout
                .iter()
                .all(|p| !match_leftover(p, Some(&roster), Some(108_900_000)))
        );
        assert!(hideout.iter().all(|p| !match_leftover(p, Some(&roster), None)));
        assert!(hideout.iter().all(|p| !match_leftover(p, None, Some(108_967_377))));
        // One number off in the same seat after the end line is still the
        // finished match's object (its end numbers); before the end line it
        // is the live match and stays.
        let mut moved = at(1_478_654_968, 3, 7, 33, false);
        moved.kills = 1;
        assert!(match_leftover(&moved, Some(&roster), Some(108_967_377)));
        assert!(!match_leftover(&moved, Some(&roster), None));
        // The party the leftovers would have been: six, though five were gone.
        let mut view = MemoryView {
            match_players: players::merge_players(hideout),
            ..MemoryView::default()
        };
        assert!(view.in_hideout());
        assert_eq!(view.party().len(), 6, "what the card showed");
        view.match_players = players::merge_players(
            vec![at(US, 2, 4, 1, true)],
        );
        assert_eq!(view.party().len(), 1);
    }

    /// The game ends a match, and seconds later five of its objects, ours
    /// among them, read in the hideout's shape before any end line. The
    /// game's own end is enough to tell them from a party.
    #[test]
    fn the_game_ending_a_match_is_enough_for_the_party() {
        const US: u32 = 1_161_088_555;
        let last_seated = vec![
            at(476_842_053, 2, 1, 35, false),
            at(441_298_411, 2, 5, 34, false),
            at(164_926_713, 3, 7, 32, false),
            at(US, 3, 9, 35, true),
            at(1_024_054_686, 3, 11, 34, false),
            at(165_193_962, 2, 2, 34, false),
        ];
        let mut view = MemoryView {
            match_players: last_seated[..5].to_vec(),
            match_roster: Some((109_128_693, last_seated.clone())),
            ..MemoryView::default()
        };
        assert!(view.in_hideout(), "what the reader made of it");
        assert_eq!(view.party().len(), 5, "no end known: as before");
        view.game_end = Some(GameEnd { match_id: 109_128_693, winner: 2 });
        assert!(view.party().is_empty(), "the game ended it: these are its objects");
        // Later: ours back at level 1, two leftovers still read.
        view.match_players = vec![
            at(US, 2, 1, 1, true),
            at(164_926_713, 3, 7, 32, false),
            at(1_024_054_686, 3, 11, 34, false),
        ];
        assert_eq!(view.party().iter().map(|p| p.account_id).collect::<Vec<_>>(), vec![US]);
        // An older match's end says nothing about this one.
        view.game_end = Some(GameEnd { match_id: 109_000_000, winner: 2 });
        assert_eq!(view.party().len(), 3);
        view.last_match_end = Some(MatchEnd { lobby_id: 0, match_id: 109_128_693 });
        assert_eq!(view.party().len(), 1, "the end line does it too");
    }

    /// A party of three in the hideout, and two spectators of a custom match
    /// played before it still read on team 1.
    #[test]
    fn spectators_are_not_in_the_party() {
        let view = MemoryView {
            match_players: vec![
                at(870_547_025, 1, 0, 0, false),
                at(242_872_005, 1, 0, 0, false),
                at(106_448_300, 2, 3, 1, true),
                at(123_762_552, 2, 4, 1, false),
                at(259_283_165, 2, 5, 1, false),
            ],
            ..MemoryView::default()
        };
        assert!(view.in_hideout());
        assert_eq!(
            view.party().iter().map(|p| p.account_id).collect::<Vec<_>>(),
            vec![106_448_300, 123_762_552, 259_283_165]
        );
        // Ours is kept whatever team the game has us on while it loads.
        let loading = MemoryView {
            match_players: vec![at(106_448_300, 1, 0, 0, true), at(1_855_716_403, 1, 0, 0, false)],
            ..MemoryView::default()
        };
        assert_eq!(
            loading.party().iter().map(|p| p.account_id).collect::<Vec<_>>(),
            vec![106_448_300]
        );
    }

    /// Since the City Never Sleeps update the hideout seats us at slot 1, so
    /// it is told from a match by the console.
    #[test]
    fn the_hideout_after_the_patch_is_told_by_the_console() {
        let mut view = MemoryView {
            match_players: vec![seated(106_448_300, 2, 1, true)],
            ..MemoryView::default()
        };
        assert!(view.in_hideout(), "alone at slot 1, no match begun");
        view.match_players = vec![
            seated(1_161_088_555, 2, 1, false),
            seated(106_448_300, 2, 2, true),
        ];
        assert!(view.in_hideout(), "a party, no match begun");
        view.match_started = Some(MatchEnd {
            lobby_id: 1,
            match_id: 108_639_637,
        });
        assert!(!view.in_hideout(), "the console named a match: it is one, however few are read");
        view.match_started = None;
        view.match_players = (0..12u32)
            .map(|i| seated(1000 + i, 2 + u8::try_from(i / 6).unwrap(), 1 + u8::try_from(i).unwrap(), i == 3))
            .collect();
        assert!(!view.in_hideout(), "twelve seats are never a party");
        view.match_players = vec![seated(1_161_088_555, 2, 1, false)];
        assert!(!view.in_hideout(), "someone else's lobby is still not ours");
    }

    #[test]
    fn an_ended_match_carries_our_player_as_last_seen() {
        let us = MatchPlayer {
            account_id: 106_448_300,
            name: "Player".to_owned(),
            team: 1,
            slot: 3,
            hero_id: 7,
            level: 22,
            net_worth: 48_000,
            kills: 6,
            deaths: 1,
            assists: 11,
            rank_badge: None,
            lane: None,
            entity_index: None,
            game_time: None,
            is_us: true,
            ult: None,
            hero_damage: None,
        };
        let end = MatchEnd {
            lobby_id: 1,
            match_id: 105_292_189,
        };
        let row = ended_match(end, Some(&us), 1_789_245_816, Some(106_448_300), Some((4, 1)));
        assert_eq!(
            row.account_id,
            Some(106_448_300),
            "the row names its account"
        );
        assert_eq!(
            (
                row.match_id,
                row.hero_id,
                row.kills,
                row.deaths,
                row.assists
            ),
            (105_292_189, 7, 6, 1, 11)
        );
        assert_eq!(row.won, None);
        assert_eq!(row.modes, Some((4, 1)), "the row carries the lobby's modes");
        let bare = ended_match(end, None, 1, None, None);
        // a spectator's end line is not listed; a seat or no read at all is
        let mut seat = us.clone();
        seat.team = 2;
        assert!(ours_to_list(Some(&seat)) && ours_to_list(None));
        seat.team = 1;
        seat.slot = 0;
        seat.hero_id = 0;
        assert!(!ours_to_list(Some(&seat)), "a spectator's match is not ours");
        assert_eq!((bare.match_id, bare.hero_id), (105_292_189, 0));
        assert_eq!(bare.modes, None);
    }

    #[test]
    fn the_newest_match_end_is_the_highest_id() {
        let ends = [
            MatchEnd {
                lobby_id: 1,
                match_id: 10,
            },
            MatchEnd {
                lobby_id: 2,
                match_id: 30,
            },
            MatchEnd {
                lobby_id: 3,
                match_id: 20,
            },
        ];
        assert_eq!(newest_end(&ends).map(|e| e.match_id), Some(30));
        assert_eq!(newest_end(&[]), None);
    }

    /// The `kingme` case: our object gone, the party member's still there.
    #[test]
    fn our_player_is_held_while_the_lobby_stays() {
        let us = seated(106_448_300, 2, 0, true);
        let other_player = seated(1_000_000_222, 2, 0, false);
        let previous = [106_448_300, 1_000_000_222];
        assert_eq!(
            hold_us(std::slice::from_ref(&other_player), Some(&us), &previous).map(|p| p.account_id),
            Some(106_448_300)
        );
        assert_eq!(
            hold_us(std::slice::from_ref(&other_player), None, &previous),
            None,
            "nothing to hold"
        );
        assert_eq!(
            hold_us(&[], Some(&us), &previous),
            None,
            "an empty read is not a lobby"
        );
        assert_eq!(
            hold_us(&[other_player.clone(), us.clone()], Some(&us), &previous),
            None,
            "we are there"
        );
        assert_eq!(
            hold_us(&[seated(9, 2, 0, false)], Some(&us), &previous),
            None,
            "a new face is a new lobby"
        );
        assert_eq!(hold_us(&[other_player], Some(&us), &[]), None, "no lobby before");
    }

    /// Twelve players read, then only our own object (team 3 slot 11) for
    /// ten minutes with the console line gone, while the rules object reads
    /// 7, in progress. That lone read is still a seated match, not the
    /// hideout, so the other eleven are looked for.
    #[test]
    fn a_match_whose_other_players_stopped_reading_is_still_a_match() {
        let lone = vec![seated(1_161_088_555, 3, 11, true)];
        let running = match_running(Some(STATE_IN_PROGRESS), Some(109_106_618), Some(109_089_347));
        assert!(running, "rules in progress, the named match not ended");
        let mut view = MemoryView {
            match_players: lone.clone(),
            match_running: running,
            ..MemoryView::default()
        };
        assert!(!view.in_hideout(), "mid match with only our object read is not the hideout");
        assert!(
            seated_in_match(&lone, false, running),
            "seated, so the players missing from the lobby are looked for"
        );
        assert!(short_pass_wanted(seated_in_match(&lone, false, running), missing_seats(&lone), 0));
        // The match over (state 8, then 11 after the end line): the same lone
        // read is the hideout again.
        view.match_running = match_running(Some(8), Some(109_106_618), Some(109_106_618));
        assert!(!view.match_running);
        assert!(view.in_hideout());
    }

    /// Back in the hideout after a match, with the party (ours at slot 3, a
    /// friend at slot 4) and one object the match left behind (level 37),
    /// the end line read. This is not a seated match, so it asks for no
    /// passes to find nine missing seats.
    #[test]
    fn the_hideout_after_a_match_asks_for_no_passes() {
        let mut leftover = seated(76_346_822, 2, 6, false);
        leftover.level = 37;
        let hideout = vec![
            seated(1_161_088_555, 2, 3, true),
            seated(123_762_552, 2, 4, false),
            leftover,
        ];
        let running = match_running(Some(11), None, Some(110_560_287));
        assert!(!seated_in_match(&hideout, false, running), "not seated in a match");
        let view = MemoryView {
            match_players: hideout,
            match_running: running,
            ..MemoryView::default()
        };
        assert!(view.in_hideout());
    }

    /// A match laid out in made-up memory: twelve player objects on the heap,
    /// ours inside a piece the pass kept for the history beside it. The bytes
    /// prove the reader's logic only, not where the game puts its objects.
    #[test]
    fn a_kept_piece_holding_our_object_does_not_lose_the_other_eleven() {
        const BASE: usize = 0x2_0000_0000;
        const GAP: usize = 0x3000;
        let us = 1_161_088_555_u32;
        let mut heap = vec![0_u8; GAP * 14];
        fn put(heap: &mut [u8], at: usize, bytes: &[u8]) {
            heap[at..at + bytes.len()].copy_from_slice(bytes);
        }
        for seat in 1..=12_u8 {
            let account = if seat == 11 { us } else { 1000 + u32::from(seat) };
            let team = if seat <= 6 { 2 } else { 3 };
            put(&mut heap, usize::from(seat) * GAP, &players::test_object(account, "P", team, seat, 7, 70));
        }
        let read = |heap: &[u8], addr: usize, buf: &mut [u8]| -> usize {
            let Some(at) = addr.checked_sub(BASE) else { return 0 };
            let Some(src) = heap.get(at..at + buf.len()) else { return 0 };
            buf.copy_from_slice(src);
            buf.len()
        };

        // The full pass: every object, at its address in the game.
        let pass: Vec<(usize, MatchPlayer)> = players::find_players(&heap, Offsets::CURRENT, us)
            .into_iter()
            .map(|(at, p)| (BASE + at, p))
            .collect();
        assert_eq!(pass.len(), 12);
        let spots: Vec<usize> = pass.iter().map(|(at, _)| *at).collect();

        // 20 s on: the kept piece is read again, and holds our object alone.
        let piece = 11 * GAP - 0x800..12 * GAP;
        let in_pieces: Vec<(usize, MatchPlayer)> =
            players::find_players(&heap[piece.clone()], Offsets::CURRENT, us)
                .into_iter()
                .map(|(at, p)| (BASE + piece.start + at, p))
                .collect();
        assert_eq!(
            in_pieces.iter().map(|(_, p)| p.account_id).collect::<Vec<_>>(),
            [us],
            "the piece holds ours alone: before the fix this was the whole read"
        );
        let read_now =
            players_between_passes(|a, b| read(&heap, a, b), &spots, in_pieces.clone(), Some(us));
        assert_eq!(read_now.len(), 12, "all twelve read, ours once although found both ways");
        assert_eq!(players::merge_players(read_now.iter().map(|(_, p)| p.clone()).collect()).len(), 12);
        // The spots carry on to the next read, all twelve.
        let next: Vec<usize> = read_now.iter().map(|(at, _)| *at).collect();
        assert_eq!(next.len(), 12);

        // The game moves on: an enemy levels up, and one object is freed.
        let mut later = heap.clone();
        // The level is the first field the player data block holds after
        // its header, 0x08 in.
        let level = 8 * GAP + Offsets::CURRENT.player_data + 0x08;
        later[level..level + 4].copy_from_slice(&15_u32.to_le_bytes());
        later[3 * GAP..4 * GAP].fill(0);
        // And a player the pass did not find turns up in the kept piece,
        // after our object.
        put(&mut later, 11 * GAP + 0xE00, &players::test_object(2000, "Late", 2, 3, 9, 70));
        let piece_later: Vec<(usize, MatchPlayer)> =
            players::find_players(&later[piece.clone()], Offsets::CURRENT, us)
                .into_iter()
                .map(|(at, p)| (BASE + piece.start + at, p))
                .collect();
        let read_later = players_between_passes(|a, b| read(&later, a, b), &next, piece_later, Some(us));
        let accounts: Vec<u32> = read_later.iter().map(|(_, p)| p.account_id).collect();
        assert_eq!(read_later.len(), 12, "eleven spots still read, and the newcomer");
        assert!(!accounts.contains(&1003), "a freed object is dropped");
        assert!(accounts.contains(&2000), "a player in a kept piece joins");
        assert_eq!(read_later.iter().find(|(_, p)| p.account_id == 1008).map(|(_, p)| p.level), Some(15));

        // Without our account nothing is read.
        assert!(players_between_passes(|a, b| read(&heap, a, b), &spots, Vec::new(), None).is_empty());
    }

    /// A match loading: twelve seats before the console names it or the
    /// rules object is found. A match all along.
    #[test]
    fn a_loading_match_is_seated_before_anything_names_it() {
        let lobby: Vec<MatchPlayer> = (1..=12u8)
            .map(|slot| seated(1000 + u32::from(slot), if slot <= 6 { 2 } else { 3 }, slot, slot == 4))
            .collect();
        assert!(seated_in_match(&lobby, false, false));
        assert!(!match_running(None, None, None));
        // A rules object in progress for a match no console line named (a
        // replay) is not a match being played.
        assert!(!match_running(Some(STATE_IN_PROGRESS), None, None));
    }

    /// The records found by a pass, read again through the same read the
    /// game's memory takes: a minute's tick on the live record shows; a
    /// record freed and overwritten drops out. The heap is made up, so this
    /// proves the reading only.
    #[test]
    fn friend_records_are_read_again_and_only_a_ticking_one_is_live() {
        use std::cell::RefCell;
        const BASE: usize = 0x5_0000_0000;
        let heap = RefCell::new(vec![0_u8; 0x3000]);
        let put = |at: usize, bytes: &[u8]| heap.borrow_mut()[at..at + bytes.len()].copy_from_slice(bytes);
        let record = |account: u32, text_at: usize| {
            let mut b = vec![0_u8; friends_presence::RECORD_BYTES];
            b[0..4].copy_from_slice(&account.to_le_bytes());
            b[4..8].copy_from_slice(&0x0110_0001_u32.to_le_bytes());
            b[8..12].copy_from_slice(&3_u32.to_le_bytes());
            b[0x10..0x18].copy_from_slice(&((BASE + 0x2800) as u64).to_le_bytes());
            b[0x20..0x28].copy_from_slice(&(text_at as u64).to_le_bytes());
            b
        };
        // The live record at 0x100, a freed copy of another friend's at 0x200.
        put(0x100, &record(104_859_703, BASE + 0x1000));
        put(0x200, &record(1_000_000_222, BASE + 0x1100));
        put(0x1000, b"Deadlock: Wraith (20 min) ");
        put(0x1100, b"Deadlock: Rem (14 min) ");
        // The last text sits at a page's end with nothing readable after it.
        let read = |at: usize, buf: &mut [u8]| {
            let h = heap.borrow();
            let Some(off) = at.checked_sub(BASE) else { return 0 };
            if off >= h.len() || off + buf.len() > h.len() {
                return 0;
            }
            buf.copy_from_slice(&h[off..off + buf.len()]);
            buf.len()
        };
        let spots = [BASE + 0x100, BASE + 0x200];
        let mut tracker = friends_presence::Tracker::default();
        let t0 = Instant::now();
        read_friend_records(read, &spots, &mut tracker, t0, 5000);
        assert!(tracker.live(t0).is_empty(), "a first read proves nothing");
        put(0x1000, b"Deadlock: Wraith (21 min) ");
        // The copy's memory is reused for something else.
        put(0x200, &[0xAA; 8]);
        read_friend_records(read, &spots, &mut tracker, t0 + Duration::from_secs(60), 5060);
        let live = tracker.live(t0 + Duration::from_secs(60));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].account_id, 104_859_703);
        assert_eq!(live[0].presence.minutes, Some(21));
        assert_eq!(tracker.spots(), vec![BASE + 0x100], "the overwritten record is dropped");
        // A text in the heap's last bytes, where a 64-byte read fails.
        let end = 0x3000 - 20;
        put(end, b"In a Party Hideout ");
        put(0x100, &record(104_859_703, BASE + end));
        read_friend_records(read, &spots[..1], &mut tracker, t0 + Duration::from_secs(80), 5080);
        let live = tracker.live(t0 + Duration::from_secs(80));
        assert_eq!(live[0].presence.state, "hideout");
    }

    /// READ-ONLY, against the running game: the full pass's record search and
    /// the quick re-read, as the worker runs them, four reads 25 s apart.
    /// Prints what it finds to the terminal running it, never to a log.
    /// `cargo test --release --lib live_friend_records -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the running game"]
    fn live_friend_records() {
        let pid = game_pid("deadlock.exe").expect("the game is running");
        let game = GameProcess::open(pid).expect("opened read-only");
        let started = Instant::now();
        let mut spots = Vec::new();
        let mut buf = vec![0_u8; CHUNK];
        for region in game.private_regions() {
            let mut off = 0;
            while off < region.size {
                let want = CHUNK.min(region.size - off);
                let got = game.read(region.base + off, &mut buf[..want]);
                spots.extend(
                    friends_presence::find_records(&buf[..got], region.base + off)
                        .into_iter()
                        .map(|(at, _)| at),
                );
                if want <= OVERLAP || off + want >= region.size {
                    break;
                }
                off += want - OVERLAP;
            }
        }
        spots.sort_unstable();
        spots.dedup();
        println!("pass: {} friend records in {:?}", spots.len(), started.elapsed());
        let mut tracker = friends_presence::Tracker::default();
        for round in 0..4 {
            if round > 0 {
                thread::sleep(Duration::from_secs(25));
            }
            read_friend_records(|a, b| game.read(a, b), &spots, &mut tracker, Instant::now(), unix_now());
            let (followed, live) = tracker.counts(Instant::now());
            println!("read {round}: {followed} followed, {live} live records");
            for p in tracker.live(Instant::now()) {
                println!("  {} {:?}", p.account_id, p.presence);
            }
        }
    }
}
