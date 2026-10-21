//! Recorded matches replayed through the reader's scheduling: the old rule,
//! full passes alone, against the current one, sweeps for players between
//! passes.
//!
//! Each row is one match's seating as the reader's log recorded it, from the
//! sign that a lobby was coming (the hideout party going as the queue popped,
//! or the game being attached) to the first read of ten or more players.
//! `brief` is the time from the last "the lobby emptied" log line, an
//! earlier measure of the same wait (median 46 s, p75 69 s, five over 200 s).
//!
//! The log cannot say when a match's objects became readable, only that a
//! pass starting at one time found nobody and a later one found them. So
//! each match is replayed for every moment in that window, and the old rule
//! must give what the log shows for all of them, which checks that the
//! replay behaves as the reader did. A read finds an object that exists when
//! it starts; a full pass costs what it cost when recorded, and a sweep what
//! that pass's reading cost plus the players' search and its rests.

use super::*;

#[derive(Clone, Copy, PartialEq, Debug)]
enum How {
    /// The hideout party went from memory: the queue popped.
    Popped,
    /// The game was attached and its first pass found the match.
    Attached,
    /// The game was attached while still starting: its first pass was a
    /// small one, or found nobody.
    Started,
    /// A custom lobby read in part for minutes while it filled.
    Custom,
}

struct Seating {
    at: &'static str,
    brief: f64,
    how: How,
    /// When the full pass before the sign ended, for the gap.
    last_pass: Option<f64>,
    found: f64,
    /// Each full pass: start, seconds, seconds of CPU, seconds reading, MB,
    /// players read after it.
    passes: &'static [(f64, f64, f64, f64, u32, usize)],
}

/// 31 recorded seatings, labelled by log time. One recording is left out: a
/// spectated custom, nineteen players read 89 minutes after the last
/// emptying, which is not a seating.
const SEATINGS: &[Seating] = &[
    Seating { at: "01 Oct 00:03:25", brief: 69.0, how: How::Popped, last_pass: Some(-25.0), found: 69.4, passes: &[(5.0, 15.5, 9.47, 2.64, 7466, 8), (50.6, 18.8, 12.02, 3.64, 8971, 12)] },
    Seating { at: "01 Oct 22:51:08", brief: 22.0, how: How::Popped, last_pass: Some(-155.3), found: 21.7, passes: &[(5.0, 16.6, 11.16, 2.35, 8823, 12)] },
    Seating { at: "01 Oct 23:33:33", brief: 25.0, how: How::Popped, last_pass: Some(-396.2), found: 25.1, passes: &[(5.0, 20.1, 12.16, 3.43, 9496, 12)] },
    Seating { at: "02 Oct 14:56:48", brief: 36.0, how: How::Attached, last_pass: None, found: 16.1, passes: &[(0.0, 16.1, 10.06, 2.71, 8249, 12)] },
    Seating { at: "02 Oct 14:58:47", brief: 48.0, how: How::Started, last_pass: None, found: 48.7, passes: &[(0.0, 0.3, 0.27, 0.09, 141, 0), (30.4, 18.4, 12.00, 3.80, 9788, 12)] },
    Seating { at: "02 Oct 15:01:58", brief: 49.0, how: How::Started, last_pass: None, found: 54.5, passes: &[(0.0, 5.6, 3.42, 0.82, 3351, 0), (35.6, 18.9, 12.84, 4.02, 9707, 12)] },
    Seating { at: "02 Oct 15:14:30", brief: 46.0, how: How::Started, last_pass: None, found: 49.9, passes: &[(0.0, 4.0, 2.36, 0.61, 2646, 0), (34.1, 15.9, 9.98, 3.16, 9351, 12)] },
    Seating { at: "02 Oct 15:19:19", brief: 45.0, how: How::Started, last_pass: None, found: 45.4, passes: &[(0.0, 0.2, 0.12, 0.01, 102, 0), (30.2, 15.2, 9.31, 2.96, 9382, 12)] },
    Seating { at: "02 Oct 15:23:00", brief: 206.0, how: How::Attached, last_pass: None, found: 13.3, passes: &[(0.0, 13.3, 7.84, 1.57, 7619, 12)] },
    Seating { at: "02 Oct 15:32:41", brief: 47.0, how: How::Started, last_pass: None, found: 47.3, passes: &[(0.0, 0.0, 0.00, 0.00, 2, 0), (30.0, 17.3, 11.45, 3.64, 9409, 12)] },
    Seating { at: "02 Oct 15:43:20", brief: 46.0, how: How::Started, last_pass: None, found: 45.6, passes: &[(0.0, 0.0, 0.01, 0.00, 2, 0), (30.1, 15.5, 9.56, 3.10, 9500, 12)] },
    Seating { at: "02 Oct 16:39:17", brief: 765.0, how: How::Attached, last_pass: None, found: 9.7, passes: &[(0.0, 9.7, 5.84, 1.23, 6328, 12)] },
    Seating { at: "02 Oct 16:40:41", brief: 33.0, how: How::Attached, last_pass: None, found: 12.9, passes: &[(0.0, 12.9, 7.88, 2.02, 8128, 12)] },
    Seating { at: "02 Oct 16:42:11", brief: 36.0, how: How::Attached, last_pass: None, found: 15.5, passes: &[(0.0, 15.5, 9.47, 2.28, 8379, 12)] },
    Seating { at: "02 Oct 16:53:01", brief: 523.0, how: How::Attached, last_pass: None, found: 13.5, passes: &[(0.0, 13.5, 7.70, 1.76, 9197, 12)] },
    Seating { at: "02 Oct 16:56:04", brief: 46.0, how: How::Started, last_pass: None, found: 51.9, passes: &[(0.0, 5.8, 3.47, 0.80, 3462, 0), (35.8, 16.1, 10.16, 3.22, 9763, 12)] },
    Seating { at: "02 Oct 16:58:28", brief: 46.0, how: How::Started, last_pass: None, found: 45.8, passes: &[(0.0, 0.1, 0.08, 0.01, 75, 0), (30.2, 15.6, 9.56, 3.02, 9734, 12)] },
    Seating { at: "02 Oct 18:33:09", brief: 23.0, how: How::Popped, last_pass: Some(-85.2), found: 23.3, passes: &[(5.0, 18.3, 11.03, 2.20, 10435, 12)] },
    Seating { at: "02 Oct 20:15:39", brief: 27.0, how: How::Popped, last_pass: Some(-20.0), found: 27.4, passes: &[(10.0, 17.4, 10.48, 2.27, 9818, 11)] },
    Seating { at: "02 Oct 21:32:17", brief: 138.0, how: How::Started, last_pass: None, found: 103.5, passes: &[(0.0, 5.6, 3.59, 0.95, 2912, 0), (35.7, 19.4, 12.28, 3.64, 9640, 5), (85.2, 18.3, 11.42, 2.44, 9796, 14)] },
    Seating { at: "02 Oct 22:15:42", brief: 323.0, how: How::Custom, last_pass: Some(-231.7), found: 323.0, passes: &[(30.1, 17.3, 10.28, 2.32, 9395, 5), (77.4, 15.3, 9.27, 1.96, 9411, 0), (122.8, 14.9, 9.00, 1.90, 9385, 0), (167.7, 14.9, 8.89, 1.96, 9352, 0), (212.6, 14.7, 8.81, 1.87, 9352, 0), (257.4, 17.6, 10.67, 2.52, 9335, 0), (305.0, 18.0, 11.38, 3.06, 10035, 14)] },
    Seating { at: "03 Oct 23:03:19", brief: 25.0, how: How::Popped, last_pass: Some(-135.2), found: 24.5, passes: &[(5.0, 19.5, 12.33, 3.21, 9549, 12)] },
    Seating { at: "05 Oct 16:34:14", brief: 436.0, how: How::Attached, last_pass: None, found: 6.7, passes: &[(0.0, 6.7, 4.19, 1.04, 3886, 10)] },
    Seating { at: "06 Oct 14:47:41", brief: 24.0, how: How::Popped, last_pass: Some(-355.6), found: 23.9, passes: &[(5.0, 18.9, 11.92, 2.46, 9875, 12)] },
    Seating { at: "06 Oct 15:47:02", brief: 19.0, how: How::Popped, last_pass: Some(-407.8), found: 19.5, passes: &[(5.0, 14.5, 8.84, 1.79, 8710, 12)] },
    Seating { at: "06 Oct 16:47:01", brief: 21.0, how: How::Popped, last_pass: Some(-345.6), found: 20.8, passes: &[(5.0, 15.8, 9.70, 2.13, 9514, 12)] },
    Seating { at: "06 Oct 17:25:16", brief: 21.0, how: How::Popped, last_pass: Some(-100.2), found: 21.2, passes: &[(5.0, 16.2, 10.19, 2.19, 9615, 12)] },
    Seating { at: "06 Oct 19:49:28", brief: 21.0, how: How::Popped, last_pass: Some(-25.1), found: 21.0, passes: &[(5.0, 16.0, 9.89, 2.07, 8912, 12)] },
    Seating { at: "06 Oct 20:27:39", brief: 20.0, how: How::Popped, last_pass: Some(-360.7), found: 20.1, passes: &[(5.0, 15.1, 9.30, 1.94, 9272, 12)] },
    Seating { at: "06 Oct 21:56:00", brief: 22.0, how: How::Popped, last_pass: Some(-546.0), found: 21.8, passes: &[(5.0, 16.8, 10.36, 2.10, 9961, 12)] },
    Seating { at: "06 Oct 22:49:02", brief: 68.0, how: How::Popped, last_pass: Some(-255.5), found: 68.2, passes: &[(5.0, 15.9, 10.28, 2.23, 9118, 0), (51.0, 17.2, 10.97, 2.96, 9363, 12)] },
];

/// The players' search per gigabyte, measured with the other finders on a
/// heap shaped like the game's (54 ms of the 845 the whole search took).
const PLAYER_SEARCH_PER_GB: f64 = 0.054;
/// One turn of the loop, `PLAYERS_EVERY`.
fn tick() -> f64 {
    PLAYERS_EVERY.as_secs_f64()
}

/// The old rule: a full pass every `MIN_PASS_GAP` for 240 s after the lobby
/// emptied, while nobody is read.
fn old_seating_pass_wanted(emptied: Option<Duration>, players_now: usize) -> bool {
    players_now == 0 && emptied.is_some_and(|gap| gap < Duration::from_secs(240))
}

/// A sweep's seconds and CPU for this match's game, from its largest pass:
/// that pass's reading, the players' search, and a rest every 64 MB.
fn sweep_cost(s: &Seating) -> (f64, f64) {
    let &(_, _, _, read, mb, _) = s
        .passes
        .iter()
        .max_by_key(|p| p.4)
        .expect("a pass");
    let cpu = read + PLAYER_SEARCH_PER_GB * f64::from(mb) / 1024.0;
    let rests = (u64::from(mb) << 20) / THROTTLE_EVERY as u64;
    (cpu + rests as f64 * SWEEP_PAUSE.as_secs_f64(), cpu)
}

#[derive(Default, Debug)]
struct Outcome {
    found: f64,
    cpu: f64,
    passes: u32,
    sweeps: u32,
}

/// The loop, from the sign, until a read finds ten or more players: some of
/// them readable from `some`, all from `all`. `new` picks the rule.
fn replay(s: &Seating, new: bool, some: f64, all: f64) -> Outcome {
    replay_until(s, new, some, all, 900.0)
}

/// `replay`, giving up at `until` seconds.
fn replay_until(s: &Seating, new: bool, some: f64, all: f64, until: f64) -> Outcome {
    let d = |x: f64| Duration::from_secs_f64(x.max(0.0));
    let attached = matches!(s.how, How::Attached | How::Started);
    let partial = s.passes.iter().map(|p| p.5).find(|&n| n > 0 && n < 10).unwrap_or(0);
    let read = |start: f64| {
        if start >= all {
            12
        } else if start >= some {
            partial
        } else {
            0
        }
    };
    let (sweep_secs, sweep_cpu) = sweep_cost(s);
    let mut out = Outcome::default();
    // An emptying is noticed at the end of a loop; an attach passes at once.
    let mut t = if attached { 0.0 } else { tick() };
    let mut last_full = s.last_pass;
    let mut last_sweep: Option<f64> = None;
    let mut last_took: Option<f64> = None;
    // How long the last full pass took: the new rule rests in proportion.
    let mut pass_took: Option<f64> = None;
    let mut observed = s.passes.iter();
    let mut players = 0_usize;
    let mut loading = false;
    // Spent on missing players: old, in passes; new, in sweeps.
    let mut short_spent = 0_u32;
    while t < until {
        let since_sign = d(t);
        let short = players > 0 && players < 10;
        let tries = if new { short_tries(short_spent) } else { short_spent };
        let seated_short = short && short_spent < SHORT_TRIES * SWEEPS_PER_PASS;
        let sweeping = new && seating_sweeps(Some(since_sign), players, seated_short);
        let settle = attached && settling_pass_wanted(since_sign, players > 0);
        let seat = !new && old_seating_pass_wanted(Some(since_sign), players);
        let waits = new && player_pass_waits(Some(since_sign), sweeping);
        let missing = short_pass_wanted(short, 12 - players.min(12), tries);
        // The old rule asked for a pass for missing players outright.
        let players_pass = (loading || settle || (new && missing)) && !waits;
        let want = seat || (!new && missing) || players_pass;
        let since_read = match (last_full, last_sweep) {
            (Some(a), Some(b)) => Some(d(t - a.max(b))),
            (a, b) => a.or(b).map(|e| d(t - e)),
        };
        if pass_due(last_full.map(|e| d(t - e)), want, false, pass_took.filter(|_| new).map(d), IDLE_PASS_EVERY) {
            let p = observed.next().or_else(|| s.passes.last()).expect("a pass");
            let start = t;
            t += p.1;
            out.cpu += p.2;
            out.passes += 1;
            players = read(start);
            if players >= 10 {
                out.found = t;
                return out;
            }
            last_full = Some(t);
            pass_took = Some(p.1);
            loading = (p.4 as usize) << 20 < STILL_LOADING_BYTES;
            if players > 0 {
                short_spent += if new { SWEEPS_PER_PASS } else { 1 };
            }
        } else if new && !players_pass && sweep_due(Some(since_sign), players, seated_short, since_read, last_took.map(d)) {
            let before = players;
            let start = t;
            t += sweep_secs;
            out.cpu += sweep_cpu;
            out.sweeps += 1;
            players = read(start);
            if players >= 10 {
                out.found = t;
                return out;
            }
            last_sweep = Some(t);
            last_took = Some(sweep_secs);
            if before > 0 {
                short_spent += 1;
            }
        }
        t += tick();
    }
    out.found = f64::INFINITY;
    out
}

/// When some players and when ten or more could first be read: (low, high]
/// from the passes that missed and found them, high taken half a second
/// early, as the loop's own time drifts a little each turn (the pass the log
/// shows at 50.6 s replays at 50.5 s).
fn windows(s: &Seating) -> ((f64, f64), (f64, f64)) {
    let first = |pred: &dyn Fn(usize) -> bool| s.passes.iter().position(|p| pred(p.5));
    let window = |i: usize| {
        let high = (s.passes[i].0 - 0.5).max(0.0);
        let low = s.passes[..i]
            .iter()
            .rev()
            .find(|p| p.5 == 0 || p.5 < s.passes[i].5)
            .map_or(0.0, |p| p.0);
        (low.min(high), high)
    };
    let all = window(first(&|n| n >= 10).expect("a full read"));
    let some = first(&|n| n > 0).map_or(all, window);
    (some, all)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn p75(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() * 3 / 4]
}

#[test]
fn his_matches_are_read_sooner_and_never_minutes_late() {
    const STEPS: u32 = 40;
    let mut brief_old = Vec::new();
    let mut brief_new = Vec::new();
    let mut reader_old = Vec::new();
    let mut reader_new = Vec::new();
    let mut reader_new_worst = Vec::new();
    let (mut cpu_old, mut cpu_new) = (0.0, 0.0);
    // The most time from the objects being readable to the lobby read.
    let (mut lag_old, mut lag_new) = (0.0_f64, 0.0_f64);
    println!(
        "{:<16} {:<9} {:>6} {:>8} {:>22} {:>12}",
        "match", "how", "brief", "old", "new best/mean/worst", "CPU old/new"
    );
    for s in SEATINGS {
        if s.how == How::Custom {
            // Read in part from the first pass on: no sweeps, the passes
            // for missing players run as before.
            println!("{:<16} {:<9} {:>5.0}s {:>7.1}s {:>22} {:>12}", s.at, "Custom", s.brief, s.found, "unchanged", "unchanged");
            brief_old.push(s.brief);
            brief_new.push(s.brief);
            continue;
        }
        let ((some_low, some_high), (low, high)) = windows(s);
        let mut news = Vec::new();
        let mut cpu = 0.0;
        for k in 1..=STEPS {
            let all = low + (high - low) * f64::from(k) / f64::from(STEPS);
            let some = some_high.min(all).max(some_low);
            // The old rule gives what the log shows, wherever in the window
            // the objects came: the replay is the reader's.
            let old = replay(s, false, some, all);
            assert!(
                (old.found - s.found).abs() <= 1.5,
                "{}: the old rule replays to {:.1} s, the log says {:.1} s (objects at {all:.1})",
                s.at,
                old.found,
                s.found
            );
            let new = replay(s, true, some, all);
            assert!(
                new.found <= old.found + 0.05,
                "{}: the new rule reads at {:.1} s, later than the old {:.1} s (objects at {all:.1}): {new:?}",
                s.at,
                new.found,
                old.found
            );
            assert!(
                new.found - all.max(0.0) <= 30.0,
                "{}: {:.1} s from the objects being readable to the lobby read",
                s.at,
                new.found - all
            );
            news.push(new.found);
            cpu += new.cpu;
            lag_old = lag_old.max(old.found - all.max(0.0));
            lag_new = lag_new.max(new.found - all.max(0.0));
        }
        let old_cpu: f64 = s.passes.iter().map(|p| p.2).sum();
        let new_cpu = cpu / f64::from(STEPS);
        cpu_old += old_cpu;
        cpu_new += new_cpu;
        let mean = news.iter().sum::<f64>() / news.len() as f64;
        let best = news.iter().copied().fold(f64::INFINITY, f64::min);
        let worst = news.iter().copied().fold(0.0, f64::max);
        println!(
            "{:<16} {:<9} {:>5.0}s {:>7.1}s {:>6.1} / {:>5.1} / {:>5.1}s {:>5.1} / {:>4.1}s",
            s.at,
            format!("{:?}", s.how),
            s.brief,
            s.found,
            best,
            mean,
            worst,
            old_cpu,
            new_cpu
        );
        brief_old.push(s.brief);
        brief_new.push(s.brief - (s.found - mean));
        reader_old.push(s.found);
        reader_new.push(mean);
        reader_new_worst.push(worst);
    }
    let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
    println!(
        "from the lobby emptying (the brief's measure, {} matches): old median {:.0} s, p75 {:.0} s, max {:.0} s; new median {:.0} s, p75 {:.0} s, max {:.0} s",
        brief_old.len(),
        median(&mut brief_old.clone()),
        p75(&mut brief_old.clone()),
        max(&brief_old),
        median(&mut brief_new.clone()),
        p75(&mut brief_new.clone()),
        max(&brief_new)
    );
    println!(
        "from the sign (the queue popping, the game attached; {} matches, the custom lobby aside): old median {:.1} s, p75 {:.1} s, max {:.1} s; new (mean over the window) median {:.1} s, p75 {:.1} s, max {:.1} s; new at worst max {:.1} s",
        reader_old.len(),
        median(&mut reader_old.clone()),
        p75(&mut reader_old.clone()),
        max(&reader_old),
        median(&mut reader_new.clone()),
        p75(&mut reader_new.clone()),
        max(&reader_new),
        max(&reader_new_worst)
    );
    println!("CPU from the sign to the lobby read, all matches: old {cpu_old:.0} s, new {cpu_new:.0} s");
    println!("most time from the players being readable to the lobby read: old {lag_old:.1} s, new {lag_new:.1} s");
    assert!(median(&mut reader_new.clone()) < median(&mut reader_old.clone()));
    assert!(p75(&mut reader_new.clone()) < p75(&mut reader_old.clone()));
    assert!(lag_new < 30.0 && lag_new < lag_old, "never a minute behind the game");
    assert!(cpu_new < cpu_old, "no more CPU spent getting there");
}

/// A match that comes after a long empty spell (the end screen, a menu)
/// is swept for from the queue popping, where the old rule waited for the
/// idle pass: replayed as the last recorded seating, but with the lobby
/// emptied five minutes before the queue popped.
#[test]
fn a_late_pop_is_read_within_a_sweep_or_two() {
    let s = &SEATINGS[SEATINGS.len() - 1];
    assert_eq!(s.at, "06 Oct 22:49:02");
    // The old rule, its 240 s window long gone: the idle pass, up to ten
    // minutes after the one before.
    let old_window = Duration::from_secs(300);
    assert!(!old_seating_pass_wanted(Some(old_window), 0));
    assert!(!pass_due(Some(Duration::from_secs(300)), false, false, None, IDLE_PASS_EVERY));
    // The new: the queue stopping is the sign, and the replay from it is
    // the same as from the party going.
    let new = replay(s, true, 51.0, 51.0);
    assert!(new.found < 60.0, "{new:?}");
}

/// A sign that no match follows: what the looking costs over the 240 s
/// window. After a pop (the last recorded seating) the sweeps cost less than
/// the passes did, and stay less with the queue's own pass, which is not
/// replayed and waits out the quick sweeps (one pass, about 10 s). A game
/// attached and left at a menu (a recorded launch, with its recorded full
/// passes) pays for the 90 s of quick sweeps on top of the passes that look
/// for the local player, about 20 s of CPU over the four minutes, out of a
/// match; a launch that reaches the hideout stops them at the first sweep
/// that reads the local player.
#[test]
fn a_window_that_finds_nobody_costs_about_what_it_did() {
    let never = f64::INFINITY;
    let popped = &SEATINGS[SEATINGS.len() - 1];
    let started = &SEATINGS[7];
    assert_eq!((popped.how, started.how), (How::Popped, How::Started));
    let mut extra = Vec::new();
    for s in [popped, started] {
        let old = replay_until(s, false, never, never, 240.0);
        let new = replay_until(s, true, never, never, 240.0);
        println!(
            "{} {:?}, nobody for 240 s: old {} passes, {:.0} s CPU; new {} passes and {} sweeps, {:.0} s CPU",
            s.at, s.how, old.passes, old.cpu, new.passes, new.sweeps, new.cpu
        );
        extra.push(new.cpu - old.cpu);
    }
    assert!(extra[0] < 0.0, "after a pop: less than the passes");
    assert!(extra[1] < 25.0, "at a menu: the quick sweeps' cost and no more");
}
