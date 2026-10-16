//! The running game's memory, opened read-only.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQueryEx,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
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

/// A handle with query and read rights only. Closed on drop.
pub struct GameProcess {
    handle: HANDLE,
}

// The handle is only a kernel reference; reads through it do not care which
// thread makes them.
unsafe impl Send for GameProcess {}

impl GameProcess {
    pub fn open(pid: u32) -> Option<Self> {
        // SAFETY: plain Win32 call; a null handle is checked below.
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
        (!handle.is_null()).then_some(Self { handle })
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
