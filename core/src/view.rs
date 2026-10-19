//! What the reader reports: its view of the running game, and the matches
//! it saw end. Plain data, sent from the reader program to the Companion as
//! JSON (see the `deadchaps-reader` binary).

use super::friends_presence::FriendPresence;
use super::players::MatchPlayer;
use super::{HistoryEntry, MatchEnd};
use serde::{Deserialize, Serialize};
#[cfg(feature = "process")]
use std::path::Path;

/// A match the game logged as ended, with the local player's numbers as the
/// lobby last showed them. The console line carries the match id the moment
/// a match ends, so the Companion can list the match at once, before the
/// history is refreshed and long before deadlock-api's player history has it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndedMatch {
    pub match_id: u64,
    pub ended_unix: u64,
    pub hero_id: u32,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub net_worth: u32,
    pub level: u32,
    /// Damage dealt to heroes, as the lobby last showed it. None where the
    /// reader never read it, and on rows saved by versions that did not keep
    /// it.
    #[serde(default)]
    pub damage: Option<u32>,
    /// Whether the player's team won, when the lobby said. The end line does
    /// not.
    pub won: Option<bool>,
    /// The Steam account signed in when the match ended, so a match played
    /// on another account on the same PC is not listed under this one. None
    /// on rows saved by versions that did not keep it.
    #[serde(default)]
    pub account_id: Option<u32>,
    /// The match's mode and game mode as the lobby object held them while
    /// it ran (ECitadelMatchMode, ECitadelGameMode), so the row knows its
    /// mode the moment the match ends rather than when deadchaps.gg learns it.
    /// None when the lobby object was never found.
    #[serde(default)]
    pub modes: Option<(u32, u32)>,
}

/// How many ended matches the view carries.
#[cfg(feature = "process")]
pub(crate) const KEEP_ENDED: usize = 10;
/// How long an ended match is kept on disk. Steam's own history never lists
/// a Street Brawl, so the end line the reader saw is the only record of one,
/// and it has to outlive a restart or an update.
#[cfg(feature = "process")]
pub(crate) const KEEP_ENDED_FOR: u64 = 14 * 24 * 3600;

/// The ended matches on disk, newest first, without the stale ones.
#[cfg(feature = "process")]
pub(crate) fn load_ended(path: &Path, now: u64) -> Vec<EndedMatch> {
    let mut ended: Vec<EndedMatch> = crate::store::load(path);
    ended.retain(|m| now.saturating_sub(m.ended_unix) < KEEP_ENDED_FOR);
    ended.sort_by_key(|m| std::cmp::Reverse(m.match_id));
    ended.dedup_by_key(|m| m.match_id);
    ended.truncate(KEEP_ENDED);
    ended
}

/// The game's clock and the mid boss, for the objective timers the Companion
/// shows. `now` is the game time when read, at `read_unix_ms`; the timers
/// count on from there.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Objectives {
    pub game_start: f32,
    pub now: f32,
    /// The match clock when read, the game's own where it reads true (it
    /// stands still through a pause, which the game time does not), and
    /// whether it is standing still now.
    pub clock: f32,
    pub paused: bool,
    pub boss_kills: u32,
    pub next_boss: f32,
    /// The match clock when the rejuvenator was claimed, while its buff is
    /// on; None when there is none or the claim was not seen (see
    /// `rejuv_claim`).
    pub rejuv_claimed: Option<f32>,
    pub read_unix_ms: u64,
}

/// A match the game ended, as its own rules object says. Reported to the
/// site so a custom match it runs can be closed the moment it ends, from a
/// player's reader or an observer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameEnd {
    pub match_id: u64,
    pub winner: u8,
}

/// Everything the reader reports about the running game.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MemoryView {
    /// The player's setting.
    pub enabled: bool,
    /// deadchaps.gg's kill switch, which turns reading off for every install.
    pub allowed: bool,
    /// The game runs and is open for reading.
    pub attached: bool,
    /// Linux: the game runs but its memory could not be opened, for want of
    /// the right the helper is given once (`grant::grant_access`).
    pub access_needed: bool,
    /// The Steam account signed in when the history was read.
    pub steam_id: Option<String>,
    /// Newest first.
    pub entries: Vec<HistoryEntry>,
    pub last_match_end: Option<MatchEnd>,
    /// The match the game is in, from the console's `created` line: the
    /// newest one begun after the newest one destroyed. Named the moment the
    /// match starts, long before deadlock-api's live feed lists it.
    pub match_started: Option<MatchEnd>,
    pub last_pass_unix: Option<u64>,
    pub last_pass_ms: Option<u64>,
    pub last_pass_mb: Option<u64>,
    /// The players of the match the game is running, the local player among
    /// them.
    pub match_players: Vec<MatchPlayer>,
    /// The game's Steam presence says a queue is running (read every twenty
    /// seconds with the hot pieces).
    pub queueing: bool,
    /// When the queue was last seen running: a match loads in the minutes
    /// after it, with nobody seated yet.
    pub queue_seen_unix: Option<u64>,
    /// The match's mode and game mode, as the game's lobby object holds
    /// them (ECitadelMatchMode, ECitadelGameMode), found by the console's
    /// ids. None until found, and after the match.
    pub match_modes: Option<(u32, u32)>,
    /// Every history list the game holds, kept apart. Other players' histories
    /// look the same as the local player's, so the Companion keeps the run
    /// that overlaps the matches deadchaps.gg already lists for this account.
    pub runs: Vec<Vec<HistoryEntry>>,
    /// Matches seen ending lately, newest first, with the local player as the
    /// lobby last showed them.
    pub ended: Vec<EndedMatch>,
    /// What the objective timers need, while a match is played.
    pub objectives: Option<Objectives>,
    /// The last match the game itself was seen to end, and who won: the rules
    /// object's state going from 7 (being played) to 8, with the winner set.
    /// Seated or spectating (see `GameEnd`).
    pub game_end: Option<GameEnd>,
    /// The draft's banned heroes as the rules object holds them, with the
    /// match they belong to, sent with the end report.
    pub match_bans: Option<(u64, Vec<u32>)>,
    /// The players of the match the console last named, as last read while
    /// seated in it, with its id (see `match_leftover`).
    #[serde(skip)]
    pub match_roster: Option<(u64, Vec<MatchPlayer>)>,
    /// A match is being played: the rules object reads "in progress" for a
    /// match the console named and no end line has closed. Kept while the
    /// console line and most of the players are gone from memory, which
    /// happens mid match (see `match_running`).
    pub match_running: bool,
    /// Steam friends' rich presence as the game holds it, only for friends
    /// whose record was seen changing lately (see `friends_presence`).
    pub friends_presence: Vec<FriendPresence>,
}

/// How long after the queue was last seen a match is taken to be loading.
const LOADING_FOR: u64 = 180;
/// The most players a hideout party holds.
pub(crate) const PARTY_MAX: usize = 6;
/// The game's spectator team. A custom match's casters sit on it at slot 0,
/// beside the players seated on teams 2 and 3, and while a match loads the
/// game puts everyone on it, at slot 0 with no hero.
pub(crate) const SPECTATORS: u8 = 1;

impl MemoryView {
    /// The minutes between the queue popping and the console naming the
    /// match: nobody seated yet, the queue seen running lately.
    pub fn loading(&self, now_unix: u64) -> bool {
        self.match_players.is_empty()
            && !self.queueing
            && self
                .queue_seen_unix
                .is_some_and(|at| now_unix.saturating_sub(at) <= LOADING_FOR)
    }

    /// Whether the player sits in the hideout rather than a match. Alone in
    /// the hideout the reader finds one player, the local one, on team 2 slot
    /// 0 at level 1; with a friend in the party, two players, both team 2
    /// slot 0. A match seats its players in slots from 1. So the hideout is
    /// everyone at slot 0, and those players are the party. It is not
    /// everyone on one team: squaring up to the party in the hideout puts a
    /// player on team 3, still at slot 0. Measured over 3,398 reads: 3,101
    /// match reads with every slot above 0, 297 hideout reads with every slot
    /// 0, 85 of those on two teams. The first seconds of a loading match can
    /// look the same, and soon pass. From game build 25611908 the hideout can
    /// seat the player at slot 1 (see `lobby_shaped`).
    pub fn in_hideout(&self) -> bool {
        self.lobby_shaped() && !self.in_sandbox()
    }

    /// The Sandbox cannot be told from the hideout yet. A fast-climbing level
    /// looks like a sign, but the level stays up back in the hideout with a
    /// party, so it is not one. Until a real signal is found this is always
    /// false.
    pub fn in_sandbox(&self) -> bool {
        false
    }

    fn lobby_shaped(&self) -> bool {
        let players = &self.match_players;
        if players.is_empty() || !players.iter().any(|p| p.is_us) {
            return false;
        }
        if players.iter().all(|p| p.slot == 0) {
            return true;
        }
        // From game build 25611908 (the City Never Sleeps update) the hideout
        // can seat the local player at slot 1, so the slot alone no longer
        // tells. The console does: no match begun and not yet ended, and no
        // more players than a party. A match is always named by the console's
        // `created` line, and its end line clears it; both still appear on
        // that build.
        party_shaped(players, self.match_started.is_some(), self.match_running)
    }

    /// The newest match known to be over: the console's end line, or the
    /// game ending it (the rules object going from 7 to 8 with a winner),
    /// which comes first. The hideout can be read in between, with the ended
    /// match's player objects still in memory, and those must not read as a
    /// party.
    fn match_over(&self) -> Option<u64> {
        self.last_match_end
            .map(|e| e.match_id)
            .max(self.game_end.map(|g| g.match_id))
    }

    /// The party in the hideout, the local player first; empty in a match. A
    /// player on the spectator team is watching, not in the party, and an
    /// object the last match left behind is nobody (see `match_leftover`).
    pub fn party(&self) -> Vec<MatchPlayer> {
        if !self.in_hideout() {
            return Vec::new();
        }
        let over = self.match_over();
        let mut party: Vec<MatchPlayer> = self
            .match_players
            .iter()
            .filter(|p| p.is_us || p.team != SPECTATORS)
            .filter(|p| p.is_us || !in_a_match_seat(p))
            .filter(|p| !match_leftover(p, self.match_roster.as_ref(), over))
            .cloned()
            .collect();
        party.sort_by_key(|p| !p.is_us);
        party
    }
}

/// Whether the players read are a party rather than a match, once the local
/// player is known to be among them and off slot 0: no match named by the
/// console, none being played, and no more players than a party.
///
/// "None being played" matters because the console's line leaves memory mid
/// match, and the reader can lose every player object but the local one for
/// minutes. That read alone looks like the hideout, while the rules object
/// still reads the match as in progress.
pub(crate) fn party_shaped(players: &[MatchPlayer], match_named: bool, running: bool) -> bool {
    !match_named && !running && players.len() <= PARTY_MAX
}

/// Whether `p` reads exactly as `q` did, a player of a match as last read
/// while seated in it: the same account, seat, hero, level, souls and K/D/A.
pub(crate) fn reads_as(p: &MatchPlayer, q: &MatchPlayer) -> bool {
    p.account_id == q.account_id
        && p.team == q.team
        && p.slot == q.slot
        && p.hero_id == q.hero_id
        && p.level == q.level
        && p.net_worth == q.net_worth
        && p.kills == q.kills
        && p.deaths == q.deaths
        && p.assists == q.assists
}

/// Whether a player read is an object the last match left behind: that
/// match's end line has been read, and the player reads exactly as they did
/// at the match's last seated read. The game keeps a finished match's player
/// objects in memory for minutes, frozen at the final numbers, and without
/// this check they read as party members: 86 of 211 hideout reads measured
/// listed one, sometimes for over a quarter of an hour. A friend who played
/// the match too would come back as their match self, because
/// `merge_players` keeps the copy further along. A live object, in the
/// hideout or the next match, starts again at level 1 and 0/0/0, so it never
/// reads as a final. `roster` is (match id, players).
///
/// Also a leftover: once the end line is read, an object of the match's own
/// seat (same account, team, slot and hero) above level 1. The game adds its
/// end-of-match numbers after the last seated read, so such an object no
/// longer reads exactly as it did, and left in, it makes the hideout look
/// like a match short of players.
pub(crate) fn match_leftover(p: &MatchPlayer, roster: Option<&(u64, Vec<MatchPlayer>)>, ended: Option<u64>) -> bool {
    let Some((match_id, players)) = roster else {
        return false;
    };
    ended.is_some_and(|e| e >= *match_id)
        && players
            .iter()
            .any(|q| reads_as(p, q) || (p.level > 1 && same_seat(p, q)))
}

/// Whether `p` sits the way a match seats its players, which a hideout party
/// never does: with a lane (the game gives one only in a match, 1, 4 or 6 in
/// a 6v6, and none in the hideout), or past any seat a party uses. A match
/// cancelled after the hero vote prints no end line, so `match_leftover`
/// cannot tell its players' objects, and the game keeps them until the next
/// match: a player's Lobby tab listed four of that lobby, an enemy among
/// them, as their party, through a restart of the app (19 Oct 2026).
pub(crate) fn in_a_match_seat(p: &MatchPlayer) -> bool {
    p.lane.is_some() || usize::from(p.slot) > PARTY_MAX
}

/// The same player in the same seat on the same hero, whatever the numbers.
pub(crate) fn same_seat(p: &MatchPlayer, q: &MatchPlayer) -> bool {
    p.account_id == q.account_id && p.team == q.team && p.slot == q.slot && p.hero_id == q.hero_id
}
