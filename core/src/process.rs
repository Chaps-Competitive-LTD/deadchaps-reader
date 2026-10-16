//! The running game's memory, opened read-only.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::ProcessStatus::{
    K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_BLOCK, PSAPI_WORKING_SET_EX_INFORMATION,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQueryEx,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Protections that allow a read: read-only, read-write, write-copy and their
/// executable forms.
const READABLE: [u32; 6] = [0x02, 0x04, 0x08, 0x20, 0x40, 0x80];
/// The top of the user address space on 64-bit Windows.
const USER_TOP: usize = 0x7FFF_FFFF_FFFF;
/// Larger private regions are texture and mesh data, not history or console text.
const MAX_REGION: usize = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub base: usize,
    pub size: usize,
}

const PAGE: usize = 4096;
/// Pages asked about in one call.
const PAGES_PER_ASK: usize = 1 << 16;

/// Pages as `(address, in RAM)`, in address order, folded into the runs of
/// pages in RAM.
pub fn resident_runs(pages: impl IntoIterator<Item = (usize, bool)>) -> Vec<Region> {
    let mut out: Vec<Region> = Vec::new();
    for (addr, valid) in pages {
        if !valid {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.base + last.size == addr => last.size += PAGE,
            _ => out.push(Region { base: addr, size: PAGE }),
        }
    }
    out
}

/// A handle with query and read rights only. Closed on drop.
pub struct GameProcess {
    handle: HANDLE,
}

// The handle is only a kernel reference; reads through it do not care which
// thread makes them.
unsafe impl Send for GameProcess {}

impl GameProcess {
    pub fn open(pid: u32) -> Option<Self> {
        // Query and read rights only: no right to write, to run code or to
        // touch threads. The full query right lets Windows say which pages
        // are in RAM (`resident`); where it is refused, the limited one still
        // reads.
        for rights in [PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ] {
            // SAFETY: plain Win32 call; a null handle is checked below.
            let handle = unsafe { OpenProcess(rights, 0, pid) };
            if !handle.is_null() {
                return Some(Self { handle });
            }
        }
        None
    }

    /// The parts of `regions` that are in the game's working set, that is in
    /// RAM now, or None when Windows will not say. Reading a page that has
    /// been moved out of RAM fetches it back from the disk: on a PC short of
    /// memory a read of the whole game then took 4 to 5 minutes, at 40 MB/s,
    /// and pushed the game's own memory out in turn. The objects read while a
    /// match is played (the players, the rules object) are used by the game
    /// every frame, so they are in RAM.
    pub fn resident(&self, regions: &[Region]) -> Option<Vec<Region>> {
        let mut pages: Vec<(usize, bool)> = Vec::new();
        let mut ask: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = Vec::with_capacity(PAGES_PER_ASK);
        let mut out = Vec::new();
        for region in regions {
            let count = region.size / PAGE;
            let mut first = 0;
            while first < count {
                let n = (count - first).min(PAGES_PER_ASK);
                ask.clear();
                ask.extend((0..n).map(|i| PSAPI_WORKING_SET_EX_INFORMATION {
                    VirtualAddress: (region.base + (first + i) * PAGE) as *mut _,
                    VirtualAttributes: PSAPI_WORKING_SET_EX_BLOCK { Flags: 0 },
                }));
                let bytes = u32::try_from(n * std::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()).ok()?;
                // SAFETY: the buffer holds `n` entries, each with the address
                // to ask about; the call fills in their attributes.
                if unsafe { K32QueryWorkingSetEx(self.handle, ask.as_mut_ptr().cast(), bytes) } == 0 {
                    return None;
                }
                pages.clear();
                pages.extend(ask.iter().map(|e| {
                    // SAFETY: a plain integer view of the attribute bits.
                    (e.VirtualAddress as usize, unsafe { e.VirtualAttributes.Flags } & 1 == 1)
                }));
                out.extend(resident_runs(pages.iter().copied()));
                first += n;
            }
        }
        Some(out)
    }

    /// Committed, readable, private regions: where the game keeps its heap.
    pub fn private_regions(&self) -> Vec<Region> {
        self.regions(|size| size <= MAX_REGION)
    }

    /// The private regions too big to read (over `MAX_REGION`), for the log
    /// when something is not found.
    pub fn too_big(&self) -> Vec<Region> {
        self.regions(|size| size > MAX_REGION)
    }

    fn regions(&self, keep: impl Fn(usize) -> bool) -> Vec<Region> {
        let mut out = Vec::new();
        let mut addr = 0_usize;
        let mut info = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::zeroed();
        while addr < USER_TOP {
            // SAFETY: the buffer is a MEMORY_BASIC_INFORMATION of the size passed.
            let written = unsafe {
                VirtualQueryEx(
                    self.handle,
                    addr as *const _,
                    info.as_mut_ptr(),
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if written == 0 {
                break;
            }
            // SAFETY: VirtualQueryEx filled it.
            let region = unsafe { info.assume_init_ref() };
            let base = region.BaseAddress as usize;
            let size = region.RegionSize;
            let protect = region.Protect;
            if region.State == MEM_COMMIT
                && region.Type == MEM_PRIVATE
                && protect & PAGE_GUARD == 0
                && protect != PAGE_NOACCESS
                && READABLE.contains(&(protect & 0xFF))
                && keep(size)
            {
                out.push(Region { base, size });
            }
            match base.checked_add(size) {
                Some(next) if next > addr => addr = next,
                _ => break,
            }
        }
        out
    }

    /// Reads up to `buf.len()` bytes at `addr`. Returns how many were read;
    /// 0 when the memory is gone.
    pub fn read(&self, addr: usize, buf: &mut [u8]) -> usize {
        let mut read = 0_usize;
        // SAFETY: the buffer is valid for its length and `read` receives the count.
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const _,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut read,
            )
        };
        if ok == 0 { read.min(buf.len()) } else { read }
    }
}

/// Windows opens the game with the user's own rights, so access is never
/// short.
pub fn access_needed() -> bool {
    false
}

/// Windows has nothing to grant before the game runs.
pub fn granted() -> bool {
    true
}

impl Drop for GameProcess {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod resident_tests {
    use super::*;

    /// One read of `regions` of the running game, counting the rules objects
    /// and the player objects for `account` it finds: (rules, players, MB, ms).
    fn scan(game: &GameProcess, regions: &[Region], account: u32) -> (usize, usize, usize, u128) {
        const CHUNK: usize = 4 << 20;
        const OVERLAP: usize = crate::players::OBJECT_BYTES + 8;
        let started = std::time::Instant::now();
        let mut buf = vec![0_u8; CHUNK];
        let (mut rules, mut players, mut bytes) = (Vec::new(), Vec::new(), 0_usize);
        for region in regions {
            let mut off = 0;
            while off < region.size {
                let want = CHUNK.min(region.size - off);
                let got = game.read(region.base + off, &mut buf[..want]);
                bytes += got;
                rules.extend(crate::rules::find(&buf[..got], region.base + off));
                players.extend(
                    crate::players::find_players(&buf[..got], crate::players::Offsets::CURRENT, account)
                        .into_iter()
                        .map(|(at, _)| region.base + off + at),
                );
                if want <= OVERLAP || off + want >= region.size {
                    break;
                }
                off += want - OVERLAP;
            }
        }
        rules.sort_unstable();
        rules.dedup();
        players.sort_unstable();
        players.dedup();
        (rules.len(), players.len(), bytes >> 20, started.elapsed().as_millis())
    }

    /// The running game read twice, only what is in RAM and all of it, then
    /// again while this test holds PRESSURE_GB of RAM so Windows moves cold
    /// parts of the game out. Run by hand, with the game in a match:
    /// `PROBE_ACCOUNT=<id> PRESSURE_GB=<n> cargo test ... live_ram_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the running game"]
    fn live_ram_probe() {
        let account: u32 = std::env::var("PROBE_ACCOUNT").expect("PROBE_ACCOUNT").parse().unwrap();
        let gb: usize = std::env::var("PRESSURE_GB").unwrap_or_default().parse().unwrap_or(0);
        let pid = crate::game_pid("deadlock.exe").expect("the game runs");
        let game = GameProcess::open(pid).expect("opens");
        let report = |label: &str| {
            let all = game.private_regions();
            let ram = game.resident(&all).expect("Windows answers");
            let total: usize = all.iter().map(|r| r.size).sum::<usize>() >> 20;
            let in_ram: usize = ram.iter().map(|r| r.size).sum::<usize>() >> 20;
            let r = scan(&game, &ram, account);
            println!("{label}: in RAM {in_ram} of {total} MB; RAM-only read: {} rules, {} player objects, {} MB in {} ms", r.0, r.1, r.2, r.3);
            r
        };
        let calm = report("calm");
        let full = scan(&game, &game.private_regions(), account);
        println!("calm full read: {} rules, {} player objects, {} MB in {} ms", full.0, full.1, full.2, full.3);
        if gb > 0 {
            let hog: Vec<Vec<u8>> = (0..gb).map(|_| vec![1_u8; 1 << 30]).collect();
            std::thread::sleep(std::time::Duration::from_secs(5));
            report("squeezed");
            let full = scan(&game, &game.private_regions(), account);
            println!("squeezed full read: {} rules, {} player objects, {} MB in {} ms", full.0, full.1, full.2, full.3);
            drop(hog);
        }
        assert!(calm.0 >= 1 && calm.0 == full.0, "the rules object from RAM alone");
    }

    #[test]
    fn pages_in_ram_fold_into_runs() {
        let pages = [(0x1000, true), (0x2000, true), (0x3000, false), (0x4000, true), (0x9000, true)];
        assert_eq!(
            resident_runs(pages),
            vec![
                Region { base: 0x1000, size: 0x2000 },
                Region { base: 0x4000, size: 0x1000 },
                Region { base: 0x9000, size: 0x1000 },
            ]
        );
        assert!(resident_runs([(0x1000, false)]).is_empty());
    }

    /// Asks Windows about this test's own memory through the same handle
    /// rights the reader opens the game with: a buffer just written is in
    /// RAM, all of it.
    #[test]
    fn a_buffer_just_written_reads_as_in_ram() {
        let buf = vec![7_u8; 32 << 20];
        let at = buf.as_ptr() as usize;
        // SAFETY: plain Win32 call.
        let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
        let me = GameProcess::open(pid).expect("opens itself");
        let region = me
            .private_regions()
            .into_iter()
            .find(|r| r.base <= at && at < r.base + r.size)
            .expect("the buffer's region");
        let resident = me.resident(&[region]).expect("Windows answers with these rights");
        let start = (at + PAGE - 1) & !(PAGE - 1);
        let end = (at + buf.len()) & !(PAGE - 1);
        let covered: usize = resident
            .iter()
            .map(|r| r.base.max(start)..(r.base + r.size).min(end))
            .filter(|span| span.start < span.end)
            .map(|span| span.end - span.start)
            .sum();
        assert_eq!(covered, end - start, "every page of the written buffer is in RAM");
        assert_eq!(buf[buf.len() - 1], 7);
    }
}
