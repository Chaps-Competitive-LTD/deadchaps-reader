//! Deadlock's game reader: reads the running game's memory so a match is known
//! the moment it ends, before deadlock-api has it.
//!
//! READ-ONLY. The game is opened with query and read rights alone: nothing is
//! written, injected or hooked. A setting turns it off, and deadchaps.gg's kill
//! switch turns it off for every install.
//!
//! The two core sources, both laid out as measured on the live game:
//! - The in-game match history, held as 144-byte records (the game's own
//!   history entry message): match id, hero, duration, start time, teams,
//!   K/D/A, last hits, denies, level, souls, mode and outcome. The layout was
//!   checked field by field against deadlock-api's rows for two matches.
//! - The console buffer, which logs `Lobby <id> for Match <id> destroyed` when
//!   a match ends, even without `-condebug`.
//!
//! Beside those the reader takes the match's players (`players`), the rules
//! object (`rules`: timers, game state, winner, the draft's bans), the queue
//! state from Steam rich presence, and the salts from Valve's replies
//! (`salts_probe`), so a match can be reported the moment it ends.
//!
//! Finding them means a throttled full pass over the game's private memory.
//! A pass runs when the game is attached and when something happens (the
//! queue starting or stopping, the console naming a match, a match ending),
//! otherwise every ten minutes out of a match and every thirty in one. Between
//! passes only the pieces that held records are read again. In a release
//! build a pass over about 8 GB takes about 10 s of wall time, most of it the
//! throttle's pauses.

use serde::{Deserialize, Serialize};

pub mod friends_presence;
pub mod game_process;
#[cfg(target_os = "linux")]
pub mod grant;
pub mod players;
pub mod presence_probe;
pub mod procs;
pub mod protocol;
pub mod rules;
pub mod salts_probe;
#[cfg(feature = "process")]
mod store;
pub mod view;

/// The game's process, opened with read rights alone. Only in the reader
/// program (feature `process`): the Companion app links none of it.
#[cfg(all(feature = "process", windows))]
mod process;
/// On Linux the game runs under Proton, the same Windows program in a Linux
/// process (process_linux.rs).
#[cfg(all(feature = "process", target_os = "linux"))]
#[path = "process_linux.rs"]
pub mod process;
#[cfg(all(feature = "process", any(windows, target_os = "linux")))]
mod worker;
#[cfg(all(feature = "process", any(windows, target_os = "linux")))]
pub use worker::{GameMemory, Hooks};

pub use game_process::game_pid;
pub use view::{EndedMatch, GameEnd, MemoryView, Objectives};

/// One in-game match history record.
pub const RECORD_SIZE: usize = 144;
/// No real match id is below this or above the next bound.
const MIN_MATCH_ID: u64 = 1_000_000;
const MAX_MATCH_ID: u64 = 10_000_000_000;
/// 1 Jan 2024, before Deadlock's first public match.
const MIN_START: u32 = 1_704_067_200;
const MAX_HERO_ID: u32 = 128;
/// Three hours.
const MAX_DURATION_S: u32 = 10_800;

/// A match as the game's own history records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub match_id: u64,
    pub hero_id: u32,
    pub duration_s: u32,
    pub start_time: u32,
    pub winning_team: u32,
    pub player_team: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub last_hits: u32,
    pub denies: u32,
    pub hero_level: u32,
    pub net_worth: u32,
    pub match_mode: u32,
    pub game_mode: u32,
    pub outcome: u32,
}

impl HistoryEntry {
    pub fn won(&self) -> bool {
        self.winning_team == self.player_team
    }
}

/// A match the console says just ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchEnd {
    pub lobby_id: u64,
    pub match_id: u64,
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(buf: &[u8], off: usize) -> Option<u64> {
    buf.get(off..off + 8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
}

/// The record at `off`, when every field is one a real match could have.
/// `now_unix` bounds the start time, so a future date is not a match.
pub fn entry_at(buf: &[u8], off: usize, now_unix: u32) -> Option<HistoryEntry> {
    let match_id = u64_at(buf, off)?;
    if !(MIN_MATCH_ID..MAX_MATCH_ID).contains(&match_id) {
        return None;
    }
    let field = |rel: usize| u32_at(buf, off + rel);
    let entry = HistoryEntry {
        match_id,
        hero_id: field(8)?,
        duration_s: field(12)?,
        start_time: field(16)?,
        winning_team: field(20)?,
        player_team: field(24)?,
        kills: field(28)?,
        deaths: field(32)?,
        assists: field(36)?,
        last_hits: field(40)?,
        denies: field(44)?,
        hero_level: field(48)?,
        net_worth: field(52)?,
        match_mode: field(76)?,
        game_mode: field(80)?,
        outcome: field(100)?,
    };
    let plausible = (1..=MAX_HERO_ID).contains(&entry.hero_id)
        && (1..=MAX_DURATION_S).contains(&entry.duration_s)
        && (MIN_START..=now_unix.saturating_add(86_400)).contains(&entry.start_time)
        && entry.winning_team <= 1
        && entry.player_team <= 1
        && entry.kills <= 200
        && entry.deaths <= 200
        && entry.assists <= 200
        && entry.last_hits <= 5_000
        && entry.denies <= 5_000
        && entry.hero_level <= 100
        && entry.net_worth <= 1_000_000
        && (1..=16).contains(&entry.match_mode)
        && entry.game_mode <= 16
        && entry.outcome <= 3;
    plausible.then_some(entry)
}

/// The first three fields of `entry_at`'s check, on the record's first 16
/// bytes: a match id, hero id and duration in range. Almost every position in
/// the game's memory fails it, so the whole check runs on almost nothing. It
/// only ever rules out what `entry_at` would reject too.
#[inline]
fn could_start_record(head: &[u8; 16]) -> bool {
    let match_id = u64::from_le_bytes([
        head[0], head[1], head[2], head[3], head[4], head[5], head[6], head[7],
    ]);
    let hero_id = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let duration_s = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    // `&`, not `&&`: no branches, so a block of these compiles to a few wide
    // compares.
    (match_id.wrapping_sub(MIN_MATCH_ID) < MAX_MATCH_ID - MIN_MATCH_ID)
        & (hero_id.wrapping_sub(1) < MAX_HERO_ID)
        & (duration_s.wrapping_sub(1) < MAX_DURATION_S)
}

/// Positions `find_history` tests a block at a time.
const BLOCK_WORDS: usize = 32;
const BLOCK_BYTES: usize = BLOCK_WORDS * 8;

/// Whether any of the block's 8-byte positions could start a record. It
/// tests every position without stopping early, which lets the compiler test
/// many at once. `block` has one word more than the positions, for the last
/// position's hero id and duration.
#[inline]
fn block_could_start_record(block: &[u8; BLOCK_BYTES + 8]) -> bool {
    let mut any = false;
    for word in 0..BLOCK_WORDS {
        let at = word * 8;
        let mut head = [0_u8; 16];
        head.copy_from_slice(&block[at..at + 16]);
        any |= could_start_record(&head);
    }
    any
}

/// The records of one history list, in the order the game holds them.
///
/// The game loads OTHER players' histories too, whenever their profile is
/// opened, and those records look exactly like the local player's. They are
/// told apart by where they sit: one player's history is a run of records back
/// to back, so a run is one player's, and the Companion keeps the run that
/// overlaps the matches deadchaps.gg already lists for this account.
pub fn runs_of(found: &[(usize, HistoryEntry)]) -> Vec<Vec<HistoryEntry>> {
    let mut runs: Vec<Vec<HistoryEntry>> = Vec::new();
    let mut last_off: Option<usize> = None;
    for (off, entry) in found {
        match last_off {
            Some(previous) if off.saturating_sub(previous) == RECORD_SIZE => {
                if let Some(run) = runs.last_mut() {
                    run.push(*entry);
                }
            }
            _ => runs.push(vec![*entry]),
        }
        last_off = Some(*off);
    }
    runs
}

/// Every history record in `buf`, as (offset, entry). A record counts only
/// beside another one a record away, with the newer match first, the way the
/// game lays out its history: one plausible-looking block alone is noise.
pub fn find_history(buf: &[u8], now_unix: u32) -> Vec<(usize, HistoryEntry)> {
    let mut out = Vec::new();
    if buf.len() < RECORD_SIZE * 2 {
        return out;
    }
    let mut off = 0;
    // Up to here a block test already found a possible record, so each
    // position is tested alone.
    let mut one_by_one_until = 0;
    while off + RECORD_SIZE <= buf.len() {
        // Most of the game's memory holds no record: step over a whole block
        // when none of its positions could start one.
        if off >= one_by_one_until
            && let Some(block) = buf
                .get(off..off + BLOCK_BYTES + 8)
                .and_then(|b| <&[u8; BLOCK_BYTES + 8]>::try_from(b).ok())
        {
            if !block_could_start_record(block) {
                off += BLOCK_BYTES;
                continue;
            }
            one_by_one_until = off + BLOCK_BYTES;
        }
        let head = buf
            .get(off..off + 16)
            .and_then(|b| <&[u8; 16]>::try_from(b).ok());
        if !head.is_some_and(could_start_record) {
            off += 8;
            continue;
        }
        let Some(entry) = entry_at(buf, off, now_unix) else {
            off += 8;
            continue;
        };
        let next = entry_at(buf, off + RECORD_SIZE, now_unix)
            .is_some_and(|n| n.start_time <= entry.start_time && n.match_id != entry.match_id);
        let prev = off
            .checked_sub(RECORD_SIZE)
            .and_then(|p| entry_at(buf, p, now_unix))
            .is_some_and(|p| p.start_time >= entry.start_time && p.match_id != entry.match_id);
        if next || prev {
            out.push((off, entry));
            off += RECORD_SIZE;
        } else {
            off += 8;
        }
    }
    out
}

const END_MIDDLE: &[u8] = b" for Match ";
const END_TAIL: &[u8] = b" destroyed";
/// The console names a match the moment it begins, the same way:
/// `Lobby <id> for Match <id> created`, often before deadlock-api's live
/// feed lists the match.
const START_TAIL: &[u8] = b" created";
const END_HEAD: &[u8] = b"Lobby ";

fn digits_before(buf: &[u8], end: usize) -> Option<u64> {
    let start = buf[..end]
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    if start == end || end - start > 20 {
        return None;
    }
    std::str::from_utf8(&buf[start..end]).ok()?.parse().ok()
}

/// Every `Lobby <id> for Match <id> destroyed` line in `buf`, oldest first.
pub fn find_match_ends(buf: &[u8]) -> Vec<MatchEnd> {
    find_lobby_lines(buf, END_TAIL)
}

/// Every `Lobby <id> for Match <id> created` line in `buf`, oldest first:
/// the match the game is in, named as it begins.
pub fn find_match_starts(buf: &[u8]) -> Vec<MatchEnd> {
    find_lobby_lines(buf, START_TAIL)
}

fn find_lobby_lines(buf: &[u8], tail: &[u8]) -> Vec<MatchEnd> {
    let mut out = Vec::new();
    for at in memchr::memmem::find_iter(buf, END_MIDDLE) {
        let ids_start = at + END_MIDDLE.len();
        let ids_end = buf[ids_start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map_or(buf.len(), |i| ids_start + i);
        if ids_end == ids_start || !buf[ids_end..].starts_with(tail) {
            continue;
        }
        let Some(match_id) = std::str::from_utf8(&buf[ids_start..ids_end])
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(lobby_id) = digits_before(buf, at) else {
            continue;
        };
        let lobby_digits = lobby_id.to_string().len();
        let head_end = at - lobby_digits;
        if head_end < END_HEAD.len() || &buf[head_end - END_HEAD.len()..head_end] != END_HEAD {
            continue;
        }
        if (MIN_MATCH_ID..MAX_MATCH_ID).contains(&match_id) {
            out.push(MatchEnd { lobby_id, match_id });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real bytes from Deadlock's memory: three history records, in the
    /// game's own order.
    const HISTORY: &[u8] = include_bytes!("fixture_history.bin");
    /// Real bytes from the game's console buffer around the match-end line.
    const CONSOLE: &[u8] = include_bytes!("fixture_console.bin");
    const NOW: u32 = 1_789_080_000;

    #[test]
    fn the_real_history_records_decode_to_the_in_game_card() {
        let found = find_history(HISTORY, NOW);
        let ids: Vec<u64> = found.iter().map(|(_, e)| e.match_id).collect();
        assert_eq!(ids, [104_921_839, 104_910_270, 104_907_711]);
        let (_, latest) = found[0];
        // The game's own history card: Loss, Ranked, Wraith, 30:45, 36k souls.
        assert_eq!(
            latest,
            HistoryEntry {
                match_id: 104_921_839,
                hero_id: 7,
                duration_s: 1845,
                start_time: 1_789_078_989,
                winning_team: 0,
                player_team: 1,
                kills: 5,
                deaths: 4,
                assists: 4,
                last_hits: 204,
                denies: 1,
                hero_level: 32,
                net_worth: 36_395,
                match_mode: 4,
                game_mode: 1,
                outcome: 2,
            }
        );
        assert!(!latest.won());
        // deadlock-api's own rows for the other two.
        let (_, second) = found[1];
        assert_eq!(
            (
                second.duration_s,
                second.kills,
                second.deaths,
                second.assists,
                second.net_worth,
                second.won()
            ),
            (1742, 2, 10, 1, 26_768, false)
        );
        let (_, third) = found[2];
        assert_eq!(
            (
                third.duration_s,
                third.kills,
                third.deaths,
                third.assists,
                third.net_worth,
                third.won(),
                third.match_mode
            ),
            (866, 1, 2, 3, 15_295, true, 1)
        );
    }

    #[test]
    fn records_back_to_back_are_one_players_history() {
        let found = find_history(HISTORY, NOW);
        let runs = runs_of(&found);
        assert_eq!(runs.len(), 1, "three records in a row are one history");
        assert_eq!(runs[0].len(), 3);

        // The same three records, then another player's list somewhere else.
        let mut apart: Vec<(usize, HistoryEntry)> = found.clone();
        let elsewhere = found[0].0 + 8192;
        apart.extend(found.iter().map(|(off, e)| (off + elsewhere, *e)));
        let split = runs_of(&apart);
        assert_eq!(split.len(), 2, "a gap starts another player's history");
        assert_eq!((split[0].len(), split[1].len()), (3, 3));
        assert!(runs_of(&[]).is_empty());
    }

    #[test]
    fn records_are_found_wherever_they_sit_but_never_alone() {
        let mut shifted = vec![0xAB_u8; 1000];
        shifted.extend_from_slice(HISTORY);
        shifted.extend(std::iter::repeat_n(0x11_u8, 333));
        let found = find_history(&shifted, NOW);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].0, 1000);
        assert!(
            find_history(&HISTORY[..RECORD_SIZE], NOW).is_empty(),
            "one record alone is noise"
        );
        assert!(find_history(&[0_u8; 4096], NOW).is_empty());
        // A start time in the future is not a match.
        assert!(find_history(HISTORY, 1_700_000_000).is_empty());
    }

    /// `find_history` without the `could_start_record` quick test: the full
    /// check at every position. The quick test must never change what is found.
    fn find_history_unfiltered(buf: &[u8], now_unix: u32) -> Vec<(usize, HistoryEntry)> {
        let mut out = Vec::new();
        if buf.len() < RECORD_SIZE * 2 {
            return out;
        }
        let mut off = 0;
        while off + RECORD_SIZE <= buf.len() {
            let Some(entry) = entry_at(buf, off, now_unix) else {
                off += 8;
                continue;
            };
            let next = entry_at(buf, off + RECORD_SIZE, now_unix)
                .is_some_and(|n| n.start_time <= entry.start_time && n.match_id != entry.match_id);
            let prev = off
                .checked_sub(RECORD_SIZE)
                .and_then(|p| entry_at(buf, p, now_unix))
                .is_some_and(|p| p.start_time >= entry.start_time && p.match_id != entry.match_id);
            if next || prev {
                out.push((off, entry));
                off += RECORD_SIZE;
            } else {
                off += 8;
            }
        }
        out
    }

    /// Words like a game heap's: zeros, small counts, pointers, float bits and
    /// ids near the match id range, with the real history bytes dropped in at
    /// 8-byte-aligned offsets, some with a field pushed out of range.
    fn heap_like(bytes: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out = Vec::with_capacity(bytes + HISTORY.len());
        while out.len() < bytes {
            let r = next();
            let word = match r % 8 {
                0..=2 => 0,
                3 => r >> 58,
                4 => 0x0000_01F0_0000_0000 | (r >> 24),
                5 => u64::from(0x3F80_0000_u32 | (r as u32 >> 12)),
                6 => MIN_MATCH_ID + (r >> 32) % (MAX_MATCH_ID - MIN_MATCH_ID),
                _ => r,
            };
            out.extend_from_slice(&word.to_le_bytes());
            if r % 50_000 == 0 {
                let mut copy = HISTORY.to_vec();
                if r % 3 == 0 {
                    copy[RECORD_SIZE + 8] = 0xFF;
                }
                out.extend_from_slice(&copy);
            }
        }
        out
    }

    #[test]
    fn the_quick_test_finds_exactly_what_the_full_check_finds() {
        let buf = heap_like(8 << 20);
        let found = find_history(&buf, NOW);
        assert!(found.len() > 10, "the planted records are found");
        assert_eq!(found, find_history_unfiltered(&buf, NOW));
        for shift in 0..8 {
            let mut shifted = vec![0_u8; shift * 8];
            shifted.extend_from_slice(&buf[..1 << 20]);
            assert_eq!(
                find_history(&shifted, NOW),
                find_history_unfiltered(&shifted, NOW)
            );
        }
    }

    /// How fast the search runs, for tuning:
    /// cargo test --release --lib search_speed -- --ignored --nocapture
    #[test]
    #[ignore = "timing, run by hand in release"]
    fn search_speed() {
        let buf = heap_like(512 << 20);
        let mb = (buf.len() >> 20) as f64;
        let started = std::time::Instant::now();
        let quick = find_history(&buf, NOW);
        let quick_s = started.elapsed().as_secs_f64();
        let started = std::time::Instant::now();
        let full = find_history_unfiltered(&buf, NOW);
        let full_s = started.elapsed().as_secs_f64();
        let started = std::time::Instant::now();
        let ends = find_match_ends(&buf);
        let ends_s = started.elapsed().as_secs_f64();
        assert_eq!(quick, full);
        println!(
            "{mb} MB, {} records: quick {:.0} MB/s, full check {:.0} MB/s, match ends {:.0} MB/s ({} found)",
            quick.len(),
            mb / quick_s,
            mb / full_s,
            mb / ends_s,
            ends.len()
        );
    }

    #[test]
    fn the_real_console_line_gives_the_match_that_ended() {
        assert_eq!(
            find_match_ends(CONSOLE),
            [MatchEnd {
                lobby_id: 174_130_669_616_673_689,
                match_id: 104_921_839
            }]
        );
        assert!(
            find_match_ends(b"xx for Match 123456789 destroyedyy").is_empty(),
            "needs the Lobby head"
        );
        assert!(find_match_ends(b"Lobby 5 for Match 104921839 created").is_empty());
        assert_eq!(
            find_match_starts(b"..Lobby 174130669617287137 for Match 105524958 created.|HB.|"),
            [MatchEnd {
                lobby_id: 174_130_669_617_287_137,
                match_id: 105_524_958
            }],
            "the created line names the match as it begins"
        );
        assert!(
            find_match_starts(CONSOLE).is_empty(),
            "a destroyed line is not a start"
        );
        assert_eq!(
            find_match_ends(
                b"..Lobby 9 for Match 104921840 destroyed..Lobby 10 for Match 104921841 destroyed"
            )
            .iter()
            .map(|e| e.match_id)
            .collect::<Vec<_>>(),
            [104_921_840, 104_921_841]
        );
    }
}
