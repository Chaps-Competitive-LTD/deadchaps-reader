//! Times each search a full pass runs, alone, over 1 GB of synthetic memory
//! in the reader's 4 MB pieces: zeros, random bytes, and a heap-like mix
//! (pointers, small numbers, floats, text, the odd Steam id). Run with
//! `cargo run --release --example search_bench`.
use std::hint::black_box;
use std::time::{Duration, Instant};

use deadchaps_reader::players::{self, Offsets};
use deadchaps_reader::{find_history, find_match_lines, friends_presence, presence_probe, rules, salts_probe};

const CHUNK: usize = 4 << 20;
const TOTAL: usize = 1 << 30;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// `ids_one_in`: one word in that many is a Steam id.
fn heap_like(rng: &mut Rng, ids_one_in: u64) -> Vec<u8> {
    let mut v = vec![0_u8; CHUNK];
    for w in v.chunks_exact_mut(8) {
        let r = rng.next();
        let word: u64 = if (r >> 7).is_multiple_of(ids_one_in) { 0x0110_0001_0000_0000 | (r & 0xFFFF_FFFF) } else { match r % 16 {
            0..=3 => 0,
            4..=6 => 0x0000_7FF0_0000_0000 | (r >> 20) & 0x000F_FFFF_FFF8, // pointer
            7..=9 => (r >> 40) & 0xFFFF,                                   // small number
            10 => u64::from(f32::to_bits((r % 10_000) as f32 / 7.0)) | (u64::from(f32::to_bits(1.0)) << 32),
            11 | 12 => u64::from_le_bytes(*b"ability_"), // text
            _ => r,
        } };
        w.copy_from_slice(&word.to_le_bytes());
    }
    v
}

fn main() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let sets: Vec<(&str, Vec<u8>)> = vec![
        ("zeros", vec![0_u8; CHUNK]),
        ("random", (0..CHUNK / 8).flat_map(|_| rng.next().to_le_bytes()).collect()),
        ("heap, Steam ids 1 word in 16", heap_like(&mut rng, 16)),
        ("heap, Steam ids 1 word in 4096", heap_like(&mut rng, 4096)),
    ];
    let now = 1_790_000_000_u32;
    let pieces = TOTAL / CHUNK;
    for (name, piece) in &sets {
        println!("{name}: {} pieces of {} MB", pieces, CHUNK >> 20);
        let row = |label: &str, f: &dyn Fn(&[u8]) -> usize| {
            let t = Instant::now();
            let mut hits = 0;
            for _ in 0..pieces {
                hits += f(black_box(piece));
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            println!("  {label:<22} {ms:>8.1} ms/GB  ({hits} hits)");
            Duration::from_secs_f64(ms / 1000.0)
        };
        // What a full pass runs on each piece now (worker.rs read_pieces).
        let mut total = Duration::ZERO;
        total += row("history", &|b| find_history(b, now).len());
        total += row("match lines", &|b| { let (e, s) = find_match_lines(b); e.len() + s.len() });
        total += row("players", &|b| players::find_players_watching(b, Offsets::CURRENT, 1_161_088_555, &[1, 2, 3], &mut Vec::new()).len());
        total += row("presence check", &|b| usize::from(presence_probe::has_presence(b)));
        total += row("replay urls", &|b| salts_probe::urls(b).len());
        total += row("rules scan", &|b| rules::scan(b, 0x1000_0000).0.len());
        total += row("friend records", &|b| friends_presence::find_records(b, 0x1000_0000).len());
        total += row("valve replies", &|b| salts_probe::replies(b, 0x1000_0000).len());
        total += row("lobby object", &|b| salts_probe::find_lobby(b, 0x1000_0000, 123_456, 61_834_207).len());
        println!("  {:<22} {:>8.1} ms/GB", "ALL", total.as_secs_f64() * 1000.0);
    }
}
