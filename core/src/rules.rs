//! The game's rules object, for the objective timers: the match clock, the
//! mid boss and the rejuvenator, plus the match's state, winner and bans.
//!
//! Read-only, like the rest of this crate. The object is the entity named
//! `citadel_gamerules`. Measured on the live game in a ranked match, it holds:
//! - `m_flGameStartTime` at 0x60, the game time at which the match clock
//!   reads 0:00;
//! - the game state at 0x74 (7 while the match is played);
//! - the minimap bounds at 0x84 to 0x98, (-10752, -10752, 0) and
//!   (10752, 10752, 0), which is how the object is found in a pass;
//! - the match mode, expected player count and winning team, and the mid
//!   boss block (kill count, each team's rejuvenator credits, next spawn),
//!   at the offsets below, from the client schema of build 25611908.
//!
//! The game time itself is on each player's controller (see
//! [`crate::players::GAME_TIME`]), and it keeps running through a pause. The
//! match clock is the game's own, kept far into the object (measured live in
//! a match paused twice, one read a second):
//! - the pausing team, -1 while nobody has paused;
//! - the tick the match clock was last set at (64 ticks a second: tick / 64
//!   is the game time, exactly);
//! - the match clock at that tick (-30.0 during the pre-game countdown);
//! - while an unpause counts down, the game time it ends at, else 0.
//!
//! So the clock is frozen while a team has the game paused or the countdown
//! runs, and otherwise is the stored value plus the game time since that
//! tick. After two pauses this matches the game's own value to the
//! hundredth. The mid boss's next spawn is a match clock time, not a game
//! time: with this clock the respawns read 7:00 after the first kill and 6:00
//! after the second, to the second.

/// Where the minimap bounds sit, and their bytes.
const BOUNDS_AT: usize = 0x84;
const BOUNDS: [f32; 6] = [-10752.0, -10752.0, 0.0, 10752.0, 10752.0, 0.0];
const GAME_START: usize = 0x60;
const STATE: usize = 0x74;
// Build 25611908 (client schema): m_eMatchMode 0xA8,
// m_unExpectedPlayerCount 0xB4, m_iWinningTeam 0xC0.
const MATCH_MODE: usize = 0xA8;
const PLAYERS: usize = 0xB4;
/// The winning team, -1 until the match is decided. Watched live at one read
/// a second, it goes from -1 to the winner in the same read as the state goes
/// from 7 (being played) to 8.
const WINNER: usize = 0xC0;
const BOSS_KILLS: usize = 0x220;
/// The rejuvenator credits each team holds: `m_iAmberRejuvCount` and
/// `m_iSapphireRejuvCount` in the client schema of build 25611908. Each
/// heavy hit on the dropped rejuvenator banks one credit for the hitter's
/// team, three between them, and they run out when spent on a revive or
/// after 180 s (generic_data's m_flRejuvinatorRebirthDuration). The game's
/// own top bar shows both teams' credits and the timer to everyone.
///
/// Both counts are read: either team can hold the credits, so one count
/// alone misses every rejuvenator the other team takes.
const AMBER_REJUV: usize = 0x224;
const SAPPHIRE_REJUV: usize = 0x228;
/// More credits than this is not a count.
const MAX_REJUV: u32 = 16;
const NEXT_BOSS: usize = 0x22C;
/// How much of the object is read each time.
pub const READ_BYTES: usize = NEXT_BOSS + 4;
/// `m_vecBannedHeroes`, the heroes banned in the draft: a vector of hero ids,
/// its count at +0 and its elements' address at +8. Offset from the game's
/// own client schema records, which give `m_flGameStartTime` as 0x60 like the
/// live measurement. Set from the match's first tick. Read separately, after
/// the timers' fields.
pub const BANS_AT: usize = 0xC8;
pub const BANS_BYTES: usize = 0x10;
/// A draft bans a handful; more than this is not the ban list.
pub const MAX_BANS: usize = 12;
/// Hero ids are small; anything above this is not one.
const MAX_HERO_ID: u32 = 1000;
/// The match clock's fields in build 25611908, read separately: they sit
/// 40 KB further on.
pub const CLOCK_AT: usize = 0x9FD4; // m_iPauseTeam; the tick and clock follow at +4 and +8
pub const CLOCK_BYTES: usize = 0x28;
const PAUSE_TEAM: usize = 0x00;
const CLOCK_TICK: usize = 0x04;
const CLOCK_VALUE: usize = 0x08;
const RESUME_AT: usize = 0x24;
const TICKS_PER_SECOND: f32 = 64.0;
/// The pre-game countdown starts the clock below zero.
const MIN_CLOCK: f32 = -300.0;
/// A match is well under a day of game time.
const MAX_GAME_TIME: f32 = 86_400.0;

/// What the timers need from the rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    pub game_start: f32,
    pub state: u32,
    pub boss_kills: u32,
    /// The rejuvenator credits held, Amber's then Sapphire's (see
    /// `AMBER_REJUV`).
    pub rejuv: [u32; 2],
    /// The winning team's number, once the match is decided (see `WINNER`).
    pub winner: Option<u8>,
    /// Match clock time the mid boss next spawns; 0 before its first kill.
    pub next_boss: f32,
}

impl Rules {
    /// The credits both teams hold between them: non-zero from the first
    /// credit banked until the last is spent or runs out.
    pub fn rejuv_held(&self) -> u32 {
        self.rejuv.iter().sum()
    }
}

/// The game's own match clock, as last set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clock {
    /// A team has the game paused, or an unpause is counting down.
    pub frozen: bool,
    /// The match clock at `game_time`.
    pub value: f32,
    /// The game time the clock was set at.
    pub game_time: f32,
}

impl Clock {
    /// The match clock at game time `now`.
    pub fn at(self, now: f32) -> f32 {
        if self.frozen {
            self.value
        } else {
            self.value + (now - self.game_time).max(0.0)
        }
    }
}

/// The clock from the [`CLOCK_BYTES`] at [`CLOCK_AT`], when every field reads as
/// a real one. A game patch that moves them reads as None, and the timers
/// fall back to the game time since the match started.
pub fn parse_clock(tail: &[u8]) -> Option<Clock> {
    let team = i32::from_le_bytes(tail.get(PAUSE_TEAM..PAUSE_TEAM + 4)?.try_into().ok()?);
    let tick = i32::from_le_bytes(tail.get(CLOCK_TICK..CLOCK_TICK + 4)?.try_into().ok()?);
    let value = f32_at(tail, CLOCK_VALUE)?;
    let resume_at = f32_at(tail, RESUME_AT)?;
    let game_time = tick as f32 / TICKS_PER_SECOND;
    let plausible = (-1..=16).contains(&team)
        && tick >= 0
        && game_time < MAX_GAME_TIME
        && (MIN_CLOCK..MAX_GAME_TIME).contains(&value)
        && value <= game_time
        && (resume_at == 0.0 || (0.0..MAX_GAME_TIME).contains(&resume_at));
    plausible.then_some(Clock {
        frozen: team != -1 || resume_at > 0.0,
        value,
        game_time,
    })
}

/// The match clock at game time `now`: the game's own when it reads true,
/// else the game time since the match started, which is right until the
/// first pause.
/// Returns the clock and whether it is frozen.
pub fn match_clock(now: f32, game_start: f32, clock: Option<Clock>) -> (f32, bool) {
    let simple = now - game_start;
    match clock {
        // A pause only ever puts the clock behind the simple one.
        Some(c) if c.at(now) <= simple + 1.0 => (c.at(now), c.frozen),
        _ => (simple, false),
    }
}

fn bounds_bytes() -> [u8; 24] {
    let mut out = [0_u8; 24];
    for (i, v) in BOUNDS.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn f32_at(buf: &[u8], off: usize) -> Option<f32> {
    u32_at(buf, off).map(f32::from_bits)
}

/// Where rules objects start in `piece`, which begins at `base` in the game.
/// Only a spot whose other fields read as rules is kept.
pub fn find(piece: &[u8], base: usize) -> Vec<usize> {
    let needle = bounds_bytes();
    memchr::memmem::find_iter(piece, &needle)
        .filter_map(|at| at.checked_sub(BOUNDS_AT))
        .filter(|start| {
            piece
                .get(*start..*start + READ_BYTES)
                .is_some_and(|obj| parse(obj).is_some())
        })
        .map(|start| base + start)
        .collect()
}

/// Why the object at each place the minimap bounds sit in `piece` was not
/// taken as the rules object, for the log when a pass finds none: the first
/// field that does not read as a real one, with its value.
pub fn misses(piece: &[u8]) -> Vec<String> {
    let needle = bounds_bytes();
    memchr::memmem::find_iter(piece, &needle)
        .map(|at| match at.checked_sub(BOUNDS_AT).and_then(|start| piece.get(start..start + READ_BYTES)) {
            None => "cut off by the end of the read".to_owned(),
            Some(obj) => why_not(obj).unwrap_or_else(|| "taken".to_owned()),
        })
        .collect()
}

/// The first field of a candidate that rules it out, or None when it is one.
fn why_not(obj: &[u8]) -> Option<String> {
    let f = |off| f32_at(obj, off).unwrap_or(f32::NAN);
    let u = |off| u32_at(obj, off).unwrap_or(u32::MAX);
    let checks: [(&str, bool, String); 7] = [
        ("game start", (0.0..MAX_GAME_TIME).contains(&f(GAME_START)), f(GAME_START).to_string()),
        ("state", u(STATE) <= 32, u(STATE).to_string()),
        ("mode", (1..=16).contains(&u(MATCH_MODE)), u(MATCH_MODE).to_string()),
        ("players", (1..=32).contains(&u(PLAYERS)), u(PLAYERS).to_string()),
        ("boss kills", u(BOSS_KILLS) <= 64, u(BOSS_KILLS).to_string()),
        (
            "rejuvenator credits",
            u(AMBER_REJUV) <= MAX_REJUV && u(SAPPHIRE_REJUV) <= MAX_REJUV,
            format!("{}/{}", u(AMBER_REJUV), u(SAPPHIRE_REJUV)),
        ),
        ("next boss", (0.0..MAX_GAME_TIME).contains(&f(NEXT_BOSS)), f(NEXT_BOSS).to_string()),
    ];
    checks.into_iter().find(|(_, ok, _)| !ok).map(|(name, _, value)| format!("{name} {value}"))
}

/// The rules from the object's first `READ_BYTES`, when every field reads
/// as a real one.
pub fn parse(obj: &[u8]) -> Option<Rules> {
    if obj.get(BOUNDS_AT..BOUNDS_AT + 24)? != bounds_bytes() {
        return None;
    }
    let rules = Rules {
        game_start: f32_at(obj, GAME_START)?,
        state: u32_at(obj, STATE)?,
        boss_kills: u32_at(obj, BOSS_KILLS)?,
        rejuv: [u32_at(obj, AMBER_REJUV)?, u32_at(obj, SAPPHIRE_REJUV)?],
        winner: u32_at(obj, WINNER).and_then(|w| u8::try_from(w).ok()).filter(|w| (2..=3).contains(w)),
        next_boss: f32_at(obj, NEXT_BOSS)?,
    };
    let mode = u32_at(obj, MATCH_MODE)?;
    let players = u32_at(obj, PLAYERS)?;
    let plausible = (0.0..MAX_GAME_TIME).contains(&rules.game_start)
        && rules.state <= 32
        && (1..=16).contains(&mode)
        && (1..=32).contains(&players)
        && rules.boss_kills <= 64
        && rules.rejuv.iter().all(|credits| *credits <= MAX_REJUV)
        && (0.0..MAX_GAME_TIME).contains(&rules.next_boss);
    plausible.then_some(rules)
}

/// The ban vector's count and elements' address, from the `BANS_BYTES` at
/// `BANS_AT`, when they read as one. A count of zero needs no address.
pub fn parse_bans_header(head: &[u8]) -> Option<(usize, usize)> {
    let count = usize::try_from(i32::from_le_bytes(head.get(0..4)?.try_into().ok()?)).ok()?;
    let at = usize::try_from(u64::from_le_bytes(head.get(8..16)?.try_into().ok()?)).ok()?;
    if count > MAX_BANS {
        return None;
    }
    if count == 0 {
        return Some((0, 0));
    }
    (0x10000..0x7FFF_FFFF_FFFF).contains(&at).then_some((count, at))
}

/// The banned hero ids from the vector's elements, when every one is a hero.
pub fn parse_ban_ids(elements: &[u8], count: usize) -> Option<Vec<u32>> {
    let ids: Vec<u32> = elements
        .get(..count * 4)?
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    ids.iter().all(|id| (1..=MAX_HERO_ID).contains(id)).then_some(ids)
}

#[cfg(test)]
mod misses_tests {
    use super::*;

    #[test]
    fn a_miss_names_the_field_that_ruled_the_candidate_out() {
        let mut obj = vec![0_u8; READ_BYTES + 16];
        obj[BOUNDS_AT..BOUNDS_AT + 24].copy_from_slice(&bounds_bytes());
        obj[GAME_START..GAME_START + 4].copy_from_slice(&12.5_f32.to_le_bytes());
        obj[STATE..STATE + 4].copy_from_slice(&7_u32.to_le_bytes());
        obj[MATCH_MODE..MATCH_MODE + 4].copy_from_slice(&1_u32.to_le_bytes());
        obj[PLAYERS..PLAYERS + 4].copy_from_slice(&12_u32.to_le_bytes());
        assert!(parse(&obj).is_some());
        assert_eq!(misses(&obj), vec!["taken".to_owned()]);
        obj[MATCH_MODE..MATCH_MODE + 4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(parse(&obj).is_none());
        assert_eq!(misses(&obj), vec!["mode 0".to_owned()]);
        obj[NEXT_BOSS..NEXT_BOSS + 4].copy_from_slice(&(-1.0_f32).to_le_bytes());
        assert_eq!(misses(&obj), vec!["mode 0".to_owned()], "the first failing field");
        assert_eq!(misses(&obj[BOUNDS_AT..]), vec!["cut off by the end of the read".to_owned()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ban_list_reads_from_its_vector() {
        let mut head = vec![0_u8; BANS_BYTES];
        head[0..4].copy_from_slice(&3_i32.to_le_bytes());
        head[8..16].copy_from_slice(&0x2a0_1234_5678_u64.to_le_bytes());
        assert_eq!(parse_bans_header(&head), Some((3, 0x2a0_1234_5678)));
        let ids: Vec<u8> = [25_u32, 81, 11].iter().flat_map(|i| i.to_le_bytes()).collect();
        assert_eq!(parse_ban_ids(&ids, 3), Some(vec![25, 81, 11]), "match 106235517's bans, from its demo");
        assert_eq!(parse_bans_header(&[0_u8; BANS_BYTES]), Some((0, 0)), "no bans: no address needed");
        head[0..4].copy_from_slice(&40_i32.to_le_bytes());
        assert_eq!(parse_bans_header(&head), None, "too many to be bans");
        head[0..4].copy_from_slice(&(-1_i32).to_le_bytes());
        assert_eq!(parse_bans_header(&head), None);
        head[0..4].copy_from_slice(&2_i32.to_le_bytes());
        head[8..16].copy_from_slice(&0x10_u64.to_le_bytes());
        assert_eq!(parse_bans_header(&head), None, "not an address");
        let junk: Vec<u8> = [25_u32, 1_634_037_861].iter().flat_map(|i| i.to_le_bytes()).collect();
        assert_eq!(parse_ban_ids(&junk, 2), None, "not hero ids");
        assert_eq!(parse_ban_ids(&ids, 4), None, "short read");
    }

    /// The object as the live game holds it, with the fields the timers use.
    fn object(start: f32, kills: u32, next: f32) -> Vec<u8> {
        let mut obj = vec![0_u8; READ_BYTES];
        obj[GAME_START..GAME_START + 4].copy_from_slice(&start.to_le_bytes());
        obj[STATE..STATE + 4].copy_from_slice(&7_u32.to_le_bytes());
        obj[BOUNDS_AT..BOUNDS_AT + 24].copy_from_slice(&bounds_bytes());
        obj[MATCH_MODE..MATCH_MODE + 4].copy_from_slice(&4_u32.to_le_bytes());
        obj[PLAYERS..PLAYERS + 4].copy_from_slice(&12_u32.to_le_bytes());
        obj[BOSS_KILLS..BOSS_KILLS + 4].copy_from_slice(&kills.to_le_bytes());
        obj[NEXT_BOSS..NEXT_BOSS + 4].copy_from_slice(&next.to_le_bytes());
        obj
    }

    #[test]
    fn the_rules_are_found_by_the_minimap_bounds_and_read() {
        let mut piece = vec![0xAB_u8; 0x40];
        piece.extend(object(58.219, 1, 1795.312));
        piece.extend([0_u8; 0x40]);
        assert_eq!(find(&piece, 0x1000), vec![0x1040]);
        let rules = parse(&piece[0x40..]).expect("rules");
        assert_eq!((rules.state, rules.boss_kills, rules.rejuv), (7, 1, [0, 0]));
        let mut held = object(58.219, 1, 1795.312);
        held[SAPPHIRE_REJUV..SAPPHIRE_REJUV + 4].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(parse(&held).map(|r| r.rejuv), Some([0, 3]));
        held[SAPPHIRE_REJUV..SAPPHIRE_REJUV + 4].copy_from_slice(&900_u32.to_le_bytes());
        assert_eq!(parse(&held), None, "not a count");
        // The winner: -1 until decided, then the team.
        let mut ended = object(58.219, 1, 1795.312);
        ended[WINNER..WINNER + 4].copy_from_slice(&(-1_i32).to_le_bytes());
        assert_eq!(parse(&ended).map(|r| r.winner), Some(None));
        ended[WINNER..WINNER + 4].copy_from_slice(&3_u32.to_le_bytes());
        ended[STATE..STATE + 4].copy_from_slice(&8_u32.to_le_bytes());
        assert_eq!(parse(&ended).map(|r| (r.state, r.winner)), Some((8, Some(3))));
        assert!((rules.game_start - 58.219).abs() < 1e-3);
        assert!((rules.next_boss - 1795.312).abs() < 1e-3);
    }

    /// The mid boss block as the client schema of build 25611908 lays it
    /// out: kills 0x220, Amber's credits 0x224, Sapphire's 0x228, the next
    /// spawn 0x22C. Credits held by Amber alone must still count.
    #[test]
    fn either_teams_rejuvenator_credits_are_read() {
        assert_eq!((BOSS_KILLS, AMBER_REJUV, SAPPHIRE_REJUV, NEXT_BOSS), (0x220, 0x224, 0x228, 0x22C));
        let mut amber = object(58.2, 1, 1460.0);
        amber[AMBER_REJUV..AMBER_REJUV + 4].copy_from_slice(&3_u32.to_le_bytes());
        let rules = parse(&amber).expect("rules");
        assert_eq!(rules.rejuv, [3, 0]);
        assert_eq!(rules.rejuv_held(), 3, "Amber's credits count");
        let mut split = amber.clone();
        split[AMBER_REJUV..AMBER_REJUV + 4].copy_from_slice(&2_u32.to_le_bytes());
        split[SAPPHIRE_REJUV..SAPPHIRE_REJUV + 4].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(parse(&split).map(|r| r.rejuv_held()), Some(3), "three credits between them");
        amber[AMBER_REJUV..AMBER_REJUV + 4].copy_from_slice(&900_u32.to_le_bytes());
        assert_eq!(parse(&amber), None, "not a count");
    }

    #[test]
    fn a_spot_that_only_shares_the_bounds_is_not_the_rules() {
        let mut obj = object(58.2, 0, 0.0);
        obj[MATCH_MODE..MATCH_MODE + 4].copy_from_slice(&999_u32.to_le_bytes());
        assert!(find(&obj, 0).is_empty());
        let mut obj = object(-5.0, 0, 0.0);
        obj[STATE] = 7;
        assert!(parse(&obj).is_none(), "a game start below zero");
    }

    fn tail(team: i32, tick: i32, value: f32, resume_at: f32) -> Vec<u8> {
        let mut t = vec![0_u8; CLOCK_BYTES];
        t[PAUSE_TEAM..PAUSE_TEAM + 4].copy_from_slice(&team.to_le_bytes());
        t[CLOCK_TICK..CLOCK_TICK + 4].copy_from_slice(&tick.to_le_bytes());
        t[CLOCK_VALUE..CLOCK_VALUE + 4].copy_from_slice(&value.to_le_bytes());
        t[RESUME_AT..RESUME_AT + 4].copy_from_slice(&resume_at.to_le_bytes());
        t
    }

    /// Values read from a live match, started at game time 57.97 and paused
    /// twice.
    #[test]
    fn the_match_clock_follows_the_games_own_through_two_pauses() {
        let start = 57.97;
        // The pre-game countdown: -30 at tick 1789, so 0:00 at the start.
        let pre = parse_clock(&tail(-1, 1789, -30.0, 0.0)).expect("clock");
        assert!((pre.at(start) - 0.02).abs() < 0.05);
        // First pause, at game time 200.7: frozen at 142.609 for its 23 s...
        let paused = parse_clock(&tail(3, 12837, 142.609, 0.0)).expect("clock");
        assert_eq!(match_clock(215.0, start, Some(paused)), (142.609, true));
        // ... and through the unpause countdown, the team already cleared.
        let counting = parse_clock(&tail(-1, 12837, 142.609, 226.406)).expect("clock");
        assert_eq!(match_clock(225.0, start, Some(counting)), (142.609, true));
        // Running again from tick 14490. The boss died at game time 1066.2:
        // match clock 16:22, and its next spawn, 1401.672, is 7:00 on.
        let running = parse_clock(&tail(-1, 14490, 142.609, 0.0)).expect("clock");
        let (clock, frozen) = match_clock(1066.2, start, Some(running));
        assert!(!frozen && (clock - 982.40).abs() < 0.05, "{clock}");
        assert!((1401.672 - clock - 419.3).abs() < 0.1);
        // After the second pause the game set 1530.719 itself at tick 104274;
        // the clock carried from the pause before it says the same.
        let second = parse_clock(&tail(-1, 95169, 1388.453, 0.0)).expect("clock");
        assert!((second.at(104_274.0 / 64.0) - 1530.719).abs() < 0.02);
        // The second kill, game time 1527.4: next spawn 1788.641 is 6:00 on.
        assert!((1788.641 - second.at(1527.4) - 359.8).abs() < 0.2);
    }

    #[test]
    fn a_clock_that_does_not_read_true_falls_back_to_the_game_time() {
        assert!(parse_clock(&tail(99, 100, 1.0, 0.0)).is_none(), "no such team");
        assert!(parse_clock(&tail(-1, -5, 1.0, 0.0)).is_none(), "a tick below zero");
        assert!(parse_clock(&tail(-1, 64, 500.0, 0.0)).is_none(), "a clock ahead of the game time");
        assert!(parse_clock(&[0_u8; 8]).is_none(), "too short");
        assert_eq!(match_clock(100.0, 58.0, None), (42.0, false));
        // A clock claiming more than the game time since the start is not it.
        let wild = Clock { frozen: false, value: 90.0, game_time: 99.0 };
        assert_eq!(match_clock(100.0, 58.0, Some(wild)), (42.0, false));
    }
}

