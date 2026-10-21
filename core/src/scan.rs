//! Byte searches tuned for game memory (21 Oct 2026).
//!
//! `memchr`'s fast search jumps to the needle's rarest bytes first, and by
//! default it guesses rarity from ordinary text, where a zero byte is rare.
//! In the game's memory a zero is the commonest byte there is, so a needle
//! with zeros in it (the minimap bounds as floats, a Steam id's high half)
//! stopped at nearly every position of every zero-filled page: the rules
//! object's search alone cost about half a second per GB, two thirds of a
//! full pass's searching. The ranks here are the byte frequencies of 480 MB
//! sampled from Deadlock's own files (zero 19%, the commonest; then 0xAA,
//! 0xFF, `$`, `I`), most common ranked 255. The finders find exactly what
//! the default ones do; only where they look first changes.
use memchr::arch::all::packedpair::HeuristicFrequencyRank;
use memchr::memmem::{Finder, FinderBuilder};

#[rustfmt::skip]
const RANK: [u8; 256] = [
    255, 246, 233, 237, 228, 138, 165, 161, 230, 182, 163, 123, 158, 100, 140, 223, // 0x00
    236, 238, 216, 191, 176, 144, 137,  94, 173,  47,  92,  91, 130, 141,  97, 195, // 0x10
    249, 205, 239, 201, 252, 194, 209, 154, 204, 185, 197, 149, 188, 145, 171, 179, // 0x20
    219, 214, 210, 240, 206, 202, 215, 148, 174, 143, 153, 127, 142,  49, 128, 229, // 0x30
    247, 196, 207, 184, 243, 177, 168, 113, 220, 251, 175, 106, 189, 151,  85, 155, // 0x40
    208, 180, 193, 186, 211, 248, 157,  86, 156,  69, 159,  80,  57,  17,  19, 122, // 0x50
    217, 117, 167, 107, 181, 135, 226,  18, 164, 114, 104,  27, 170, 146,  43, 109, // 0x60
    203,  31, 119,  77, 150,  95, 183, 227,  79,  36,  24,   2,  14,   4,  90, 245, // 0x70
    234, 125, 132,  76,  83,  72,  66,   7, 187, 118,  62,  37,  41,  38,   5,  75, // 0x80
    218, 166, 250,  99, 162, 147,  54,  53,  73, 212,  29,  58,   9,  39,   0,  55, // 0x90
    222, 232,  67,  51,  87,  40, 105,  44, 120, 116, 254, 110, 242, 199, 112, 241, // 0xA0
     78,  59,  15,  96,  22,  60, 126,  33,  23,  81,   8, 225,   1,  11,   3, 178, // 0xB0
    244,  50,  12,  34, 131,  16,  42,  52, 115, 133,  74,  61, 221,  70,  30, 108, // 0xC0
    121,  71,  28,  48,  10,  89,  25,  26,  63,  88,  35, 139,  68, 224,  45, 169, // 0xD0
    190,  65,   6,  20,  32,  21,  13,  64, 235,  56,  98, 103,  84,  46, 213, 134, // 0xE0
    192, 152,  82,  93, 101, 124, 111, 102, 160, 136, 129, 172, 200, 198, 231, 253, // 0xF0
];

/// How common a byte is in the game's memory, 255 the commonest.
pub struct GameMemoryRank;

impl HeuristicFrequencyRank for GameMemoryRank {
    fn rank(&self, byte: u8) -> u8 {
        RANK[usize::from(byte)]
    }
}

/// A finder for `needle` that looks first where game memory is sparse.
pub fn finder(needle: &[u8]) -> Finder<'static> {
    FinderBuilder::new().build_forward_with_ranker_owned(GameMemoryRank, needle.to_vec())
}

/// Every place `needle` starts in `buf`, overlapping ones too, oldest first.
/// (`find_iter` skips a match that overlaps the one before; a search that
/// filters its hits by alignment must not lose the aligned one that way.)
pub fn all_at<'a>(finder: &'a Finder<'static>, buf: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let mut from = 0_usize;
    std::iter::from_fn(move || {
        let at = from + finder.find(buf.get(from..)?)?;
        from = at + 1;
        Some(at)
    })
}

/// A finder made once, the first time it is asked for.
macro_rules! static_finder {
    ($name:ident, $needle:expr) => {
        fn $name() -> &'static memchr::memmem::Finder<'static> {
            static F: std::sync::OnceLock<memchr::memmem::Finder<'static>> = std::sync::OnceLock::new();
            F.get_or_init(|| $crate::scan::finder($needle))
        }
    };
}
pub(crate) use static_finder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_the_default_finder_finds() {
        let mut buf = vec![0_u8; 1 << 16];
        let needle = [0x00, 0x00, 0x28, 0xC6, 0x00, 0x00, 0x28, 0xC6];
        for at in [0, 4096, 9_001, 65_528] {
            buf[at..at + 8].copy_from_slice(&needle);
        }
        let ours: Vec<usize> = finder(&needle).find_iter(&buf).collect();
        let theirs: Vec<usize> = memchr::memmem::find_iter(&buf, &needle).collect();
        assert_eq!(ours, theirs);
        assert_eq!(ours, vec![0, 4096, 9_001, 65_528]);
    }

    #[test]
    fn all_at_keeps_overlapping_matches() {
        // 01 00 10 01 overlaps itself by one byte.
        let buf = [0x01, 0x00, 0x10, 0x01, 0x00, 0x10, 0x01, 0xFF];
        let f = finder(&[0x01, 0x00, 0x10, 0x01]);
        assert_eq!(all_at(&f, &buf).collect::<Vec<_>>(), vec![0, 3]);
        assert_eq!(f.find_iter(&buf).collect::<Vec<_>>(), vec![0]);
        assert_eq!(all_at(&f, &[]).count(), 0);
    }
}
