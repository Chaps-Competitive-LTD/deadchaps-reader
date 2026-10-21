//! Friends' Steam rich presence as the running game holds it, so the Companion
//! can show what each friend is doing in game, including matches that public
//! match lists do not cover.
//!
//! The game keeps one record per Steam friend and refreshes it from Steam,
//! as read from client.dll: the function at client.dll RVA 0x5e0ee0 asks
//! `GetFriendRichPresence(friend, "steam_display")` and stores the answer when
//! it differs. The record:
//!
//! | offset | what |
//! |---|---|
//! | 0x00 | u64 SteamID64 |
//! | 0x08 | u32 relationship (3: friends) |
//! | 0x0c | u32 persona state (0 offline .. 7) |
//! | 0x10 | char* persona name |
//! | 0x18 | i32 status bucket (-1 .. 4) |
//! | 0x20 | char* rich presence, as Steam localised it |
//! | 0x28 | u32 app id being played (24 bits) |
//! | 0x2c | u8 can_spectate (`GetFriendRichPresence(friend, "can_spectate")`) |
//!
//! The text is Steam's own, localised with the game's rich presence file
//! (`citadel_gc_english.txt`, `Steam_Citadel_*`): "Ranked: Bebop (21 min)",
//! "Deadlock: Wraith (20 min)", "In a Party Hideout".
//!
//! Copies of old records stay in freed memory and look the same, so no record
//! is trusted until it is seen to change. Steam moves an in-match friend's
//! minutes every minute, and only a record whose text changed recently counts
//! as live (see [`Tracker`]).

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What a friend is doing, according to their rich presence line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    /// `in_match`, `hideout`, `main_menu`, `finding_match`, `spectating`,
    /// `sandbox`, `online`.
    pub state: Cow<'static, str>,
    /// The match's mode as a short key (`ranked`, `unranked`, `brawl`,
    /// `private`, ...). Set only in a match or in sandbox.
    pub mode: Option<Cow<'static, str>>,
    /// The hero's English name as the line gives it ("Wraith", "Rat King").
    pub hero: Option<String>,
    /// Minutes the match has run, as the line gives them.
    pub minutes: Option<u32>,
}

/// The game's labels for a match's mode (`Steam_Citadel_RP_MM_*`,
/// `Steam_Citadel_RP_StreetBrawl`), each with the short key for it.
const MODES: [(&str, &str); 9] = [
    ("Deadlock:", "unranked"),
    ("Ranked:", "ranked"),
    ("Street Brawl:", "brawl"),
    ("Lobby:", "private"),
    ("Hero Labs:", "herolabs"),
    ("Bots:", "bots"),
    ("Tutorial:", "tutorial"),
    ("Sandbox:", "sandbox"),
    ("Placement Match:", "calibration"),
];

/// Parses one rich presence line. None for anything that is not one of the
/// game's lines, so text that merely looks alike is not taken for one.
pub fn parse(line: &str) -> Option<Presence> {
    let simple = |state: &'static str| {
        Some(Presence {
            state: state.into(),
            mode: None,
            hero: None,
            minutes: None,
        })
    };
    match line {
        "Main Menu" => return simple("main_menu"),
        "In a Party Hideout" => return simple("hideout"),
        "Finding Match" => return simple("finding_match"),
        "Spectating a Match" => return simple("spectating"),
        "Online" => return simple("online"),
        _ => {}
    }
    // `Steam_Citadel_Hideout_*`: every variant names the Hideout.
    if line.ends_with(" the Hideout")
        || line.ends_with(" in the Hideout")
        || line.starts_with("Wishing the Hideout")
    {
        return simple("hideout");
    }
    // `Steam_Citadel_InGame_Sandbox`: "Testing {hero} in Sandbox Mode".
    if let Some(hero) = line
        .strip_prefix("Testing ")
        .and_then(|rest| rest.strip_suffix(" in Sandbox Mode"))
    {
        return Some(Presence {
            state: "sandbox".into(),
            mode: Some("sandbox".into()),
            hero: hero_name(hero).map(str::to_owned),
            minutes: None,
        });
    }
    // `Steam_Citadel_InGame_Hero`: "{mode} {hero} ({minutes} min)", and
    // `Steam_Citadel_InGame`: "{mode}  ({minutes} min)" with no hero.
    let (head, minutes) = line.strip_suffix(" min)")?.rsplit_once(" (")?;
    if minutes.is_empty() || minutes.len() > 3 || !minutes.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minutes: u32 = minutes.parse().ok()?;
    let (label, mode) = MODES.iter().find(|(label, _)| head.starts_with(label))?;
    let hero = head[label.len()..].trim();
    let hero = if hero.is_empty() {
        None
    } else {
        Some(hero_name(hero)?.to_owned())
    };
    Some(Presence {
        state: "in_match".into(),
        mode: Some((*mode).into()),
        hero,
        minutes: Some(minutes),
    })
}

/// A hero's name as the line can carry it: letters, spaces, `&`, `'`, `.`
/// and `-` ("Lady Geist", "Mo & Krill", "Rat King"), 2 to 24 of them.
fn hero_name(name: &str) -> Option<&str> {
    let ok = (2..=24).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | '&' | '\'' | '.' | '-'));
    ok.then_some(name)
}

/// How many bytes of a friend record are read.
pub const RECORD_BYTES: usize = 0x30;
/// The high half of an individual account's SteamID64 (universe 1, type 1,
/// instance 1).
const STEAM_HIGH: u32 = 0x0110_0001;
const RELATIONSHIP_FRIEND: u32 = 3;
/// How much of the text is read: the longest line ("Figuring Out What
/// Happened Last Night in the Hideout") is 52 bytes.
pub const TEXT_BYTES: usize = 64;

/// A friend record's fields that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub account_id: u32,
    /// Where the rich presence text sits; 0 for none.
    pub text_at: usize,
}

fn user_pointer(p: u64) -> bool {
    (0x1_0000..0x7FFF_FFFF_FFFF).contains(&p)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | (u64::from(u32_at(b, at + 4)) << 32)
}

/// The record at the start of `b`, when every field reads the way the game
/// writes it.
pub fn record_at(b: &[u8]) -> Option<Record> {
    if b.len() < RECORD_BYTES || u32_at(b, 4) != STEAM_HIGH {
        return None;
    }
    let account_id = u32_at(b, 0);
    let relationship = u32_at(b, 8);
    let persona_state = u32_at(b, 0x0c);
    let name = u64_at(b, 0x10);
    let bucket = u32_at(b, 0x18).cast_signed();
    let text = u64_at(b, 0x20);
    let app = u32_at(b, 0x28);
    let spectate = b[0x2c];
    let ok = account_id != 0
        && relationship == RELATIONSHIP_FRIEND
        && persona_state <= 7
        && (-1..=8).contains(&bucket)
        && user_pointer(name)
        // A null text pointer is allowed: an offline friend's line is empty,
        // and an empty string may have no buffer. The record is still
        // followed, so the friend shows up as soon as they start playing
        // rather than at the next full pass.
        && (text == 0 || user_pointer(text))
        && app < 1 << 24
        && spectate <= 1;
    ok.then(|| Record {
        account_id,
        text_at: usize::try_from(text).unwrap_or(0),
    })
}

crate::scan::static_finder!(steam_high, &STEAM_HIGH.to_le_bytes());

/// Every record in a piece read from `base`, with its address. Records sit
/// at 8-byte boundaries.
pub fn find_records(piece: &[u8], base: usize) -> Vec<(usize, Record)> {
    let mut out = Vec::new();
    // The SteamID's high half is the rare part: look for it, then check the
    // record around it.
    for at in crate::scan::all_at(steam_high(), piece) {
        let Some(start) = at.checked_sub(4) else {
            continue;
        };
        if !(base + start).is_multiple_of(8) || start + RECORD_BYTES > piece.len() {
            continue;
        }
        if let Some(record) = record_at(&piece[start..]) {
            out.push((base + start, record));
        }
    }
    out
}

/// The text a record points at, up to its NUL; None when it is not text.
pub fn text_of(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&bytes[..end]).ok()
}

/// One friend's live presence, as the reader reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendPresence {
    pub account_id: u32,
    #[serde(flatten)]
    pub presence: Presence,
    /// When the line was seen to change to its current text, in unix seconds.
    pub changed_unix: u64,
}

/// How long a record stays live after its text was last seen to change.
/// Steam moves an in-match friend's minutes every minute, so a live record
/// changes at least that often; two and a half minutes allows for a missed
/// read or a slow tick.
pub const LIVE_FOR: Duration = Duration::from_secs(150);

#[derive(Debug, Clone)]
struct Seen {
    account_id: u32,
    text_at: usize,
    text: String,
    /// When the text was last seen to differ from the read before. None
    /// until it has: the first read proves nothing about a record.
    changed: Option<Instant>,
    /// The same moment in unix seconds, for [`FriendPresence::changed_unix`].
    changed_unix: u64,
}

/// The records found by the last full pass, which are re-read on a shorter
/// interval between passes, and what each read returned.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<usize, Seen>,
}

impl Tracker {
    /// Applies the records a full pass found: records already followed keep
    /// their history, and records the pass did not find are dropped. New
    /// records start on their first [`Tracker::observe`].
    pub fn keep_only(&mut self, spots: &[usize]) {
        self.seen.retain(|at, _| spots.contains(at));
    }

    pub fn spots(&self) -> Vec<usize> {
        self.seen.keys().copied().collect()
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }

    /// Records one read of the record at `at`: what [`record_at`] made of it
    /// and the text it pointed at. A record that no longer reads as one, or
    /// now belongs to a different friend, starts over.
    pub fn observe(
        &mut self,
        at: usize,
        read: Option<(Record, String)>,
        now: Instant,
        now_unix: u64,
    ) {
        let Some((record, text)) = read else {
            self.seen.remove(&at);
            return;
        };
        match self.seen.get_mut(&at) {
            Some(s) if s.account_id == record.account_id => {
                if s.text != text || s.text_at != record.text_at {
                    s.changed = Some(now);
                    s.changed_unix = now_unix;
                    s.text = text;
                    s.text_at = record.text_at;
                }
            }
            _ => {
                self.seen.insert(
                    at,
                    Seen {
                        account_id: record.account_id,
                        text_at: record.text_at,
                        text,
                        changed: None,
                        changed_unix: 0,
                    },
                );
            }
        }
    }

    /// Records followed, and how many of them are live, for the log.
    pub fn counts(&self, now: Instant) -> (usize, usize) {
        let live = self.seen.values().filter(|s| is_live(s, now)).count();
        (self.seen.len(), live)
    }

    /// Each friend whose record changed within [`LIVE_FOR`], taken from the
    /// record that changed last. A text that records of two different friends
    /// point at is left out for both, since one of them is a freed copy
    /// pointing at memory the live one reuses.
    pub fn live(&self, now: Instant) -> Vec<FriendPresence> {
        let mut owner: HashMap<usize, u32> = HashMap::new();
        let mut shared = Vec::new();
        for s in self.seen.values().filter(|s| s.text_at != 0) {
            if let Some(other) = owner.insert(s.text_at, s.account_id)
                && other != s.account_id
            {
                shared.push(s.text_at);
            }
        }
        let mut best: HashMap<u32, &Seen> = HashMap::new();
        for s in self.seen.values() {
            if !is_live(s, now) || shared.contains(&s.text_at) {
                continue;
            }
            if best
                .get(&s.account_id)
                .is_none_or(|b| s.changed > b.changed)
            {
                best.insert(s.account_id, s);
            }
        }
        let mut out: Vec<FriendPresence> = best
            .into_values()
            .filter_map(|s| {
                Some(FriendPresence {
                    account_id: s.account_id,
                    presence: parse(&s.text)?,
                    changed_unix: s.changed_unix,
                })
            })
            .collect();
        out.sort_by_key(|p| p.account_id);
        out
    }
}

fn is_live(s: &Seen, now: Instant) -> bool {
    s.changed
        .is_some_and(|at| now.saturating_duration_since(at) <= LIVE_FOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_lines_give_mode_hero_and_minutes() {
        let p = parse("Deadlock: Wraith (20 min)").unwrap();
        assert_eq!(
            (p.state.as_ref(), p.mode.as_deref(), p.hero.as_deref(), p.minutes),
            ("in_match", Some("unranked"), Some("Wraith"), Some(20))
        );
        let p = parse("Ranked: Rat King (28 min)").unwrap();
        assert_eq!(
            (p.mode.as_deref(), p.hero.as_deref(), p.minutes),
            (Some("ranked"), Some("Rat King"), Some(28))
        );
        let p = parse("Street Brawl: Mo & Krill (3 min)").unwrap();
        assert_eq!(
            (p.mode.as_deref(), p.hero.as_deref()),
            (Some("brawl"), Some("Mo & Krill"))
        );
        let p = parse("Placement Match: Lady Geist (0 min)").unwrap();
        assert_eq!((p.mode.as_deref(), p.minutes), (Some("calibration"), Some(0)));
        // `Steam_Citadel_InGame`: no hero, two spaces.
        let p = parse("Lobby:  (12 min)").unwrap();
        assert_eq!(
            (p.mode.as_deref(), p.hero, p.minutes),
            (Some("private"), None, Some(12))
        );
    }

    #[test]
    fn the_other_lines_give_a_state_only() {
        assert_eq!(parse("Main Menu").unwrap().state, "main_menu");
        assert_eq!(parse("In a Party Hideout").unwrap().state, "hideout");
        assert_eq!(
            parse("Ignoring Lash in the Hideout").unwrap().state,
            "hideout"
        );
        assert_eq!(parse("Investigating the Hideout").unwrap().state, "hideout");
        assert_eq!(
            parse("Wishing the Hideout was The Cube").unwrap().state,
            "hideout"
        );
        assert_eq!(parse("Finding Match").unwrap().state, "finding_match");
        assert_eq!(parse("Spectating a Match").unwrap().state, "spectating");
        let p = parse("Testing Vyper in Sandbox Mode").unwrap();
        assert_eq!((p.state.as_ref(), p.hero.as_deref()), ("sandbox", Some("Vyper")));
    }

    #[test]
    fn look_alikes_are_not_presence() {
        for line in [
            "Deadlock: Wraith (20 mins)",
            "Deadlock: Wraith (1234 min)",
            "Unknown: Wraith (20 min)",
            "Deadlock: Wr@ith (20 min)",
            "Deadlock: Wraith ( min)",
            "Jab Jabich",
            "",
        ] {
            assert_eq!(parse(line), None, "{line}");
        }
    }

    /// A friend record as the game lays it out (see the module comment).
    fn record(account_id: u32, relationship: u32, text_at: u64) -> Vec<u8> {
        let mut b = vec![0_u8; RECORD_BYTES];
        b[0..4].copy_from_slice(&account_id.to_le_bytes());
        b[4..8].copy_from_slice(&STEAM_HIGH.to_le_bytes());
        b[8..12].copy_from_slice(&relationship.to_le_bytes());
        b[0x0c..0x10].copy_from_slice(&1_u32.to_le_bytes());
        b[0x10..0x18].copy_from_slice(&0x1f3_0808_0000_u64.to_le_bytes());
        b[0x18..0x1c].copy_from_slice(&(-1_i32).to_le_bytes());
        b[0x20..0x28].copy_from_slice(&text_at.to_le_bytes());
        b[0x28..0x2c].copy_from_slice(&1_422_450_u32.to_le_bytes());
        b
    }

    #[test]
    fn records_are_found_at_eight_byte_boundaries_only() {
        let mut piece = vec![0_u8; 0x200];
        let rec = record(104_859_703, 3, 0x55d_4673_e9a0);
        piece[0x48..0x48 + RECORD_BYTES].copy_from_slice(&rec);
        // The same bytes off the boundary, and a non-friend's record.
        piece[0xa4..0xa4 + RECORD_BYTES].copy_from_slice(&rec);
        piece[0x100..0x100 + RECORD_BYTES].copy_from_slice(&record(1, 2, 0x55d_4673_e9a0));
        let found = find_records(&piece, 0x1000);
        assert_eq!(
            found,
            vec![(
                0x1048,
                Record {
                    account_id: 104_859_703,
                    text_at: 0x55d_4673_e9a0
                }
            )]
        );
        // A text pointer out of user space is no record. A null one (an
        // offline friend with no line yet) is a record with no text.
        assert_eq!(
            record_at(&record(5, 3, 0)),
            Some(Record {
                account_id: 5,
                text_at: 0
            })
        );
        assert_eq!(record_at(&record(5, 3, 0xFFFF_8000_0000_0000)), None);
        assert_eq!(
            text_of(b"In a Party Hideout\0junk"),
            Some("In a Party Hideout")
        );
        assert_eq!(text_of(b"no end"), None);
    }

    fn seen(account_id: u32, text_at: usize) -> Option<(Record, String)> {
        seen_text(account_id, text_at, "In a Party Hideout")
    }

    fn seen_text(account_id: u32, text_at: usize, text: &str) -> Option<(Record, String)> {
        Some((
            Record {
                account_id,
                text_at,
            },
            text.to_owned(),
        ))
    }

    #[test]
    fn only_a_record_seen_changing_lately_is_live() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut tr = Tracker::default();
        // First read: nothing proven.
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Deadlock: Wraith (20 min)"),
            t0,
            1000,
        );
        tr.observe(
            0x80,
            seen_text(7, 0xb0, "Deadlock: Wraith (12 min)"),
            t0,
            1000,
        );
        assert!(tr.live(t0).is_empty());
        // Twenty seconds on, unchanged: still nothing.
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Deadlock: Wraith (20 min)"),
            t0 + s(20),
            1020,
        );
        assert!(tr.live(t0 + s(20)).is_empty());
        // The minute ticks on the live record; the freed copy at 0x80 never moves.
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Deadlock: Wraith (21 min)"),
            t0 + s(60),
            1060,
        );
        tr.observe(
            0x80,
            seen_text(7, 0xb0, "Deadlock: Wraith (12 min)"),
            t0 + s(60),
            1060,
        );
        let live = tr.live(t0 + s(60));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].account_id, 7);
        assert_eq!(live[0].presence.minutes, Some(21));
        assert_eq!(live[0].presence.hero.as_deref(), Some("Wraith"));
        assert_eq!(live[0].changed_unix, 1060);
        assert_eq!(tr.counts(t0 + s(60)), (2, 1));
        // No change for longer than LIVE_FOR: the match may be over, so it goes.
        assert_eq!(tr.live(t0 + s(60) + LIVE_FOR).len(), 1);
        assert!(tr.live(t0 + s(61) + LIVE_FOR).is_empty());
    }

    #[test]
    fn a_match_ending_shows_the_hideout_then_nothing() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut tr = Tracker::default();
        tr.observe(0x10, seen_text(7, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        tr.observe(0x10, seen(7, 0xc0), t0 + s(80), 1080);
        let live = tr.live(t0 + s(80));
        assert_eq!(live[0].presence.state, "hideout");
        assert_eq!(live[0].presence.minutes, None);
        assert!(tr.live(t0 + s(81) + LIVE_FOR).is_empty());
    }

    #[test]
    fn a_record_that_stops_reading_as_one_starts_over() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut tr = Tracker::default();
        tr.observe(0x10, seen_text(7, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        assert_eq!(tr.live(t0 + s(60)).len(), 1);
        // The memory now holds another friend's record: unproven again.
        tr.observe(
            0x10,
            seen_text(9, 0xa0, "Ranked: Bebop (33 min)"),
            t0 + s(80),
            1080,
        );
        assert!(tr.live(t0 + s(80)).is_empty());
        // Gone altogether.
        tr.observe(0x10, None, t0 + s(100), 1100);
        assert_eq!(tr.counts(t0 + s(100)), (0, 0));
        // A pass that no longer finds a record drops it.
        tr.observe(0x20, seen(3, 0xd0), t0, 1000);
        tr.observe(0x30, seen(4, 0xe0), t0, 1000);
        tr.keep_only(&[0x30]);
        assert_eq!(tr.spots(), vec![0x30]);
    }

    #[test]
    fn a_friend_with_no_line_yet_shows_once_they_start_playing() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut tr = Tracker::default();
        let none = Record {
            account_id: 7,
            text_at: 0,
        };
        tr.observe(0x10, Some((none, String::new())), t0, 1000);
        tr.observe(
            0x18,
            Some((
                Record {
                    account_id: 8,
                    text_at: 0,
                },
                String::new(),
            )),
            t0,
            1000,
        );
        assert!(tr.live(t0).is_empty());
        tr.observe(0x10, seen_text(7, 0xa0, "Finding Match"), t0 + s(20), 1020);
        let live = tr.live(t0 + s(20));
        assert_eq!(
            live.len(),
            1,
            "two empty lines are not one text two friends share"
        );
        assert_eq!(live[0].presence.state, "finding_match");
    }

    #[test]
    fn a_text_two_friends_point_at_is_nobodys() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut tr = Tracker::default();
        // A freed record for friend 9 still points where friend 7's text now sits.
        tr.observe(0x10, seen_text(7, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(0x90, seen_text(9, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        tr.observe(
            0x90,
            seen_text(9, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        assert!(tr.live(t0 + s(60)).is_empty());
        // Two records of the same friend are one friend.
        let mut tr = Tracker::default();
        tr.observe(0x10, seen_text(7, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(0x90, seen_text(7, 0xa0, "Ranked: Bebop (31 min)"), t0, 1000);
        tr.observe(
            0x10,
            seen_text(7, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        tr.observe(
            0x90,
            seen_text(7, 0xa0, "Ranked: Bebop (32 min)"),
            t0 + s(60),
            1060,
        );
        assert_eq!(tr.live(t0 + s(60)).len(), 1);
    }
}
