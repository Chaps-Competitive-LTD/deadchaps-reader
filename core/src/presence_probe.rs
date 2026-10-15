//! A look at the game's own Steam rich presence, as it sits in memory: the
//! game sets `steam_display` to a `#Steam_Citadel_RP_*` token ("Finding
//! Match", "Ranked:", "Street Brawl:") with the hero and minutes as
//! parameters, and Steam's client library keeps the current values inside
//! the game process. The reader takes only the queue state from it ("Finding
//! Match" is in memory only while a queue runs). The text itself is not
//! logged, since it carries player names and server addresses.

const KEY: &[u8] = b"steam_display";
/// The game's own token for a queue in progress, as its Steam rich presence
/// carries it. It is in memory while the player queues and gone once the
/// match starts.
pub const FINDING_MATCH: &str = "#Steam_Citadel_RP_FindingMatch";

/// How far from the key the player's own rich presence record carries their
/// SteamID64. Memory holds several `steam_display` keys at once: the player's
/// own record, with their SteamID64 255 bytes after the key, holds the
/// current value, while stale network buffers with no id near them can keep
/// `#Steam_Citadel_RP_FindingMatch` long after the queue has ended.
pub const OURS_WITHIN: usize = 512;
const STEAM64_BASE: u64 = 76_561_197_960_265_728;

/// Whether the key at `at` is the player's own record: their SteamID64 within
/// [`OURS_WITHIN`] bytes of it.
fn ours_near(buf: &[u8], at: usize, account: u32) -> bool {
    let id = (STEAM64_BASE + u64::from(account)).to_le_bytes();
    let lo = at.saturating_sub(OURS_WITHIN);
    let hi = (at + OURS_WITHIN).min(buf.len());
    memchr::memmem::find(&buf[lo..hi], &id).is_some()
}

/// Whether any of the text around the keys says the queue is running.
pub fn queueing(snippets: &[String]) -> bool {
    snippets.iter().any(|s| s.contains(FINDING_MATCH))
}

/// Whether `buf` holds the rich presence at all, so the piece is read again
/// on the quick pass: waiting for the next full pass leaves the queue state
/// minutes behind.
pub fn has_presence(buf: &[u8]) -> bool {
    memchr::memmem::find(buf, KEY).is_some()
}
const BEFORE: usize = 48;
const AFTER: usize = 208;
/// Snippets kept per pass: a loaded localisation file can hold the key many
/// times over.
pub const AT_MOST: usize = 6;

/// The printable text around each `steam_display` in `buf`, control bytes
/// shown as dots, runs of zeros collapsed. With the player's account known,
/// only their own record's: stale send buffers and friends' entries say
/// nothing about their queue.
pub fn snippets(buf: &[u8], account: Option<u32>) -> Vec<String> {
    around(buf, KEY, account)
}

fn around(buf: &[u8], key: &[u8], account: Option<u32>) -> Vec<String> {
    memchr::memmem::find_iter(buf, key)
        .filter(|&at| account.is_none_or(|a| ours_near(buf, at, a)))
        .take(AT_MOST)
        .map(|at| {
            let start = at.saturating_sub(BEFORE);
            let end = (at + AFTER).min(buf.len());
            printable(&buf[start..end])
        })
        .collect()
}

fn printable(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut zeros = 0;
    for &b in bytes {
        if b == 0 {
            zeros += 1;
            if zeros == 1 {
                out.push('|');
            }
            continue;
        }
        zeros = 0;
        out.push(if (0x20..0x7F).contains(&b) {
            b as char
        } else {
            '.'
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_around_the_key_is_shown_once_per_key() {
        let mut buf = vec![0_u8; 300];
        let text = b"steam_display\0#Steam_Citadel_RP_MM_Ranked\0hero\0Wraith\0";
        buf[100..100 + text.len()].copy_from_slice(text);
        let got = snippets(&buf, None);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].contains("steam_display|#Steam_Citadel_RP_MM_Ranked|hero|Wraith|"),
            "{}",
            got[0]
        );
        assert!(snippets(b"nothing here", None).is_empty());
        assert!(queueing(&[
            "x|steam_display|#Steam_Citadel_RP_FindingMatch|y".to_owned()
        ]));
        assert!(!queueing(&[
            "steam_display|#Steam_Citadel_RP_InParty".to_owned()
        ]));
        assert_eq!(printable(b"a\x01b\0\0\0c"), "a.b|c");
    }

    /// The key, a length byte and the value, as the live game holds them;
    /// `id_at` puts a SteamID64 that many bytes after the key.
    fn record(value: &str, id_at: Option<(usize, u32)>) -> Vec<u8> {
        let mut buf = vec![0_u8; 2048];
        let at = 1024;
        let mut rec = b"steam_display\0".to_vec();
        rec.push(u8::try_from(value.len() + 1).unwrap());
        rec.extend_from_slice(value.as_bytes());
        rec.extend_from_slice(&[0, 0xb0, 0x5a, 0x40, 0x2a]);
        buf[at..at + rec.len()].copy_from_slice(&rec);
        if let Some((off, account)) = id_at {
            let id = (STEAM64_BASE + u64::from(account)).to_le_bytes();
            buf[at + off..at + off + 8].copy_from_slice(&id);
        }
        buf
    }

    #[test]
    fn only_our_own_record_says_whether_we_queue() {
        let me = 106_448_300;
        // our own record: the hideout, our SteamID64 255 bytes on
        let mine = record("#Steam_Citadel_Hideout_Cards", Some((255, me)));
        // a stale send buffer, no id near, still saying Finding Match
        let stale = record(FINDING_MATCH, None);
        // a friend's record queueing
        let friend = record(FINDING_MATCH, Some((255, 1_000_000_222)));
        let mut memory = mine.clone();
        memory.extend_from_slice(&stale);
        memory.extend_from_slice(&friend);
        assert!(queueing(&snippets(&memory, None)), "the old reading: any record");
        assert!(!queueing(&snippets(&memory, Some(me))), "our own record says the hideout");
        assert_eq!(snippets(&memory, Some(me)).len(), 1);
        // and when we queue, our own record says so
        let queueing_now = record(FINDING_MATCH, Some((255, me)));
        assert!(queueing(&snippets(&queueing_now, Some(me))));
        // an id further than OURS_WITHIN away is not our record
        let far = record(FINDING_MATCH, Some((OURS_WITHIN + 8, me)));
        assert!(!queueing(&snippets(&far, Some(me))));
    }
}
