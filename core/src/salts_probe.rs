//! A match's replay and metadata salts, read from the game's memory once the
//! client has asked Valve for the match.
//!
//! Valve's reply (`CMsgClientToGCGetMatchMetaDataResponse`) stays in the
//! game's memory as a protobuf object. The download address is built in
//! Steam's process, not the game's, so the game's memory normally does not
//! hold it and the salts are taken from the reply instead. The reply names
//! no match, but two of its times are exact functions of the match,
//! `start + 21 days` and `end + 20 minutes`, so a history record with the
//! same start time identifies it ([`match_of`]), and failing that, a match
//! that has just ended does once Valve confirms the file exists
//! ([`key_by_endings`]).
//!
//! With the salts, a match can be submitted as soon as it ends and fetched
//! once Valve publishes it. Watching Steam's cache files alone misses every
//! match whose files Valve has not yet published.

use super::HistoryEntry;

const HOST_TAIL: &[u8] = b".valve.net/1422450/";
const HEAD: &[u8] = b"http://replay";
/// The most bytes between "http://replay" and ".valve.net": a cluster id.
const CLUSTER_MAX: usize = 8;
const NAME_MAX: usize = 48;

/// Every replay address in `buf`, whole and once each, in the order found.
pub fn urls(buf: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for at in memchr::memmem::find_iter(buf, HOST_TAIL) {
        // Back to "http://replay", at most a cluster id away.
        let Some(start) = (at.saturating_sub(HEAD.len() + CLUSTER_MAX)
            ..=at.saturating_sub(HEAD.len()))
            .rev()
            .find(|&s| buf[s..].starts_with(HEAD))
        else {
            continue;
        };
        if !buf[start + HEAD.len()..at].iter().all(u8::is_ascii_digit) || start + HEAD.len() == at {
            continue;
        }
        // Forward to the end of the file name.
        let name_start = at + HOST_TAIL.len();
        let name_end = buf[name_start..]
            .iter()
            .take(NAME_MAX)
            .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'.'))
            .map_or(buf.len().min(name_start + NAME_MAX), |i| name_start + i);
        let Ok(url) = std::str::from_utf8(&buf[start..name_end]) else {
            continue;
        };
        if !(url.ends_with(".meta.bz2") || url.ends_with(".dem.bz2")) {
            continue;
        }
        if !out.iter().any(|u| u == url) {
            out.push(url.to_owned());
        }
    }
    out
}

/// The replay stays downloadable for three weeks from the match's start.
const VALID_FOR: u32 = 21 * 24 * 3600;
/// The metadata is dated twenty minutes after the match's end.
const AFTER_END: u32 = 20 * 60;
/// Every field of the reply set.
const ALL_FIELDS: u32 = 63;
const RESULT_OK: u32 = 1;
/// Unix times the game could plausibly hold (2020 to 2038).
const TIMES: std::ops::RangeInclusive<u32> = 1_577_836_800..=2_145_916_800;

/// Valve's reply as the client keeps it: twelve 32-bit words, a pointer
/// into a loaded module, eight zero bytes, has-bits, a zero cached size,
/// then result, replay salt, metadata salt, valid-through, cluster and the
/// metadata's date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub at: usize,
    pub replay_salt: u32,
    pub metadata_salt: u32,
    pub cluster: u32,
    /// The match's start, as the game's history records it.
    pub start_time: u32,
    /// The match's end.
    pub end_time: u32,
}

/// Every successful reply in `buf`, `base` being the address of `buf`.
/// Aligned to sixteen bytes, as the heap places the object.
pub fn replies(buf: &[u8], base: usize) -> Vec<Reply> {
    let mut out = Vec::new();
    let word = |i: usize| {
        let b = &buf[i..i + 4];
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    };
    let mut i = (16 - base % 16) % 16;
    while i + 48 <= buf.len() {
        if (0x7ff0..=0x7fff).contains(&word(i + 4))
            && word(i + 8) == 0
            && word(i + 12) == 0
            && word(i + 16) == ALL_FIELDS
            && word(i + 20) == 0
            && word(i + 24) == RESULT_OK
            && word(i + 32) != 0
            && (1..=4000).contains(&word(i + 40))
            && TIMES.contains(&word(i + 36))
            && TIMES.contains(&word(i + 44))
            && word(i + 36) > word(i + 44)
        {
            out.push(Reply {
                at: base + i,
                replay_salt: word(i + 28),
                metadata_salt: word(i + 32),
                cluster: word(i + 40),
                start_time: word(i + 36) - VALID_FOR,
                end_time: word(i + 44) - AFTER_END,
            });
        }
        i += 16;
    }
    out
}

/// The match a reply is for: the history record that started when the
/// reply says, and, when the record knows its length, ended within a few
/// seconds of when the reply says.
pub fn match_of(reply: &Reply, known: &[HistoryEntry]) -> Option<u64> {
    known
        .iter()
        .find(|e| {
            e.start_time == reply.start_time
                && (e.duration_s == 0
                    || (e.start_time + e.duration_s).abs_diff(reply.end_time) <= 5)
        })
        .map(|e| e.match_id)
}

/// The demo's address, as the client would build it.
pub fn dem_url(match_id: u64, cluster: u32, replay_salt: u32) -> String {
    format!("http://replay{cluster}.valve.net/1422450/{match_id}_{replay_salt}.dem.bz2")
}

/// The game's lobby object (`CSOCitadelLobby`: lobby id, match id, match
/// mode, game mode, ...), found by the lobby and match ids from the game's
/// console output. The
/// modes are the two 32-bit words after the match id when they read as the
/// game's enums (match mode 1 to 8, game mode 1 to 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbyHit {
    pub at: usize,
    pub match_mode: Option<u32>,
    pub game_mode: Option<u32>,
    /// Sixteen words from the lobby id, for the log.
    pub words: Vec<u32>,
}

pub fn find_lobby(buf: &[u8], base: usize, lobby_id: u64, match_id: u64) -> Vec<LobbyHit> {
    let needle = lobby_id.to_le_bytes();
    let word = |i: usize| {
        buf.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    memchr::memmem::find_iter(buf, &needle)
        .filter(|at| (base + at).is_multiple_of(8))
        .take(3)
        .map(|at| {
            let mut match_mode = None;
            let mut game_mode = None;
            for off in [8_usize, 16, 24] {
                let here = at + off;
                if buf
                    .get(here..here + 8)
                    .is_some_and(|b| b == match_id.to_le_bytes())
                {
                    let mm = word(here + 8);
                    let gm = word(here + 12);
                    if mm.is_some_and(|m| (1..=8).contains(&m))
                        && gm.is_some_and(|g| (1..=6).contains(&g))
                    {
                        match_mode = mm;
                        game_mode = gm;
                    }
                    break;
                }
            }
            let words = (0..16).filter_map(|i| word(at + i * 4)).collect();
            LobbyHit {
                at: base + at,
                match_mode,
                game_mode,
                words,
            }
        })
        .collect()
}

/// The most replies checked against Valve in one pass, so a memory full of
/// old replies cannot turn into a run of requests.
pub const KEY_BY_ENDING_AT_MOST: usize = 4;

/// Keys replies the game's history could not key to a match that has just
/// ended, and only when Valve confirms the pairing.
///
/// [`match_of`] needs a history record with the reply's start time. The game
/// may not have written that record yet, or may have written it without the
/// length, and the reply would then go unused although it holds the right
/// salts.
///
/// The game's memory also holds replies for other matches (opening a match
/// in the game leaves its reply behind), so a guess is not enough: `has` asks
/// Valve whether `<ending>_<metadata salt>.meta.bz2` exists on the reply's
/// cluster, which no foreign reply can pass.
///
/// Every match that ended recently is offered (`endings`), newest first, not
/// only the one the caller is waiting on: the caller learns of a new match
/// from a console line that can leave memory before it is read, so its idea
/// of the match that just ended can be one match behind. A reply is only put
/// to Valve for a match whose end agrees with the reply's own `end_time`,
/// which keeps the asking to about one request per match.
///
/// `tried` holds every pair asked about, once per asking. A pair is asked
/// about at most [`ASKS_PER_PAIR`] times, since Valve may not have the file
/// in the first seconds after the end. Memory often holds two copies of one
/// reply, so a pair is asked about at most once per call however many copies
/// there are. Otherwise the copies use up the asks before Valve's file
/// appears, which takes 70 to 85 s after the end.
pub fn key_by_endings(
    replies: &[Reply],
    endings: &[Ending],
    tried: &mut Vec<(u64, u32)>,
    has: &dyn Fn(&str) -> bool,
) -> Vec<(u64, Reply)> {
    let mut keyed: Vec<(u64, Reply)> = Vec::new();
    let mut asked = 0;
    let mut asked_now: Vec<(u64, u32)> = Vec::new();
    for ending in endings {
        for reply in replies {
            if asked >= KEY_BY_ENDING_AT_MOST {
                return keyed;
            }
            let pair = (ending.match_id, reply.metadata_salt);
            if reply.metadata_salt == 0
                || !ending.agrees_with(reply)
                || keyed.iter().any(|(id, r)| *id == ending.match_id || r.at == reply.at)
                || asked_now.contains(&pair)
                || tried.iter().filter(|p| **p == pair).count() >= ASKS_PER_PAIR
            {
                continue;
            }
            asked += 1;
            asked_now.push(pair);
            tried.push(pair);
            if has(&url(ending.match_id, reply.cluster, reply.metadata_salt)) {
                keyed.push((ending.match_id, *reply));
            }
        }
    }
    keyed
}

/// How many times one (match, reply) pair is put to Valve: once a pass, and
/// passes are 30 to 45 s apart while a reply is awaited, so this covers about
/// five minutes, the time the worker waits for a reply (`REPLY_FOR`).
pub const ASKS_PER_PAIR: usize = 8;
/// A reply belongs to a match seen ending at `ended_unix` when the reply's
/// own end falls in this window before that (players sit on the end screen
/// before the lobby empties) or just after (clocks differ). Matches last
/// twenty minutes or more, so the window cannot reach the match before.
const END_BEFORE_AT_MOST: u64 = 900;
const END_AFTER_AT_MOST: u64 = 120;

/// A match the reader saw end, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ending {
    pub match_id: u64,
    pub ended_unix: u64,
}

impl Ending {
    /// A reply with no end of its own is not ruled out: Valve decides.
    fn agrees_with(&self, reply: &Reply) -> bool {
        let end = u64::from(reply.end_time);
        end == 0 || (end + END_BEFORE_AT_MOST >= self.ended_unix && end <= self.ended_unix + END_AFTER_AT_MOST)
    }
}

/// The matches to offer, at most three, each once: `first` (the one being
/// waited on) ahead of the rest, the rest newest first, and those in
/// `replied` left out.
pub fn endings_to_offer(first: Option<Ending>, rest: &[Ending], replied: &[u64]) -> Vec<Ending> {
    let mut out: Vec<Ending> = Vec::new();
    let mut all: Vec<Ending> = rest.to_vec();
    all.sort_by_key(|e| std::cmp::Reverse(e.match_id));
    for ending in first.into_iter().chain(all) {
        if !replied.contains(&ending.match_id) && !out.iter().any(|e| e.match_id == ending.match_id) {
            out.push(ending);
        }
    }
    out.truncate(3);
    out
}

/// The metadata's address, as the client would build it.
pub fn url(match_id: u64, cluster: u32, metadata_salt: u32) -> String {
    format!("http://replay{cluster}.valve.net/1422450/{match_id}_{metadata_salt}.meta.bz2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_addresses_are_lifted_whole_and_once() {
        let mut buf = vec![0_u8; 64];
        buf.extend_from_slice(
            b"http://replay404.valve.net/1422450/37959196_937530290.meta.bz2\0junk",
        );
        buf.extend_from_slice(
            b"\0\0http://replay183.valve.net/1422450/42476710_428480166.dem.bz2?x=1",
        );
        buf.extend_from_slice(b"\0http://replay404.valve.net/1422450/37959196_937530290.meta.bz2");
        buf.extend_from_slice(b"\0http://replayX.valve.net/1422450/1_2.meta.bz2");
        buf.extend_from_slice(b"\0http://replay1.valve.net/1422450/1_2.txt");
        assert_eq!(
            urls(&buf),
            vec![
                "http://replay404.valve.net/1422450/37959196_937530290.meta.bz2".to_owned(),
                "http://replay183.valve.net/1422450/42476710_428480166.dem.bz2".to_owned(),
            ]
        );
        assert!(urls(b"nothing here").is_empty());
    }

    fn entry(match_id: u64, start_time: u32, duration_s: u32) -> HistoryEntry {
        HistoryEntry {
            match_id,
            hero_id: 7,
            duration_s,
            start_time,
            winning_team: 0,
            player_team: 0,
            kills: 0,
            deaths: 0,
            assists: 0,
            last_hits: 0,
            denies: 0,
            hero_level: 0,
            net_worth: 0,
            match_mode: 0,
            game_mode: 0,
            outcome: 0,
        }
    }

    /// A reply read from the live game for match 102948911, which started at
    /// 1788222625 and ran 649 s.
    #[test]
    fn valves_reply_is_keyed_to_the_match_by_its_start() {
        let seen: [u32; 12] = [
            603_464_808,
            32763,
            0,
            0,
            63,
            0,
            1,
            509_659_246,
            818_291_387,
            1_790_037_025,
            273,
            1_788_224_474,
        ];
        let mut buf = vec![0_u8; 32];
        for w in seen {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        buf.extend_from_slice(&[0; 16]);
        let found = replies(&buf, 0x10);
        assert_eq!(found.len(), 1);
        let reply = found[0];
        assert_eq!(
            (
                reply.at,
                reply.metadata_salt,
                reply.replay_salt,
                reply.cluster
            ),
            (0x30, 818_291_387, 509_659_246, 273)
        );
        assert_eq!(
            (reply.start_time, reply.end_time),
            (1_788_222_625, 1_788_223_274)
        );
        let known = [
            entry(1, 1_788_222_625 - 3600, 600),
            entry(102_948_911, 1_788_222_625, 649),
            entry(3, 1_788_222_625 + 3600, 0),
        ];
        assert_eq!(match_of(&reply, &known), Some(102_948_911));
        assert_eq!(
            match_of(&reply, &[entry(9, 1_788_222_625, 1200)]),
            None,
            "a record of another length is another match"
        );
        assert_eq!(
            match_of(&reply, &[entry(9, 1_788_222_625, 0)]),
            Some(9),
            "a record without a length is taken on its start"
        );
        assert_eq!(
            url(102_948_911, 273, 818_291_387),
            "http://replay273.valve.net/1422450/102948911_818291387.meta.bz2"
        );
        assert_eq!(
            dem_url(105_533_699, 380, 1_084_440_399),
            "http://replay380.valve.net/1422450/105533699_1084440399.dem.bz2"
        );
        // A lobby object: lobby id, match id, match mode 4 (Ranked), game mode 1.
        let mut lobby = vec![0_u8; 8];
        lobby.extend_from_slice(&102_073_075_579_351_738_u64.to_le_bytes());
        lobby.extend_from_slice(&105_540_758_u64.to_le_bytes());
        lobby.extend_from_slice(&4_u32.to_le_bytes());
        lobby.extend_from_slice(&1_u32.to_le_bytes());
        lobby.extend_from_slice(&[0; 48]);
        let hits = find_lobby(&lobby, 0x1000, 102_073_075_579_351_738, 105_540_758);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].at, hits[0].match_mode, hits[0].game_mode),
            (0x1008, Some(4), Some(1))
        );
        // The ids alone, no modes that read as the enums: found, modes unknown.
        let mut bare = lobby.clone();
        bare[24..28].copy_from_slice(&999_u32.to_le_bytes());
        let hits = find_lobby(&bare, 0x1000, 102_073_075_579_351_738, 105_540_758);
        assert_eq!((hits.len(), hits[0].match_mode), (1, None));
        assert!(
            find_lobby(&lobby, 0x1004, 102_073_075_579_351_738, 105_540_758).is_empty(),
            "unaligned"
        );
        // A failed reply (result 2, only that field set) is not one.
        let mut failed = buf.clone();
        failed[0x20 + 16..0x20 + 20].copy_from_slice(&1_u32.to_le_bytes());
        failed[0x20 + 24..0x20 + 28].copy_from_slice(&2_u32.to_le_bytes());
        assert!(replies(&failed, 0x10).is_empty());
    }

    /// A reply read from the live game for match 105808982 while the game
    /// had no history record with its start time, so [`match_of`] cannot key
    /// it. Valve did serve the metadata file this reply's salt names.
    fn reply_105808982() -> Vec<u8> {
        let seen: [u32; 12] = [
            872_162_408,
            32763,
            0,
            0,
            63,
            0,
            1,
            1_263_361_078,
            2_047_037_298,
            1_791_299_528,
            271,
            1_789_488_785,
        ];
        let mut buf = vec![0_u8; 32];
        for w in seen {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        buf.extend_from_slice(&[0; 16]);
        buf
    }

    #[test]
    fn the_reply_that_went_unsent_is_read_but_keyed_by_nothing() {
        let buf = reply_105808982();
        let found = replies(&buf, 0x10);
        assert_eq!(found.len(), 1, "the reply is there to be read");
        let reply = found[0];
        assert_eq!(
            (reply.metadata_salt, reply.replay_salt, reply.cluster),
            (2_047_037_298, 1_263_361_078, 271)
        );
        // The start time the game reported for 105808982 during the match.
        assert_eq!(reply.start_time, 1_789_485_128);
        assert_eq!(match_of(&reply, &[]), None, "no record, no key: the drop");
        assert_eq!(
            url(105_808_982, reply.cluster, reply.metadata_salt),
            "http://replay271.valve.net/1422450/105808982_2047037298.meta.bz2"
        );
    }

    #[test]
    fn an_unkeyed_reply_is_keyed_to_the_ending_match_only_when_valve_agrees() {
        let reply = replies(&reply_105808982(), 0x10)[0];
        let at = |match_id: u64| Ending { match_id, ended_unix: u64::from(reply.end_time) + 20 };
        let mut tried = Vec::new();
        let asked = std::cell::RefCell::new(Vec::new());
        let yes = |u: &str| {
            asked.borrow_mut().push(u.to_owned());
            true
        };
        assert_eq!(
            key_by_endings(&[reply], &[at(105_808_982)], &mut tried, &yes),
            vec![(105_808_982, reply)]
        );
        assert_eq!(
            asked.borrow().as_slice(),
            ["http://replay271.valve.net/1422450/105808982_2047037298.meta.bz2".to_owned()]
        );

        // A reply left in memory by another match: Valve says no, nothing is
        // keyed, and the pair is asked about ASKS_PER_PAIR times (the file may
        // be late to Valve's CDN) and then never again.
        let mut tried = Vec::new();
        let no = |_: &str| false;
        for _ in 0..ASKS_PER_PAIR + 2 {
            assert_eq!(key_by_endings(&[reply], &[at(999_999)], &mut tried, &no), vec![]);
        }
        assert_eq!(tried, vec![(999_999, 2_047_037_298); ASKS_PER_PAIR]);

        // The caller waits on 106921001, an older match, while 106926374 has
        // just ended and 106923299's reply is still in memory. Each reply goes
        // to the match whose end it agrees with, and the older match, which
        // ended over an hour earlier, is never put to Valve.
        let mut newest = reply;
        newest.at = 0x2fee4671ee0;
        newest.metadata_salt = 1_918_097_415;
        newest.end_time = 1_789_997_760;
        let mut older = reply;
        older.at = 0x2feca5150e0;
        older.metadata_salt = 1_110_460_982;
        older.end_time = 1_789_995_820;
        let endings = endings_to_offer(
            Some(Ending { match_id: 106_921_001, ended_unix: 1_789_993_310 }),
            &[
                Ending { match_id: 106_923_299, ended_unix: 1_789_995_830 },
                Ending { match_id: 106_926_374, ended_unix: 1_789_997_788 },
            ],
            &[],
        );
        let mut tried = Vec::new();
        let asked = std::cell::RefCell::new(Vec::new());
        let valve = |u: &str| {
            asked.borrow_mut().push(u.to_owned());
            u.contains("106926374_1918097415") || u.contains("106923299_1110460982")
        };
        assert_eq!(
            key_by_endings(&[older, newest], &endings, &mut tried, &valve),
            vec![(106_926_374, newest), (106_923_299, older)]
        );
        assert_eq!(asked.borrow().len(), 2, "one request a match: {:?}", asked.borrow());
        assert!(!asked.borrow().iter().any(|u| u.contains("106921001")));

        // Two copies of one reply are one question to Valve per call, so the
        // asks last as many passes as ASKS_PER_PAIR says.
        let mut twin = reply;
        twin.at += 0x1000;
        let mut tried = Vec::new();
        let counted = std::cell::Cell::new(0);
        let count = |_: &str| {
            counted.set(counted.get() + 1);
            false
        };
        for pass in 1..=ASKS_PER_PAIR {
            assert_eq!(key_by_endings(&[reply, twin], &[at(105_808_982)], &mut tried, &count), vec![]);
            assert_eq!(counted.get(), pass, "one ask a pass");
        }
        assert_eq!(key_by_endings(&[reply, twin], &[at(105_808_982)], &mut tried, &count), vec![]);
        assert_eq!(counted.get(), ASKS_PER_PAIR, "and then no more");

        // Matches already answered are not offered again.
        assert_eq!(
            endings_to_offer(None, &[Ending { match_id: 5, ended_unix: 1 }], &[5]),
            vec![]
        );

        // However many replies memory holds, only so many are asked about.
        let mut many: Vec<Reply> = Vec::new();
        for i in 0..10 {
            let mut r = reply;
            r.metadata_salt = 1000 + i;
            many.push(r);
        }
        let mut tried = Vec::new();
        let counted = std::cell::Cell::new(0);
        let count = |_: &str| {
            counted.set(counted.get() + 1);
            false
        };
        assert_eq!(key_by_endings(&many, &[at(105_808_982)], &mut tried, &count), vec![]);
        assert_eq!(counted.get(), KEY_BY_ENDING_AT_MOST);

        // A reply with no metadata salt is never offered to Valve.
        let mut empty = reply;
        empty.metadata_salt = 0;
        let mut tried = Vec::new();
        let never = |_: &str| panic!("asked about a reply with no metadata salt");
        assert_eq!(key_by_endings(&[empty], &[at(105_808_982)], &mut tried, &never), vec![]);
    }
}
