//! The players in the match the game is running: who they are, their team and
//! slot, hero, level, net worth, K/D/A and rank badge.
//!
//! Read-only, like the rest of this crate. The game keeps one object per
//! player, each holding that player's Steam id, name, team, lobby slot, hero,
//! level, net worth, K/D/A and rank badge. A Steam id has a shape nothing else
//! shares (its top half is always 0x01100001), so a player is found by that and
//! then confirmed by every other field being one a real player could have.
//! Nothing is reported when the confirmation fails, so a game patch that moves
//! these fields stops the reading rather than inventing numbers.
//!
//! The offsets belong to game build 25611908. They come from the game's own
//! client schema records (CBasePlayerController, CCitadelPlayerController,
//! PlayerDataGlobal_t) and were checked on the live game in a full match: one
//! object per player, with heroes, levels, net worth, rank badges and slots
//! matching what the game shows.

use serde::{Deserialize, Serialize};

/// The top half of every SteamID64 for an individual account.
const STEAM_HIGH: u32 = 0x0110_0001;
const STEAM_ID_BASE: u64 = 76_561_197_960_265_728;

/// Where each field sits from the start of a player object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offsets {
    pub team: usize,
    pub name: usize,
    pub steam_id: usize,
    pub player_data: usize,
    pub slot: usize,
}

impl Offsets {
    pub const CURRENT: Self = Self {
        team: 0x3EF,
        name: 0x6FC,
        steam_id: 0x788,
        player_data: 0x908,
        slot: 0xC51,
    };
}

/// The player's lane, on the controller itself: 1, 4 or 6 in a 6v6, the same
/// for both players of a lane pair, 0 in the hideout. Measured on the live
/// game in a full match: it is the one byte in the controller that pairs all
/// twelve players two by two per team.
///
/// That byte is `m_nOriginalLaneAssignment`, not `m_nAssignedLane`: the
/// client schema of build 25611908 has the pair at 0x838 and 0x839, followed
/// by `m_bBotDisconnectTakeover`, `m_bInTeamChat`, `m_bInPartyChat` and
/// `m_bLaneSwapLocked`. Read live, the six bytes from `m_nAssignedLane` on
/// are `00 lane 00 01 00 01`: `m_nAssignedLane` is 0 for every player, so
/// only the original assignment says the lane.
const LANE: usize = 0x839; // m_nOriginalLaneAssignment, build 25611908
const MAX_LANE: u8 = 16;
/// The entity's identity sits behind a pointer at 0x10 of every entity, and
/// its handle at 0x10 of that; the handle's low 15 bits are the entity index.
const IDENTITY_PTR: usize = 0x10;
const IDENTITY_HANDLE: usize = 0x10;
const MAX_PLAYER_INDEX: u16 = 64;

/// The game time on the controller: a float in seconds, rising one second a
/// second (measured on the live game).
pub const GAME_TIME: usize = 0x3C0;

/// Fields inside the player data block, from its start.
// Build 25611908: m_iGoldNetWorth 0x2C, m_iPlayerKills 0x58, m_iNPCKills 0x5C
// (new), m_iPlayerAssists 0x60, m_iDeaths 0x64, m_unPackedRank 0x340.
const LEVEL: usize = 0x08;
const HERO_ID: usize = 0x1C;
const NET_WORTH: usize = 0x2C;
const KILLS: usize = 0x58;
const ASSISTS: usize = 0x60;
const DEATHS: usize = 0x64;
const RANK_BADGE: usize = 0x340;
/// The ultimate and hero damage. Not measured on the live game: laid out from
/// the game's own schema for PlayerDataGlobal_t (SteamTracking's
/// GameTracking-Deadlock dump), whose field order with standard alignment
/// puts every field above at exactly the offset measured live, so these
/// follow from the same layout. Every one is checked for a shape it must
/// have, and a failed check leaves the ultimate unread rather than the player
/// unfound.
// Build 25611908 (schema): m_bUltimateTrained 0x7C, cooldown start 0x80 and
// end 0x84 (game time in seconds), m_bHasRejuvenator 0x88 opens four flags,
// m_iHeroDamage 0x8C.
// Client 6801 (20 Oct 2026, schema in client.dll on disk): Valve added
// m_flUltimateStoppedCooldownRemaining at 0x88, after the cooldown pair, so the
// four flags moved to 0x8C and m_iHeroDamage to 0x90. Every other field read
// here kept its offset.
const ULT_TRAINED: usize = 0x7C;
const ULT_START: usize = 0x80;
const ULT_END: usize = 0x84;
const ULT_FLAGS: usize = 0x8C;
const HERO_DAMAGE: usize = 0x90;
/// Game time runs from the map loading; a match is well under a day.
const MAX_GAME_TIME: f32 = 86_400.0;
/// The longest ultimate cooldown in the game is far below this.
const MAX_ULT_COOLDOWN: f32 = 600.0;
const MAX_HERO_DAMAGE: u32 = 5_000_000;

/// How much of an object is read: past the lobby slot, the last field used.
pub const OBJECT_BYTES: usize = 0xD00;
/// The name field is a fixed block of bytes, ending at the first zero.
const NAME_BYTES: usize = 128;
/// The teams are 2 and 3; 1 is a player the match has not placed.
const MIN_TEAM: u8 = 1;
const MAX_TEAM: u8 = 3;
const MAX_SLOT: u8 = 16;
const MAX_HERO_ID: u32 = 128;
const MAX_LEVEL: u32 = 100;
const MAX_NET_WORTH: u32 = 1_000_000;
const MAX_KDA: u32 = 200;
/// Ranks run to Eternus VI; anything above this is not a badge.
const MAX_BADGE: u8 = 130;

/// One player of the running match, as the game holds them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchPlayer {
    pub account_id: u32,
    pub name: String,
    pub team: u8,
    pub slot: u8,
    pub hero_id: u32,
    pub level: u32,
    pub net_worth: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    /// The game's own rank badge for this player, when it has one.
    pub rank_badge: Option<u8>,
    /// The lane the player was put in (1, 4 or 6 in a 6v6), None outside a
    /// match. The game's top bar shows each team lane by lane.
    pub lane: Option<u8>,
    /// The player's entity index, which orders the two players of a lane in
    /// the game's top bar. Read separately, as it sits behind a pointer.
    pub entity_index: Option<u16>,
    /// The game time as this object last had it, for the objective timers.
    #[serde(skip)]
    pub game_time: Option<f32>,
    /// Whether this is the player on this PC.
    pub is_us: bool,
    /// The ultimate's state, when its fields read as one. Left out of the
    /// published player list.
    #[serde(skip)]
    pub ult: Option<Ult>,
    /// Hero damage dealt this match, when it reads as a count. Reported with
    /// the match's end rather than in the player list.
    #[serde(skip)]
    pub hero_damage: Option<u32>,
}

/// A player's ultimate as the game keeps it for the scoreboard. The start
/// moves to the current game time whenever the ultimate is cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ult {
    pub trained: bool,
    /// Game time in milliseconds.
    pub start_ms: u32,
    pub end_ms: u32,
}

fn f32_at(buf: &[u8], off: usize) -> Option<f32> {
    u32_at(buf, off).map(f32::from_bits)
}

fn game_ms(seconds: f32) -> Option<u32> {
    (seconds.is_finite() && (0.0..=MAX_GAME_TIME).contains(&seconds))
        .then(|| (seconds * 1000.0).round() as u32)
}

/// The ultimate at a player data block, when the bytes have its shape: a
/// bool with zero padding, a start no later than its end, a cooldown no
/// longer than any in the game, and four bools after.
fn ult_at(buf: &[u8], data: usize) -> Option<Ult> {
    let trained = *buf.get(data + ULT_TRAINED)?;
    let padding = buf.get(data + ULT_TRAINED + 1..data + ULT_START)?;
    let flags = buf.get(data + ULT_FLAGS..data + ULT_FLAGS + 4)?;
    if trained > 1 || padding.iter().any(|b| *b != 0) || flags.iter().any(|b| *b > 1) {
        return None;
    }
    let start = f32_at(buf, data + ULT_START)?;
    let end = f32_at(buf, data + ULT_END)?;
    let (start_ms, end_ms) = (game_ms(start)?, game_ms(end)?);
    if start > end || end - start > MAX_ULT_COOLDOWN {
        return None;
    }
    Some(Ult {
        trained: trained == 1,
        start_ms,
        end_ms,
    })
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

/// A player object for a watched account that failed the limits, with what
/// failed. A player seen in one pass can drop out of later ones while still
/// in the lobby, and this says which field turned them away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub account_id: u32,
    pub why: String,
}

/// Why the object at `off` is not a player, for an account in `watch`. None
/// when it is a player, or the account is not watched, or the bytes do not
/// even hold a Steam id.
pub fn rejected_at(buf: &[u8], off: usize, offsets: Offsets, watch: &[u32]) -> Option<Rejected> {
    let steam_id = u64_at(buf, off + offsets.steam_id)?;
    let account_id = u32::try_from(steam_id.checked_sub(STEAM_ID_BASE)?).ok()?;
    if !watch.contains(&account_id) {
        return None;
    }
    let data = off + offsets.player_data;
    let mut why = Vec::new();
    let team = buf.get(off + offsets.team).copied();
    if team.is_none_or(|t| !(MIN_TEAM..=MAX_TEAM).contains(&t)) {
        why.push(format!("team {team:?}"));
    }
    let slot = buf.get(off + offsets.slot).copied();
    if slot.is_none_or(|s| s > MAX_SLOT) {
        why.push(format!("slot {slot:?}"));
    }
    if read_name(buf, off + offsets.name).is_none() {
        let head: Vec<u8> = buf
            .get(off + offsets.name..off + offsets.name + 8)
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        why.push(format!("name {head:?}"));
    }
    for (what, at, max) in [
        ("hero", HERO_ID, MAX_HERO_ID),
        ("level", LEVEL, MAX_LEVEL),
        ("net_worth", NET_WORTH, MAX_NET_WORTH),
        ("kills", KILLS, MAX_KDA),
        ("deaths", DEATHS, MAX_KDA),
        ("assists", ASSISTS, MAX_KDA),
    ] {
        let value = u32_at(buf, data + at);
        if value.is_none_or(|v| v > max) {
            why.push(format!("{what} {value:?}"));
        }
    }
    let badge = buf.get(data + RANK_BADGE).copied();
    if badge.is_some_and(|b| b > MAX_BADGE) {
        why.push(format!("badge {badge:?}"));
    }
    if buf.len() < off + OBJECT_BYTES {
        why.push(format!(
            "cut at {} of {OBJECT_BYTES} bytes",
            buf.len() - off
        ));
    }
    (!why.is_empty()).then(|| Rejected {
        account_id,
        why: why.join(", "),
    })
}

/// The player object starting at `off`, when every field is one a real player
/// could have. `our_account` marks which of them is this PC's player.
pub fn player_at(
    buf: &[u8],
    off: usize,
    offsets: Offsets,
    our_account: u32,
) -> Option<MatchPlayer> {
    let steam_id = u64_at(buf, off + offsets.steam_id)?;
    let account_id = u32::try_from(steam_id.checked_sub(STEAM_ID_BASE)?).ok()?;
    if u32::try_from(steam_id >> 32).ok()? != STEAM_HIGH {
        return None;
    }
    let team = *buf.get(off + offsets.team)?;
    let slot = *buf.get(off + offsets.slot)?;
    let data = off + offsets.player_data;
    let player = MatchPlayer {
        account_id,
        name: read_name(buf, off + offsets.name)?,
        team,
        slot,
        hero_id: u32_at(buf, data + HERO_ID)?,
        level: u32_at(buf, data + LEVEL)?,
        net_worth: u32_at(buf, data + NET_WORTH)?,
        kills: u32_at(buf, data + KILLS)?,
        deaths: u32_at(buf, data + DEATHS)?,
        assists: u32_at(buf, data + ASSISTS)?,
        rank_badge: buf.get(data + RANK_BADGE).copied().filter(|b| *b > 0),
        lane: buf
            .get(off + LANE)
            .copied()
            .filter(|l| (1..=MAX_LANE).contains(l)),
        entity_index: None,
        game_time: u32_at(buf, off + GAME_TIME)
            .map(f32::from_bits)
            .filter(|t| (0.0..MAX_GAME_TIME).contains(t)),
        is_us: account_id == our_account,
        ult: ult_at(buf, data),
        hero_damage: u32_at(buf, data + HERO_DAMAGE).filter(|d| *d <= MAX_HERO_DAMAGE),
    };
    let plausible = (MIN_TEAM..=MAX_TEAM).contains(&player.team)
        && player.slot <= MAX_SLOT
        && player.hero_id <= MAX_HERO_ID
        && player.level <= MAX_LEVEL
        && player.net_worth <= MAX_NET_WORTH
        && player.kills <= MAX_KDA
        && player.deaths <= MAX_KDA
        && player.assists <= MAX_KDA
        && player.rank_badge.is_none_or(|badge| badge <= MAX_BADGE);
    plausible.then_some(player)
}

/// The entity index of the player object at `at` (an address in the game),
/// read through `read` (address, buffer) -> bytes read. None when either read
/// fails or the index is not a player's.
pub fn entity_index(read: impl Fn(usize, &mut [u8]) -> usize, at: usize) -> Option<u16> {
    let mut ptr = [0_u8; 8];
    if read(at + IDENTITY_PTR, &mut ptr) != 8 {
        return None;
    }
    let identity = usize::try_from(u64::from_le_bytes(ptr)).ok()?;
    if identity < 0x10000 {
        return None;
    }
    let mut handle = [0_u8; 4];
    if read(identity + IDENTITY_HANDLE, &mut handle) != 4 {
        return None;
    }
    let index = u16::try_from(u32::from_le_bytes(handle) & 0x7FFF).ok()?;
    (1..=MAX_PLAYER_INDEX).contains(&index).then_some(index)
}

/// The name, up to its first zero. None when the bytes are not a name.
fn read_name(buf: &[u8], off: usize) -> Option<String> {
    let field = buf.get(off..off + NAME_BYTES)?;
    let end = field.iter().position(|b| *b == 0)?;
    let name = field.get(..end)?;
    if name.is_empty() || name.iter().any(|b| *b < 0x20) {
        return None;
    }
    std::str::from_utf8(name).ok().map(str::to_owned)
}

crate::scan::static_finder!(steam_high, &STEAM_HIGH.to_le_bytes());

/// Every player object in `buf`, as (offset, player). Steam ids are 8-aligned
/// inside the object, so only every eighth byte is tested, and the test itself
/// is the top half of a Steam id, which almost nothing else in memory carries.
pub fn find_players(buf: &[u8], offsets: Offsets, our_account: u32) -> Vec<(usize, MatchPlayer)> {
    find_players_watching(buf, offsets, our_account, &[], &mut Vec::new())
}

/// [`find_players`], also collecting why an object for a watched account was
/// turned away.
pub fn find_players_watching(
    buf: &[u8],
    offsets: Offsets,
    our_account: u32,
    watch: &[u32],
    rejected: &mut Vec<Rejected>,
) -> Vec<(usize, MatchPlayer)> {
    let mut out = Vec::new();
    // The top half of an 8-aligned word, found by the fast search (21 Oct
    // 2026: a compare per word cost 50 to 230 ms per GB; this finds the same
    // words). Overlapping hits are kept so an aligned one is never skipped.
    for high_at in crate::scan::all_at(steam_high(), buf) {
        if high_at % 8 != 4 {
            continue;
        }
        let at = high_at - 4;
        let Some(start) = at.checked_sub(offsets.steam_id) else {
            continue;
        };
        if let Some(player) = player_at(buf, start, offsets, our_account) {
            out.push((start, player));
        } else if !watch.is_empty()
            && let Some(reject) = rejected_at(buf, start, offsets, watch)
        {
            rejected.push(reject);
        }
    }
    out
}

/// How far into the match a copy of a player is. The game holds more than
/// one object per player, and a copy from the lobby or the loading screen
/// stays at level 1, 600 souls and 0/0/0 all match; the live one only ever
/// climbs. So of two copies the one further along is the one on screen.
fn liveliness(p: &MatchPlayer) -> (u32, u32, u32) {
    (p.level, p.net_worth, p.kills + p.deaths + p.assists)
}

/// One row per account, by team then lobby slot, keeping the copy furthest
/// into the match.
///
/// A lobby without this PC's own player is not this PC's match: the game keeps
/// the objects of a finished match in memory, so those would show a lobby the
/// player has already left. Nothing is returned for one.
pub fn merge_players(found: Vec<MatchPlayer>) -> Vec<MatchPlayer> {
    let mut out: Vec<MatchPlayer> = Vec::new();
    for player in found {
        match out.iter_mut().find(|p| p.account_id == player.account_id) {
            Some(existing) => {
                if liveliness(&player) >= liveliness(existing) {
                    *existing = player;
                }
            }
            None => out.push(player),
        }
    }
    if !out.iter().any(|p| p.is_us) {
        return Vec::new();
    }
    out.sort_by_key(|p| (p.team, p.slot));
    out
}

/// A player object laid out like the game's, so the reader is tested on the
/// offsets it ships with; the live game is what proves those offsets. The
/// worker's tests build their game memory from it too.
#[cfg(test)]
pub(crate) fn test_object(account_id: u32, name: &str, team: u8, slot: u8, hero: u32, badge: u8) -> Vec<u8> {
    let o = Offsets::CURRENT;
    let mut buf = vec![0_u8; OBJECT_BYTES];
    let steam = STEAM_ID_BASE + u64::from(account_id);
    buf[o.steam_id..o.steam_id + 8].copy_from_slice(&steam.to_le_bytes());
    buf[o.name..o.name + name.len()].copy_from_slice(name.as_bytes());
    buf[o.team] = team;
    buf[o.slot] = slot;
    let data = o.player_data;
    buf[data + HERO_ID..data + HERO_ID + 4].copy_from_slice(&hero.to_le_bytes());
    buf[data + LEVEL..data + LEVEL + 4].copy_from_slice(&9_u32.to_le_bytes());
    buf[data + NET_WORTH..data + NET_WORTH + 4].copy_from_slice(&12_345_u32.to_le_bytes());
    buf[data + KILLS..data + KILLS + 4].copy_from_slice(&7_u32.to_le_bytes());
    buf[data + DEATHS..data + DEATHS + 4].copy_from_slice(&3_u32.to_le_bytes());
    buf[data + ASSISTS..data + ASSISTS + 4].copy_from_slice(&11_u32.to_le_bytes());
    buf[data + RANK_BADGE] = badge;
    buf[data + ULT_TRAINED] = 1;
    buf[data + ULT_START..data + ULT_START + 4].copy_from_slice(&312.5_f32.to_le_bytes());
    buf[data + ULT_END..data + ULT_END + 4].copy_from_slice(&412.5_f32.to_le_bytes());
    buf[data + ULT_FLAGS] = 1;
    buf[data + HERO_DAMAGE..data + HERO_DAMAGE + 4].copy_from_slice(&23_456_u32.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::test_object as object;
    use super::*;

    /// The word-by-word loop the fast search replaced (to 21 Oct 2026).
    fn find_players_by_word(
        buf: &[u8],
        offsets: Offsets,
        our_account: u32,
        watch: &[u32],
        rejected: &mut Vec<Rejected>,
    ) -> Vec<(usize, MatchPlayer)> {
        let mut out = Vec::new();
        for (word, chunk) in buf.chunks_exact(8).enumerate() {
            let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            if high != STEAM_HIGH {
                continue;
            }
            let at = word * 8;
            let Some(start) = at.checked_sub(offsets.steam_id) else {
                continue;
            };
            if let Some(player) = player_at(buf, start, offsets, our_account) {
                out.push((start, player));
            } else if !watch.is_empty()
                && let Some(reject) = rejected_at(buf, start, offsets, watch)
            {
                rejected.push(reject);
            }
        }
        out
    }

    /// The fast search finds every player and every turned-away object the
    /// word loop did: random memory crowded with Steam id halves at every
    /// alignment, overlapping runs of them, and real objects among them.
    #[test]
    fn the_fast_search_finds_what_the_word_loop_found() {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20_usize {
            let len = 64 * 1024 + (round * 8) + (round % 7);
            let mut buf: Vec<u8> = (0..len).map(|_| (next() >> 24) as u8).collect();
            let high = STEAM_HIGH.to_le_bytes();
            for _ in 0..400 {
                let at = usize::try_from(next() % (len as u64 - 8)).unwrap();
                buf[at..at + 4].copy_from_slice(&high);
            }
            // 01 00 10 01 00 10 01: overlapping halves.
            for _ in 0..40 {
                let at = usize::try_from(next() % (len as u64 - 8)).unwrap();
                buf[at..at + 7].copy_from_slice(&[0x01, 0x00, 0x10, 0x01, 0x00, 0x10, 0x01]);
            }
            for (i, account) in [11_u32, 22, 33, 44].into_iter().enumerate() {
                let obj = object(account, "name", 2, u8::try_from(i + 1).unwrap(), 7, 83);
                let at = 8 * (1000 + 900 * i) + (round % 3) * 8;
                if at + obj.len() <= len {
                    buf[at..at + obj.len()].copy_from_slice(&obj);
                }
            }
            for our in [33_u32, 1] {
                let (mut a, mut b) = (Vec::new(), Vec::new());
                let fast = find_players_watching(&buf, Offsets::CURRENT, our, &[11, 44, 99], &mut a);
                let slow = find_players_by_word(&buf, Offsets::CURRENT, our, &[11, 44, 99], &mut b);
                assert_eq!(fast, slow, "round {round}, ours {our}");
                assert_eq!(a, b, "round {round}, ours {our}: turned away");
                assert!(our != 33 || fast.len() >= 3, "round {round}: the planted players were found");
            }
        }
    }

    #[test]
    fn the_ultimate_and_hero_damage_read_when_they_have_their_shape() {
        let o = Offsets::CURRENT;
        let data = o.player_data;
        let buf = object(1, "Name", 2, 3, 7, 70);
        let player = player_at(&buf, 0, o, 1).expect("a player");
        assert_eq!(
            player.ult,
            Some(Ult { trained: true, start_ms: 312_500, end_ms: 412_500 })
        );
        assert_eq!(player.hero_damage, Some(23_456));

        // Not yet cast: both times zero is an ultimate at rest.
        let mut rest = object(1, "Name", 2, 3, 7, 70);
        rest[data + ULT_START..data + ULT_END + 4].fill(0);
        assert_eq!(player_at(&rest, 0, o, 1).unwrap().ult, Some(Ult { trained: true, start_ms: 0, end_ms: 0 }));

        // Anything out of shape leaves the ultimate unread, never the player.
        let bad: [(usize, &[u8]); 6] = [
            (data + ULT_TRAINED, &[2]),
            (data + ULT_TRAINED + 1, &[9]),
            (data + ULT_FLAGS + 2, &[7]),
            (data + ULT_START, &f32::NAN.to_le_bytes()),
            (data + ULT_START, &500.0_f32.to_le_bytes()),
            (data + ULT_END, &5000.0_f32.to_le_bytes()),
        ];
        for (at, bytes) in bad {
            let mut buf = object(1, "Name", 2, 3, 7, 70);
            buf[at..at + bytes.len()].copy_from_slice(bytes);
            let player = player_at(&buf, 0, o, 1).expect("still a player");
            assert_eq!(player.ult, None, "bytes {bytes:?} at {at:#X}");
        }
        let mut damage = object(1, "Name", 2, 3, 7, 70);
        damage[data + HERO_DAMAGE..data + HERO_DAMAGE + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(player_at(&damage, 0, o, 1).unwrap().hero_damage, None);
    }

    /// Run by hand in a match on a test account, never a main one:
    /// `DEADCHAPS_PROBE_ACCOUNT=<test account> cargo test --release live_ult_fields -- --ignored --nocapture`
    /// Prints the player's own ultimate and hero damage every two seconds for
    /// two minutes, to check against a cast and the scoreboard. Read-only.
    #[cfg(windows)]
    #[test]
    #[ignore = "reads the running game, by hand only"]
    fn live_ult_fields() {
        let account: u32 = std::env::var("DEADCHAPS_PROBE_ACCOUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .expect("DEADCHAPS_PROBE_ACCOUNT names the test account");
        assert_ne!(account, 106_448_300, "memory tests run on the test account only");
        let pid = crate::game_process::game_pid("deadlock.exe").expect("the game is running");
        let game = super::super::process::GameProcess::open(pid).expect("the game opens for reading");
        for _ in 0..60 {
            let mut found = Vec::new();
            for region in game.private_regions() {
                let mut buf = vec![0_u8; region.size.min(512 << 20)];
                let got = game.read(region.base, &mut buf);
                buf.truncate(got);
                found.extend(find_players(&buf, Offsets::CURRENT, account).into_iter().map(|(_, p)| p));
            }
            match merge_players(found).into_iter().find(|p| p.is_us) {
                Some(us) => eprintln!(
                    "hero {} k/d {}/{} ult {:?} hero damage {:?}",
                    us.hero_id, us.kills, us.deaths, us.ult, us.hero_damage
                ),
                None => eprintln!("our player not found"),
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    /// Each player's team, slot, lane, entity index and souls, to check the
    /// game's top bar against after a patch: both teams run lane 6, 4, 1 left
    /// to right, and a lane's two players by entity index. Run by hand like
    /// `live_ult_fields`.
    #[cfg(windows)]
    #[test]
    #[ignore = "reads the running game, by hand only"]
    fn live_lane_fields() {
        let account: u32 = std::env::var("DEADCHAPS_PROBE_ACCOUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .expect("DEADCHAPS_PROBE_ACCOUNT names the test account");
        assert_ne!(account, 106_448_300, "memory tests run on the test account only");
        let pid = crate::game_process::game_pid("deadlock.exe").expect("the game is running");
        let game = super::super::process::GameProcess::open(pid).expect("the game opens for reading");
        for region in game.private_regions() {
            let mut buf = vec![0_u8; region.size.min(512 << 20)];
            let got = game.read(region.base, &mut buf);
            buf.truncate(got);
            for (at, p) in find_players(&buf, Offsets::CURRENT, account) {
                let index = entity_index(|a, b| game.read(a, b), region.base + at);
                eprintln!(
                    "LANE t{} s{} hero {} lane {:?} index {:?} souls {} us {}",
                    p.team, p.slot, p.hero_id, p.lane, index, p.net_worth, p.is_us
                );
            }
        }
    }

    /// The lane block as the live game holds it for every player: from
    /// `m_nAssignedLane` on, `00 lane 00 01 00 01`, at 0x838 in build
    /// 25611908. The lane is the second byte; the first is 0 for everyone.
    #[test]
    fn the_lane_is_read_from_the_byte_the_game_fills() {
        let o = Offsets::CURRENT;
        let block_at = 0x838;
        for lane in [1_u8, 4, 6] {
            let mut buf = object(11, "Ally", 2, 2, 7, 70);
            buf[block_at..block_at + 6].copy_from_slice(&[0, lane, 0, 1, 0, 1]);
            let player = player_at(&buf, 0, o, 11).expect("a player");
            assert_eq!(player.lane, Some(lane), "lane {lane} from the original assignment");
        }
        let mut zero = object(11, "Ally", 2, 2, 7, 70);
        zero[block_at..block_at + 6].copy_from_slice(&[0, 0, 0, 1, 0, 1]);
        assert_eq!(player_at(&zero, 0, o, 11).unwrap().lane, None, "the hideout has no lane");
    }

    #[test]
    fn a_players_entity_index_is_read_through_its_identity() {
        // The object at 0x1000 points at an identity at 0x20000, whose handle
        // (serial 0x9B, index 8) sits 0x10 in.
        let read = |addr: usize, buf: &mut [u8]| -> usize {
            let src: Vec<u8> = match addr {
                0x1010 => 0x20000_u64.to_le_bytes().to_vec(),
                0x20010 => 0x009B_8008_u32.to_le_bytes().to_vec(),
                _ => return 0,
            };
            buf.copy_from_slice(&src[..buf.len()]);
            buf.len()
        };
        assert_eq!(entity_index(read, 0x1000), Some(8));
        assert_eq!(entity_index(read, 0x2000), None, "an unreadable pointer is no index");
        let bad = |addr: usize, buf: &mut [u8]| -> usize {
            let src: Vec<u8> = match addr {
                0x1010 => 0x20000_u64.to_le_bytes().to_vec(),
                0x20010 => 0x0000_0FFF_u32.to_le_bytes().to_vec(),
                _ => return 0,
            };
            buf.copy_from_slice(&src[..buf.len()]);
            buf.len()
        };
        assert_eq!(entity_index(bad, 0x1000), None, "4095 is not a player's index");
    }

    #[test]
    fn a_player_object_reads_as_the_game_shows_it() {
        let buf = object(1_161_088_555, "Peepo", 2, 5, 7, 76);
        let player = player_at(&buf, 0, Offsets::CURRENT, 1_161_088_555).expect("a player");
        assert_eq!(
            (
                player.account_id,
                player.team,
                player.slot,
                player.hero_id,
                player.rank_badge,
                player.is_us
            ),
            (1_161_088_555, 2, 5, 7, Some(76), true)
        );
        assert_eq!(
            (
                player.kills,
                player.deaths,
                player.assists,
                player.net_worth
            ),
            (7, 3, 11, 12_345)
        );
        assert_eq!(player.name, "Peepo");
        assert_eq!(player.lane, None, "no lane written, none read");
        let mut laned = buf.clone();
        laned[LANE] = 4;
        assert_eq!(
            player_at(&laned, 0, Offsets::CURRENT, 1_161_088_555).and_then(|p| p.lane),
            Some(4)
        );
        // Somebody else's object is theirs, not ours.
        let other = object(106_448_300, "Someone", 3, 11, 52, 71);
        let them = player_at(&other, 0, Offsets::CURRENT, 1_161_088_555).expect("a player");
        assert!(!them.is_us);
        assert_eq!(them.rank_badge, Some(71));
    }

    #[test]
    fn a_field_out_of_range_is_not_a_player() {
        let o = Offsets::CURRENT;
        for (at, value) in [
            (o.team, 9_u8),
            (o.slot, 40),
            (o.player_data + RANK_BADGE, 200),
        ] {
            let mut buf = object(1, "Name", 2, 3, 7, 70);
            buf[at] = value;
            assert!(
                player_at(&buf, 0, o, 1).is_none(),
                "offset {at:#X} = {value} should fail"
            );
        }
        let mut hero = object(1, "Name", 2, 3, 7, 70);
        hero[o.player_data + HERO_ID..o.player_data + HERO_ID + 4]
            .copy_from_slice(&999_u32.to_le_bytes());
        assert!(player_at(&hero, 0, o, 1).is_none());
        // A name that is not text, and an empty name.
        let mut name = object(1, "Name", 2, 3, 7, 70);
        name[o.name] = 0x01;
        assert!(player_at(&name, 0, o, 1).is_none());
        let mut blank = object(1, "Name", 2, 3, 7, 70);
        blank[o.name..o.name + 4].fill(0);
        assert!(player_at(&blank, 0, o, 1).is_none());
        // Zeros are not a lobby of bots.
        assert!(find_players(&vec![0_u8; OBJECT_BYTES * 2], o, 1).is_empty());
    }

    #[test]
    fn players_are_found_wherever_they_sit_and_sorted_by_team_then_slot() {
        let mut buf = vec![0xAB_u8; 4096];
        buf.extend(object(11, "Ally", 2, 2, 7, 70));
        buf.extend(vec![0_u8; 1024]);
        buf.extend(object(22, "Enemy", 3, 9, 52, 80));
        buf.extend(object(33, "Us", 2, 1, 13, 76));
        let found = find_players(&buf, Offsets::CURRENT, 33);
        assert_eq!(found.len(), 3, "every object is found wherever it sits");
        let merged = merge_players(found.into_iter().map(|(_, p)| p).collect());
        assert_eq!(
            merged
                .iter()
                .map(|p| (p.team, p.slot, p.account_id))
                .collect::<Vec<_>>(),
            [(2, 1, 33), (2, 2, 11), (3, 9, 22)]
        );
        assert!(merged.iter().filter(|p| p.is_us).count() == 1);
    }

    #[test]
    fn the_copy_further_into_the_match_wins_whichever_was_read_last() {
        let first = player_at(&object(11, "Us", 2, 2, 7, 70), 0, Offsets::CURRENT, 11).unwrap();
        let mut later = first.clone();
        later.kills = 12;
        let merged = merge_players(vec![first.clone(), later.clone()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].kills, 12);
        // The stale lobby copy read after the live one does not win.
        let mut fresh = first.clone();
        fresh.level = 1;
        fresh.net_worth = 600;
        fresh.kills = 0;
        fresh.deaths = 0;
        fresh.assists = 0;
        let merged = merge_players(vec![later, fresh]);
        assert_eq!(merged[0].kills, 12);
    }

    #[test]
    fn a_lobby_without_us_is_a_match_we_have_left() {
        // The game keeps a finished match's objects, so a lobby without our
        // own player in it is not the match on screen.
        let them = player_at(&object(11, "Ally", 2, 2, 7, 70), 0, Offsets::CURRENT, 33).unwrap();
        let also = player_at(&object(22, "Enemy", 3, 9, 52, 80), 0, Offsets::CURRENT, 33).unwrap();
        assert!(merge_players(vec![them.clone(), also.clone()]).is_empty());
        let us = player_at(&object(33, "Us", 2, 1, 13, 76), 0, Offsets::CURRENT, 33).unwrap();
        assert_eq!(merge_players(vec![them, also, us]).len(), 3);
    }

    /// The reasons name the field: a level past the limit, a name that is
    /// not text, and nothing for a player that passes.
    #[test]
    fn a_watched_account_says_why_it_was_turned_away() {
        let o = Offsets::CURRENT;
        let mut buf = object(44, "Other", 2, 0, 7, 0);
        assert_eq!(rejected_at(&buf, 0, o, &[44]), None, "a player passes");
        assert_eq!(rejected_at(&buf, 0, o, &[45]), None, "not watched");
        buf[o.player_data + LEVEL..o.player_data + LEVEL + 4]
            .copy_from_slice(&300_u32.to_le_bytes());
        buf[o.name] = 1;
        let reject = rejected_at(&buf, 0, o, &[44]).unwrap();
        assert_eq!(reject.account_id, 44);
        assert!(reject.why.contains("level Some(300)"), "{}", reject.why);
        assert!(reject.why.contains("name [1"), "{}", reject.why);
        let mut rejected = Vec::new();
        assert!(find_players_watching(&buf, o, 1, &[44], &mut rejected).is_empty());
        assert_eq!(rejected.len(), 1);
    }
}
