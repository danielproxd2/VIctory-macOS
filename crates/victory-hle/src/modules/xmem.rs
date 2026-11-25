//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub(crate) mod range;

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_MAPPED,
    MEM_PRESERVE_PLACEHOLDER, MEM_PRIVATE, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use self::range::{ADDRESS_32BIT, PAGE, address_range};
use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    virtual_alloc(base, size, kind, flags, protect, 0)
}

/// The host places a reservation at multiples of this without being asked.
const GRANULARITY: usize = 64 << 10;

/// [`XMemVirtualAlloc`], at a multiple of `align` (a power of two, or 0 for
/// none of ours) where the address is our choice.
fn virtual_alloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
    align: usize,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let pages = if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
        2 * MIB
    } else {
        0
    };
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        // The host takes 0 or a power of two from its granularity up.
        Alignment: match pages.max(align) {
            a if a > GRANULARITY => a,
            _ => 0,
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can, and a
    // placeholder cannot be committed: a later commit inside one replaces
    // that part of it (`commit_in_placeholder`).
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // A commit at the title's address changes what a thread that maps
    // physical pages there has just asked the host about, and the fallback
    // below asks and releases itself: both wait their turn (see `PHYSICAL`).
    let mut turn = (!base.is_null() && kind & MEM_COMMIT != 0).then(physical);
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if at.is_null() {
        let error = std::io::Error::last_os_error();
        if kind & MEM_COMMIT != 0 {
            commit_refused(size, "virtual memory", error);
        } else {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
        }
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    if at.is_null() {
        if let Some(physical) = turn.as_mut() {
            return commit_in_placeholder(physical, base as usize, size, protect);
        }
    } else if kind & MEM_RESERVE != 0 {
        // Nothing left to keep others from, and `REGIONS` is taken alone.
        drop(turn);
        remember(at as usize, size, flags, placeholder);
    }
    at
}

// Test only: what this thread runs once between asking the host what a
// range holds and the release that relies on the answer.
#[cfg(test)]
thread_local! {
    static CHECKED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Marks the point between a check of the host's memory (`reserved_only`)
/// and the release that relies on it. Nothing happens here outside the
/// tests, which run another thread at this point to show that it cannot get
/// in between.
fn checked() {
    #[cfg(test)]
    if let Some(hook) = CHECKED.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
}

/// What the host says of the region of `process` that holds `address`;
/// `None` where it has nothing to say (past the last address a process
/// has, or a process we may not ask about).
fn host_region(process: HANDLE, address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: a query, into a structure of the size given.
    let written = unsafe {
        VirtualQueryEx(
            process,
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    (written != 0).then_some(info)
}

/// Whether the host has all of `[start, end)` of our process, in regions
/// that each pass `fits`.
fn host_regions(
    start: usize,
    end: usize,
    fits: impl Fn(&MEMORY_BASIC_INFORMATION) -> bool,
) -> bool {
    // SAFETY: a pseudo handle needs no closing.
    let process = unsafe { GetCurrentProcess() };
    let mut at = start;
    while at < end {
        let Some(info) = host_region(process, at).filter(&fits) else {
            return false;
        };
        match (info.BaseAddress as usize).checked_add(info.RegionSize) {
            Some(next) if next > at => at = next,
            _ => return false,
        }
    }
    true
}

/// Whether `[base, base + len)` is reserved memory with nothing committed
/// or mapped in it: all a placeholder can be, and the only memory `map` and
/// `commit_in_placeholder` may put something else in place of.
fn reserved_only(base: usize, len: usize) -> bool {
    base.checked_add(len)
        .is_some_and(|end| host_regions(base, end, |region| region.State == MEM_RESERVE))
}

/// Whether physical pages may be mapped over `[base, base + len)`: all of
/// it is reserved or a mapped view. Committed memory there is the title's
/// data, which a mapping would destroy: what the console does with such a
/// call is not known here, so it is refused before anything is unmapped.
fn takes_views(base: usize, len: usize) -> bool {
    base.checked_add(len).is_some_and(|end| {
        host_regions(base, end, |region| {
            region.State == MEM_RESERVE || region.Type == MEM_MAPPED
        })
    })
}

/// A commit at the title's address that the host refused: if the bytes lie
/// in one of our placeholder reservations, the 64 KiB pages holding them
/// are split off and become committed memory of their own. Null if they do
/// not, or if any of those pages is committed or mapped already.
///
/// The caller holds `PHYSICAL` (it passes the guard's contents): the check
/// of the host's memory and the release below are one step to every other
/// thread that commits or maps here.
fn commit_in_placeholder(
    _turn: &mut Physical,
    base: usize,
    size: usize,
    protect: u32,
) -> *mut c_void {
    const HOST_PAGE: usize = 0x1000;
    let null = std::ptr::null_mut();
    let start = base & !(PAGE - 1);
    let end = base
        .checked_add(size)
        .and_then(|end| end.checked_next_multiple_of(PAGE));
    let Some(end) = end.filter(|&end| end > start) else {
        return null;
    };
    let mut regions = regions();
    let inside = regions
        .placeholders
        .range(..=start)
        .next_back()
        .filter(|(at, (size, _))| at.checked_add(*size).is_some_and(|e| end <= e))
        .map(|(_, (_, flags))| *flags);
    let Some(flags) = inside else {
        return null;
    };
    // The release below does not fail on memory that is no placeholder: of
    // committed memory that took a placeholder's place (what this function
    // makes) it makes a placeholder again, on Windows as documented and
    // under Wine as measured, and the memory's contents are gone. So the
    // host is asked first, and only reserved memory is touched.
    // shortcut: a placeholder the title released with the host's
    // `VirtualFree` keeps its record until the range is reserved here
    // again; a plain reservation the host made there since cannot be told
    // from the placeholder, and a commit in it that the host refuses for a
    // reason of its own still reaches the release.
    if !reserved_only(start, end - start) {
        return null;
    }
    checked();
    // SAFETY: the range was reserved here as a placeholder and the host
    // says nothing is committed or mapped in it (and under `PHYSICAL` no
    // call of ours changes that before the release), so the release
    // destroys no memory: it splits the placeholder around the range (and fails
    // harmlessly when the range is all of it). The replacement acts on a
    // placeholder of exactly this range or fails.
    let at = unsafe {
        VirtualFree(
            start as *mut c_void,
            end - start,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        VirtualAlloc2(
            GetCurrentProcess(),
            start as *const c_void,
            end - start,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            protect,
            std::ptr::null_mut(),
            0,
        )
    };
    if at.is_null() {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            size,
            error = %std::io::Error::last_os_error(),
            "committing part of a 64 KiB-page reservation failed"
        );
        return null;
    }
    // Memory of the reservation, as the title sees it.
    regions.record(start, flags);
    // What the host answers a commit with: the first of its pages.
    (base & !(HOST_PAGE - 1)) as *mut c_void
}

/// The regions `XMemVirtualAlloc` reserved, by base, with the XMem flags
/// the title gave, which `XMemVirtualQuery` gives back.
// The title frees these with the host's `VirtualFree`, which we do not see:
// a record is dropped when its range is handed out here again, and a query
// drops one whose allocation the host no longer has as it was recorded
// (see `flags_of`).
// Lock order: taken alone, or under `PHYSICAL` (see there), never before it.
static REGIONS: Mutex<Regions> = Mutex::new(Regions {
    reserved: BTreeMap::new(),
    placeholders: BTreeMap::new(),
});

struct Regions {
    /// Memory that can be committed where it is, by base.
    reserved: BTreeMap<usize, Region>,
    /// Reservations of 64 KiB pages (host placeholders), by base: the size
    /// the title gave and its flags.
    placeholders: BTreeMap<usize, (usize, u64)>,
}

/// A host allocation as it was when recorded.
#[derive(Clone, Copy)]
struct Region {
    /// Where the host's allocation ends (it rounds sizes up to its pages).
    end: usize,
    /// The protection the host says it was allocated with.
    protect: u32,
    flags: u64,
}

fn regions() -> MutexGuard<'static, Regions> {
    REGIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the allocation of `process` that starts at `base` ends, and the
/// protection it was made with; `None` if no allocation starts there.
fn host_allocation(process: HANDLE, base: usize) -> Option<(usize, u32)> {
    let starts_here = |region: &MEMORY_BASIC_INFORMATION| region.AllocationBase as usize == base;
    let first = host_region(process, base).filter(starts_here)?;
    let mut end = base;
    // One region per run of pages in one state: one or two of a new
    // allocation, which is what this is asked of.
    while let Some(region) = host_region(process, end).filter(starts_here) {
        match (region.BaseAddress as usize).checked_add(region.RegionSize) {
            Some(next) if next > end => end = next,
            _ => break,
        }
    }
    Some((end, first.AllocationProtect))
}

/// Whether the allocation of `process` that `host` is a region of is still
/// the one recorded as `region` at `base`: private memory made with the
/// same protection that ends where it ended. Two more questions to the
/// host, however many regions the allocation has by now.
fn still_allocated(
    process: HANDLE,
    host: &MEMORY_BASIC_INFORMATION,
    base: usize,
    region: &Region,
) -> bool {
    let ours =
        |at: usize| host_region(process, at).is_some_and(|r| r.AllocationBase as usize == base);
    host.Type == MEM_PRIVATE
        && host.AllocationProtect == region.protect
        && region.end > base
        && ours(region.end - 1)
        && !ours(region.end)
}

impl Regions {
    /// Records the host allocation that starts at `at` as memory with
    /// these flags.
    fn record(&mut self, at: usize, flags: u64) {
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        match host_allocation(process, at) {
            Some((end, protect)) => {
                let region = Region {
                    end,
                    protect,
                    flags,
                };
                self.reserved.insert(at, region);
            }
            // Nothing to check a later query against: no record.
            None => {
                self.reserved.remove(&at);
            }
        }
    }
}

/// Records the flags of the region the host just reserved at `at`.
fn remember(at: usize, size: usize, flags: u64, placeholder: bool) {
    let mut regions = regions();
    let Regions {
        reserved,
        placeholders,
    } = &mut *regions;
    // The host gave this range out, so whatever was recorded in it is gone.
    let end = at.saturating_add(size);
    // Neither map's records overlap, so those that reach into the range are
    // the last ones that start below its end.
    let stale: Vec<usize> = reserved
        .range(..end)
        .rev()
        .take_while(|(_, region)| region.end > at)
        .map(|(base, _)| *base)
        .collect();
    for base in stale {
        reserved.remove(&base);
    }
    let stale: Vec<usize> = placeholders
        .range(..end)
        .rev()
        .take_while(|(base, (size, _))| base.saturating_add(*size) > at)
        .map(|(base, _)| *base)
        .collect();
    for base in stale {
        placeholders.remove(&base);
    }
    if placeholder {
        placeholders.insert(at, (size, flags));
    } else {
        regions.record(at, flags);
    }
}

fn forget(at: usize) {
    regions().reserved.remove(&at);
}

/// The flags the region `host` (of `process`) was reserved with, if it is
/// one of ours and still what we reserved: private memory (a view of
/// physical pages mapped into it is not) of an allocation that starts at
/// our base, ends where ours ended and was made with our protection. A
/// record whose allocation is no longer that one (the title released it
/// with the host's `VirtualFree` and the host gave the base out again) is
/// dropped here.
// shortcut: an allocation the host made since at the same base, of the same
// size and protection, cannot be told from ours and reads our flags; the
// host's `VirtualFree` would have to pass through here to end that.
fn flags_of(process: HANDLE, host: &MEMORY_BASIC_INFORMATION) -> Option<u64> {
    let base = host.AllocationBase as usize;
    let mut regions = regions();
    let region = *regions.reserved.get(&base)?;
    if !still_allocated(process, host, base, &region) {
        regions.reserved.remove(&base);
        return None;
    }
    Some(region.flags)
}

/// The XMem flags of the region the host describes as `host`: committed
/// memory has those `recorded` knows for it, or graphics memory's; memory
/// that is free or only reserved has none, whatever is recorded for its
/// address, and `recorded` is not asked.
fn flags_for(
    host: &MEMORY_BASIC_INFORMATION,
    recorded: impl FnOnce(&MEMORY_BASIC_INFORMATION) -> Option<u64>,
) -> u64 {
    if host.State == MEM_COMMIT {
        recorded(host).unwrap_or(GRAPHICS)
    } else {
        0
    }
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

/// Committed memory of a region `XMemVirtualAlloc` reserved reports the
/// flags that call was given.
// shortcut: every other committed page (the host's heaps, mapped physical
// pages, memory of `VirtualAlloc`) reports graphics memory, since all of it
// is GPU-reachable here (Grand Theft Auto VI's BVH builder wants graphics
// memory), and no page has a tag; what the console answers for such memory
// is not known here.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // Windows fails the query for a null structure; Wine writes through it
    // inside its own lock and the process hangs.
    if out.is_null() {
        return 0;
    }
    // SAFETY: the title passes room for the whole structure.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = flags_for(&info.host, |host| flags_of(process, host));
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (Grand Theft Auto VI does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                let at = hook_table(base);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    read_hooks(at)
}

/// Where the image at `base` exports its hooks, or 0.
fn hook_table(base: usize) -> usize {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    // SAFETY: a mapped image and a NUL-terminated name.
    unsafe { GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast()) }
        .map_or(0, |f| f as usize)
}

/// The two routines of the hook table at `at`; none if there is no table.
fn read_hooks(at: usize) -> [usize; 2] {
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    alloc_through(hooks()[0], size, attributes)
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    free_through(hooks()[1], address, attributes)
}

/// Allocates with the title's routine `hook`, or ours if it has none (0):
/// a title may hook one of the two and not the other.
fn alloc_through(hook: usize, size: usize, attributes: u64) -> *mut c_void {
    match hook {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`alloc_through`].
fn free_through(hook: usize, address: *mut c_void, attributes: u64) {
    match hook {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        // A reservation of its own, at the alignment the attributes ask.
        return virtual_alloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
            1 << (attributes >> ALIGNMENT_SHIFT & 0x1f),
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the block lies wherever the host's heap puts it, with no
    // address limit of ours, unlike the console's. Under CrossOver Wine the
    // blocks measured lay below 2^40 (0x147470 for 100 bytes, 0x1612000 for
    // 3 MiB; `default_blocks_come_from_the_host_heap_today` prints them),
    // which nothing promises; where the heap lies on Windows was not
    // measured. Nothing seen builds ray-tracing structures in this memory
    // (Grand Theft Auto VI uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            if VirtualFree(address, 0, MEM_RELEASE) != 0 {
                forget(address as usize);
            }
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

struct Section(HANDLE);

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.0) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each slice (section, page index) is mapped: the addresses of
    /// its page in every view showing it (the GPU side's change tracking
    /// follows writes through one view to the others, `aliases`).
    shown: HashMap<(usize, usize), Vec<usize>>,
}

impl Slice {
    /// The key of page `k` of the slices from this one on in `shown`.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

/// Every address of a title view showing a page that title memory at
/// `[at, at + len)` shows, except the page's own address: pushed to `out`
/// as (page address, other address). The GPU side's change tracking
/// (`victory_gpu::watch`) marks writes there too.
pub fn aliases(at: usize, len: usize, out: &mut Vec<(usize, usize)>) {
    let physical = physical();
    let end = at.saturating_add(len);
    let first = physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| *base + size > at)
        .map(|(base, _)| *base);
    let starts = first.into_iter().chain(
        physical
            .views
            .range(at.saturating_add(1)..end)
            .map(|(b, _)| *b),
    );
    for base in starts.collect::<Vec<_>>() {
        let (size, slice) = &physical.views[&base];
        let (from, to) = (at.max(base), end.min(base + size));
        for page in (from / PAGE..to.div_ceil(PAGE)).map(|p| p * PAGE) {
            let k = (page - base) / PAGE;
            if let Some(addresses) = physical.shown.get(&slice.key(k)) {
                out.extend(addresses.iter().filter(|&&a| a != page).map(|&a| (page, a)));
            }
        }
    }
}

/// The addresses of title views showing `pages` (page numbers): writes into
/// them through our own views must be reported there.
fn shown_at(pages: &[usize]) -> Vec<usize> {
    let physical = physical();
    pages
        .iter()
        .filter_map(|p| physical.pages.get(p))
        .filter_map(|slice| physical.shown.get(&slice.key(0)))
        .flatten()
        .copied()
        .collect()
}

/// Reports writes into physical pages through our own views at every title
/// view showing them (DirectStorage's page destinations).
fn report_written(shown: &[usize]) {
    for &page in shown {
        drop(victory_gpu::kernel_will_write(page, PAGE));
    }
}

/// The page table, and the turn to change what a reservation of ours holds.
///
/// A release that keeps the placeholder (`VirtualFree` with
/// `MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER`) does not fail on committed
/// memory that took a placeholder's place: it destroys it. So the host is
/// asked what a range holds before each such release (`takes_views`,
/// `reserved_only`), and the answer must still be true at the release:
/// every path that asks and releases, and every commit of ours at an address
/// the title names, holds this lock from before the first question until
/// after the release and what replaces the placeholder. Those paths are
/// `XMemMapPhysicalPages`, `map_alias` (with `unmap` and `map` below them),
/// and a fixed-base commit in `virtual_alloc` with `commit_in_placeholder`.
///
/// Lock order: `PHYSICAL`, then `REGIONS`. Nothing takes `PHYSICAL` while
/// it holds `REGIONS`; `commit_in_placeholder` takes `REGIONS` under
/// `PHYSICAL`, and `remember`, `forget` and `flags_of` take `REGIONS` alone.
// shortcut: the title's own calls of the host's `VirtualAlloc` and
// `VirtualFree` do not pass through here and take no turn; a title that
// commits with the host while it maps pages at the same address is not
// ordered by anything here.
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        next_page: FIRST_NUMBER,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    allocate_pages(&PHYSICAL, flags, count, pages, create_section)
}

/// A pagefile-backed section of `size` bytes, its pages committed when
/// first mapped or written (`commit`): Grand Theft Auto VI asks for 6.1 GiB
/// at once, which a busy host often cannot commit (error 1450).
fn create_section(size: usize) -> std::io::Result<Section> {
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Section(handle))
}

/// [`XMemAllocatePhysicalPages`], with the pages entered in `table` and the
/// section made by `create` (the host's, or a test's refusal). A request
/// that fails gives no page: no number is written, and `count` reads 0
/// unless it named no size at all.
fn allocate_pages(
    table: &Mutex<Physical>,
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
    create: impl FnOnce(usize) -> std::io::Result<Section>,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // Room for `n` page numbers cannot lie at `pages` at all.
    if !range::array_fits(pages as usize, n, size_of::<usize>()) {
        return 0;
    }
    let refused = || {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        0
    };
    // The count is the title's. More pages than there are numbers for are
    // refused before the host is asked for anything: under Wine it would
    // make a sparse section of almost any size, and the tables below would
    // then grow by the title's count.
    if n > MAX_PAGES {
        tracing::warn!(
            n,
            most = MAX_PAGES,
            "XMemAllocatePhysicalPages: more pages than there are page numbers"
        );
        return refused();
    }
    let section = match create(size) {
        Ok(section) => Arc::new(section),
        Err(error) => {
            tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
            // Under Wine this is where the memory is asked for (`commit`
            // has nothing left to do there), so this is the failure to
            // explain.
            if on_wine() {
                report(&section_refused_message(
                    size,
                    &error,
                    system_commit().as_ref(),
                ));
            }
            return refused();
        }
    };
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let mut physical = table.lock().unwrap_or_else(|e| e.into_inner());
    // The section is closed again where its pages get no numbers.
    let Some(first) = physical.number(&section, n) else {
        tracing::warn!(
            n,
            next = physical.next_page,
            "XMemAllocatePhysicalPages: out of page numbers, or of memory for the page table"
        );
        return refused();
    };
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: Grand Theft Auto VI keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;
/// The first page number handed out. Page numbers are opaque to titles; 0
/// stays unused.
const FIRST_NUMBER: usize = 0x10_0000;
/// One past the last page number that survives 21 bits of `number >> 4`.
const NUMBER_END: usize = 1 << (21 + FRAMES.trailing_zeros());
/// The most pages there are numbers for, in one request or in all of them.
// shortcut: the numbers of freed pages are not handed out again, so a title
// that allocates and frees more than this over its run is refused from then
// on (124 GiB of pages; Grand Theft Auto VI allocates its 6.1 GiB once).
// Hand freed numbers out again if a title runs out.
const MAX_PAGES: usize = (NUMBER_END - FIRST_NUMBER) / FRAMES;

impl Physical {
    /// Gives the `n` pages of `section` the next page numbers and returns
    /// the first (the others follow, `FRAMES` apart). `None`, with nothing
    /// changed, where fewer than `n` numbers are left or the page table
    /// cannot grow by `n`.
    fn number(&mut self, section: &Arc<Section>, n: usize) -> Option<usize> {
        let first = self.next_page;
        let end = n
            .checked_mul(FRAMES)
            .and_then(|frames| first.checked_add(frames))
            .filter(|&end| end <= NUMBER_END)?;
        self.pages.try_reserve(n).ok()?;
        for index in 0..n {
            let slice = Slice {
                section: section.clone(),
                index,
            };
            self.pages.insert(first + index * FRAMES, slice);
        }
        self.next_page = end;
        Some(first)
    }
}

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    // The count is the title's: one that no array can have is refused before
    // a page number is read or a page forgotten.
    if pages.is_null() || !range::array_fits(pages as usize, n, size_of::<usize>()) {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.0,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len };
        commit(at.Value as usize, len).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let physical = physical();
    range::runs(
        pages,
        offset,
        len,
        |page| physical.pages.get(page),
        |first, n, slice| {
            Arc::ptr_eq(&first.section, &slice.section) && slice.index == first.index + n
        },
    )
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let shown = shown_at(
        pages
            .get(offset / PAGE..(offset + data.len()).div_ceil(PAGE))
            .unwrap_or(&[]),
    );
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    report_written(&shown);
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
    /// Title views showing the pages, told when the window is done.
    shown: Vec<usize>,
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        report_written(&self.shown);
    }
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
        shown: shown_at(
            pages
                .get(offset / PAGE..(offset + len).div_ceil(PAGE))
                .unwrap_or(&[]),
        ),
    })
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    // The count is the title's. One whose bytes wrap would unmap and map a
    // range it does not name: refused before anything changes.
    let Some(len) = range::page_span(base, n) else {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            n,
            "XMemMapPhysicalPages: the range does not fit the address space"
        );
        return std::ptr::null_mut();
    };
    // Held from the question to the last mapping: see `PHYSICAL`.
    let mut physical = physical();
    if !pages.is_null() && !takes_views(base, len) {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            n,
            "XMemMapPhysicalPages: the range holds committed memory or none"
        );
        return std::ptr::null_mut();
    }
    unmap(&mut physical, base, len);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers (fewer bytes than the
    // range checked above).
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (Grand Theft Auto VI passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    let entry = size_of::<usize>();
    if addresses.is_null()
        || !range::array_fits(addresses as usize, n, entry)
        || !(pages.is_null() || range::array_fits(pages as usize, n, entry))
    {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`): Grand Theft Auto
/// VI maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    // All three are the title's (through the GPU's page-mapping batches). A
    // range that runs off the end of the address space names no memory:
    // refused before anything is unmapped.
    let Some((to, len)) = range::byte_span(to, len) else {
        return false;
    };
    let Some((from, _)) = range::byte_span(from, if from == 0 { 0 } else { len as u64 }) else {
        return false;
    };
    // Nor is the title's committed memory replaced (see `takes_views`). The
    // lock is held from the question to the last mapping: see `PHYSICAL`.
    let mut physical = physical();
    if from != 0 && !takes_views(to, len) {
        return false;
    }
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder. Having `physical` is having the turn that
/// makes the check and the release below one step (see `PHYSICAL`).
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // The release below is not confined to placeholders: it makes one of
    // committed memory that took a placeholder's place (see
    // `commit_in_placeholder`), and the view would then replace that
    // memory. Only reserved memory, with nothing committed or mapped in
    // it, is touched.
    if !reserved_only(base, len) {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            "mapping physical pages refused: not reserved memory alone"
        );
        return false;
    }
    checked();
    // SAFETY: the host says the range is reserved and holds no memory (and
    // under `PHYSICAL` no call of ours changes that before the release), so
    // the release destroys none: it splits the placeholder around the range
    // (failing harmlessly when the range is the whole placeholder). The
    // mapping acts on a placeholder of exactly this range or fails.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.0,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    for k in 0..len / PAGE {
        let shown = physical.shown.entry(slice.key(k)).or_default();
        // Pages shown elsewhere already: guarded here too if they are there.
        victory_gpu::title_memory_aliased(base + k * PAGE, shown);
        shown.push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    commit_refused(len, "physical pages", std::io::Error::last_os_error());
    false
}

/// Reports memory the host would not give: at the Windows commit limit to
/// `crashes.log`, since the title usually faults soon after on the null it
/// gets; otherwise as a warning.
fn commit_refused(len: usize, what: &str, error: std::io::Error) {
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        report(&commit_limit_message(
            len,
            what,
            &error,
            system_commit().as_ref(),
        ));
    } else {
        tracing::warn!(len, %error, "committing {what} failed");
    }
}

/// The system's commit charge and limit, if it tells.
fn system_commit() -> Option<PERFORMANCE_INFORMATION> {
    let mut info = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
    let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
    queried.then_some(info)
}

/// A memory failure the user can act on goes to the crash log, as the
/// title is unlikely to survive it.
fn report(message: &str) {
    victory_loader::log_crash(message);
}

fn commit_limit_message(
    len: usize,
    what: &str,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of {what}: {error}.\n"
    );
    push_system_commit(&mut message, info);
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// What to say when Wine refuses the section behind physical pages: the
/// request, the error, and what the system has committed.
fn section_refused_message(
    size: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Wine could not create a section for {size} bytes of physical pages: {error}.\n"
    );
    push_system_commit(&mut message, info);
    message
        .push_str("The pages are a sparse file there: free memory and disk space, then retry.\n");
    message
}

fn push_system_commit(message: &mut String, info: Option<&PERFORMANCE_INFORMATION>) {
    use std::fmt::Write;

    let Some(info) = info else { return };
    // Saturating: the figures are the host's, and a report must not fail.
    let committed = info.CommitTotal.saturating_mul(info.PageSize) / MIB;
    let limit = info.CommitLimit.saturating_mul(info.PageSize) / MIB;
    let _ = writeln!(
        message,
        "System commit: {committed} MiB of {limit} MiB in use."
    );
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    // Callers have checked the range; one that wraps holds no view.
    let Some(end) = base.checked_add(len) else {
        return;
    };
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        for k in 0..size / PAGE {
            let key = slice.key(k);
            if let Some(addresses) = physical.shown.get_mut(&key) {
                addresses.retain(|&a| a != at + k * PAGE);
                if addresses.is_empty() {
                    physical.shown.remove(&key);
                }
            }
        }
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Memory::{MEM_FREE, PAGE_READONLY};

    use super::*;

    /// What reached the crash log (`victory_loader::log_crash`) and holds
    /// `part`.
    fn reported(part: &str) -> Vec<String> {
        let mut all = victory_loader::logged_crashes();
        all.retain(|m| m.contains(part));
        all
    }

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    /// Tries `attempt` until it gives something. It gives `None` when the
    /// address it had just released was taken before it looked again: the
    /// tests of this crate run side by side in one process and all of them
    /// allocate, so nothing may be asserted of a released address without
    /// this. 64 losses running fail the test.
    fn until_not_taken<T>(what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
        for _ in 0..64 {
            if let Some(result) = attempt() {
                return result;
            }
        }
        panic!("{what}: 64 addresses running were taken meanwhile, or never came free");
    }

    /// What a query answers for an address whose memory was released, from
    /// records as the host gives them: no test may ask the host itself,
    /// which hands the address to another thread whenever it likes.
    #[test]
    fn memory_that_is_not_committed_reads_no_flags() {
        let asked = std::cell::Cell::new(0);
        let recorded = |flags: Option<u64>| {
            let asked = &asked;
            move |_: &MEMORY_BASIC_INFORMATION| {
                asked.set(asked.get() + 1);
                flags
            }
        };
        let base = 0x2_4000_0000usize as *mut c_void;
        // Released: the host has nothing there, and a record that is still
        // kept for the address (the title's `VirtualFree` is not seen)
        // gives its flags to nothing.
        let released = MEMORY_BASIC_INFORMATION {
            BaseAddress: base,
            RegionSize: 2 * PAGE,
            State: MEM_FREE,
            Protect: PAGE_NOACCESS,
            ..Default::default()
        };
        assert_eq!(flags_for(&released, recorded(Some(1 | 6 << 12))), 0);
        // Reserved and never committed, or decommitted.
        let reserved = MEMORY_BASIC_INFORMATION {
            AllocationBase: base,
            AllocationProtect: PAGE_READWRITE,
            State: MEM_RESERVE,
            Type: MEM_PRIVATE,
            ..released
        };
        assert_eq!(flags_for(&reserved, recorded(Some(GRAPHICS | 7 << 12))), 0);
        assert_eq!(asked.get(), 0, "no record is looked up for such memory");
        // Committed: the record's flags, bit for bit, or graphics memory's.
        let committed = MEMORY_BASIC_INFORMATION {
            State: MEM_COMMIT,
            Protect: PAGE_READWRITE,
            ..reserved
        };
        for flags in [0, 1, GRAPHICS | 7 << 12, 2 | 5 << 12 | 1 << 20] {
            assert_eq!(flags_for(&committed, recorded(Some(flags))), flags);
        }
        assert_eq!(flags_for(&committed, recorded(None)), GRAPHICS);
        assert_eq!(asked.get(), 5);
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn aligned_32bit_default_allocations() {
        // Alignments above what the host gives unasked: 128 KiB, 2 MiB and
        // 16 MiB. Three blocks of each, held together: one could be aligned
        // by chance.
        for field in [0x11u64, 0x15, 0x18] {
            let (align, attributes) = (1usize << field, field << ALIGNMENT_SHIFT | ALLOC_32BIT);
            let blocks = [(); 3].map(|()| XMemAllocDefault(3 * PAGE, attributes));
            for p in blocks {
                assert!(!p.is_null(), "field {field:#x}");
                assert_eq!(p as usize % align, 0, "field {field:#x}: {p:p}");
                assert!(p as usize + 3 * PAGE <= 1 << 32, "{p:p}");
                // SAFETY: three pages were allocated.
                unsafe { p.cast::<u8>().write_bytes(0xab, 3 * PAGE) };
                assert_eq!(query(p).host.State, MEM_COMMIT);
            }
            for p in blocks {
                XMemFreeDefault(p, attributes);
            }
            // A freed block's address is the host's again. The host may
            // give it to another thread before it is looked at, so a block
            // more is freed until one is seen free.
            until_not_taken("a freed 32-bit block", || {
                let p = XMemAllocDefault(3 * PAGE, attributes);
                assert!(!p.is_null(), "field {field:#x}");
                XMemFreeDefault(p, attributes);
                (query(p).host.State == MEM_FREE).then_some(())
            });
        }
        // Alignments the host gives anyway still allocate.
        for field in [0u64, 4, 0xc, 0x10] {
            let (align, attributes) = (1usize << field, field << ALIGNMENT_SHIFT | ALLOC_32BIT);
            let p = XMemAllocDefault(100, attributes);
            assert!(!p.is_null() && (p as usize) < 1 << 32, "field {field:#x}");
            assert_eq!(p as usize % align, 0, "field {field:#x}: {p:p}");
            XMemFreeDefault(p, attributes);
        }
        // No size: nothing to allocate, as the host says.
        assert!(XMemAllocDefault(0, 0x15 << ALIGNMENT_SHIFT | ALLOC_32BIT).is_null());
    }

    #[test]
    fn part_of_a_64k_page_reservation_can_be_committed() {
        let flags = 1 | 5 << 12;
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            5 * PAGE,
            MEM_RESERVE | PAGES_64K,
            flags,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        let state = |page: usize| query(at(page)).host.State;
        let commit = |base: *mut c_void, size| {
            XMemVirtualAlloc(base, size, MEM_COMMIT | PAGES_64K, GRAPHICS, PAGE_READWRITE)
        };
        // One page in the middle, as the host commits inside a reservation.
        assert_eq!(commit(at(1), PAGE), at(1));
        assert_eq!(
            [0, 1, 2, 3, 4].map(state),
            [
                MEM_RESERVE,
                MEM_COMMIT,
                MEM_RESERVE,
                MEM_RESERVE,
                MEM_RESERVE
            ]
        );
        let info = query(at(1));
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags, flags, "the reservation's memory");
        // SAFETY: the page is committed read-write.
        unsafe {
            at(1).cast::<u8>().write_bytes(0x6b, PAGE);
            assert_eq!(at(1).cast::<u8>().add(PAGE - 1).read(), 0x6b);
        }
        // Committed again, it is the same memory.
        assert_eq!(commit(at(1), PAGE), at(1));
        // SAFETY: still committed.
        assert_eq!(unsafe { at(1).cast::<u8>().read() }, 0x6b);

        // A few bytes inside a page commit the 64 KiB page they are in,
        // and the answer is the host's: the first 4 KiB page of the bytes.
        let inside = at(3).wrapping_byte_add(0x1800);
        assert_eq!(commit(inside, 0x10), at(3).wrapping_byte_add(0x1000));
        assert_eq!([2, 3, 4].map(state), [MEM_RESERVE, MEM_COMMIT, MEM_RESERVE]);
        // SAFETY: the whole 64 KiB page is committed read-write.
        unsafe { at(3).cast::<u8>().write_bytes(0x2e, PAGE) };

        // The rest still takes physical pages, next to the committed part.
        let pages = allocate(2);
        assert_eq!(XMemMapPhysicalPages(at(2), 1, pages.as_ptr()), at(2));
        assert_eq!(XMemMapPhysicalPages(at(4), 1, pages[1..].as_ptr()), at(4));
        // SAFETY: mapped read-write.
        unsafe { at(2).cast::<u8>().write(0x4d) };

        // Refused, with nothing changed: bytes that run past the
        // reservation, bytes with no end, no bytes, and bytes that cross
        // what is committed or mapped already.
        let null = std::ptr::null_mut::<c_void>();
        assert_eq!(commit(at(0), 6 * PAGE), null);
        assert_eq!(commit(at(0), usize::MAX), null);
        assert_eq!(commit(at(0), usize::MAX - a as usize + 1), null);
        assert_eq!(commit(at(0), 0), null);
        assert_eq!(commit(at(0), 2 * PAGE), null, "across a committed part");
        assert_eq!(commit(at(0), 3 * PAGE), null, "and a mapped one");
        assert_eq!(
            [0, 1, 2, 3, 4].map(state),
            [MEM_RESERVE, MEM_COMMIT, MEM_COMMIT, MEM_COMMIT, MEM_COMMIT]
        );
        assert_eq!(mine(a, 5), [(at(2) as usize, PAGE), (at(4) as usize, PAGE)]);
        // SAFETY: all three are still what they were.
        unsafe {
            assert_eq!(at(1).cast::<u8>().read(), 0x6b);
            assert_eq!(at(2).cast::<u8>().read(), 0x4d);
            assert_eq!(at(3).cast::<u8>().add(PAGE - 1).read(), 0x2e);
        }
        // The first page, the last of the placeholder left.
        assert_eq!(commit(at(0), 1), at(0));
        assert_eq!(state(0), MEM_COMMIT);

        assert_eq!(XMemMapPhysicalPages(at(2), 1, std::ptr::null()), at(2));
        assert_eq!(XMemMapPhysicalPages(at(4), 1, std::ptr::null()), at(4));
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        // A commit at an address that is in no reservation of ours fails as
        // the host fails it.
        let nowhere = (usize::MAX - 2 * PAGE + 1) as *mut c_void;
        assert_eq!(commit(nowhere, PAGE), null);
    }

    #[test]
    fn physical_pages_do_not_replace_committed_memory() {
        let flags = 1 | 6 << 12;
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE | PAGES_64K,
            flags,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let at = |page: usize| a.wrapping_byte_add(page * PAGE);
        let state = |page: usize| query(at(page)).host.State;
        let commit = |base: *mut c_void, protect| {
            XMemVirtualAlloc(base, PAGE, MEM_COMMIT | PAGES_64K, GRAPHICS, protect)
        };
        // Memory of the reservation's own in its second page, and a view
        // in its first: what a refused call must leave as it is.
        assert_eq!(commit(at(1), PAGE_READWRITE), at(1));
        let pages = allocate(3);
        assert_eq!(XMemMapPhysicalPages(at(0), 1, pages.as_ptr()), at(0));
        // SAFETY: both pages are read-write.
        unsafe {
            at(0).cast::<u8>().write(0x4d);
            at(1).cast::<u8>().write_bytes(0x6b, PAGE);
        }
        let before = mine(a, 4);
        assert_eq!(before, [(a as usize, PAGE)]);
        let unchanged = |what: &str| {
            assert_eq!(mine(a, 4), before, "{what}");
            assert_eq!(
                [0, 1, 2, 3].map(state),
                [MEM_COMMIT, MEM_COMMIT, MEM_RESERVE, MEM_RESERVE],
                "{what}"
            );
            let info = query(at(1));
            assert_eq!(info.host.Type, MEM_PRIVATE, "{what}");
            assert_eq!(info.flags, flags, "{what}");
            // SAFETY: the states above say both pages are still there.
            unsafe {
                assert_eq!(at(0).cast::<u8>().read(), 0x4d, "{what}");
                assert_eq!(at(1).cast::<u8>().read(), 0x6b, "{what}");
                assert_eq!(at(1).cast::<u8>().add(PAGE - 1).read(), 0x6b, "{what}");
            }
        };
        let null = std::ptr::null_mut::<c_void>();

        // Pages over the committed memory: on it alone, from the view into
        // it, from it into the placeholder behind, and across all three.
        for (first, n) in [(1, 1), (0, 2), (1, 2), (0, 3)] {
            let what = format!("{n} pages at page {first}");
            let mapped = XMemMapPhysicalPages(at(first), n, pages.as_ptr());
            assert_eq!(mapped, null, "{what}");
            unchanged(&what);
        }
        let addresses = [at(1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            0
        );
        unchanged("scattered");
        // The GPU's page-mapping copies: of what the view shows, and of two
        // pages mapped elsewhere over the view and the memory behind it.
        assert!(!map_alias(at(1) as u64, a as u64, PAGE as u64));
        unchanged("the GPU's copy");
        let elsewhere = mapped(2, &pages[1..]);
        assert!(!map_alias(a as u64, elsewhere as u64, 2 * PAGE as u64));
        unchanged("the GPU's copy of two pages");
        assert_eq!(
            XMemMapPhysicalPages(elsewhere, 2, std::ptr::null()),
            elsewhere
        );
        // The mapping itself looks again, whoever calls it.
        let slice = physical().pages.get(&pages[0]).cloned().expect("known");
        assert!(!map(&mut physical(), at(1) as usize, PAGE, slice));
        unchanged("the mapping itself");
        // A commit the host refuses (no protection is both of these) does
        // not take the memory back either.
        assert_eq!(commit(at(1), PAGE_READWRITE | PAGE_READONLY), null);
        unchanged("a refused commit");
        assert_eq!(commit(at(2), PAGE_READWRITE | PAGE_READONLY), null);
        unchanged("a refused commit in the placeholder");

        // An unmap of the range returns views and nothing else.
        assert_eq!(XMemMapPhysicalPages(at(0), 3, std::ptr::null()), at(0));
        assert_eq!(mine(a, 4), []);
        assert_eq!([0, 1, 2].map(state), [MEM_RESERVE, MEM_COMMIT, MEM_RESERVE]);
        // SAFETY: still committed.
        assert_eq!(unsafe { at(1).cast::<u8>().read() }, 0x6b);

        // The title's way to map there is the host's: it gives the memory
        // back to a placeholder first.
        // SAFETY: memory of this test's reservation.
        let released = unsafe { VirtualFree(at(1), PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
        assert_ne!(released, 0);
        assert_eq!(state(1), MEM_RESERVE);
        assert_eq!(XMemMapPhysicalPages(at(1), 1, pages.as_ptr()), at(1));
        let info = query(at(1));
        assert_eq!((info.host.State, info.host.Type), (MEM_COMMIT, MEM_MAPPED));
        assert_eq!(info.flags, GRAPHICS, "a view, not the reservation's memory");
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(1).cast::<u8>().read() }, 0x4d, "the first page");
        assert_eq!(XMemMapPhysicalPages(at(1), 1, std::ptr::null()), at(1));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// How long a thread started between another's check and its release
    /// gets to finish. It never does where both hold one lock, so this is
    /// what each interleaving below costs; without the lock its calls take
    /// well under a millisecond.
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(250);

    /// Runs `first` on this thread and, once it has asked the host what its
    /// range holds and before it releases (`checked`), starts `second` on
    /// another thread and gives it `WINDOW` to finish. `first`'s result,
    /// whether `second` finished in between, and `second`'s result.
    fn interleaved<A, B: Send + 'static>(
        first: impl FnOnce() -> A,
        second: impl FnOnce() -> B + Send + 'static,
    ) -> (A, bool, B) {
        type Started<B> = Option<(std::thread::JoinHandle<B>, bool)>;
        let started: std::rc::Rc<std::cell::RefCell<Started<B>>> = Default::default();
        let hook = {
            let started = started.clone();
            move || {
                let (done, finished) = std::sync::mpsc::channel();
                let thread = std::thread::spawn(move || {
                    let result = second();
                    let _ = done.send(());
                    result
                });
                let between = finished.recv_timeout(WINDOW).is_ok();
                *started.borrow_mut() = Some((thread, between));
            }
        };
        CHECKED.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        let a = first();
        // A `first` that never reached its release leaves the hook unused.
        CHECKED.with(|slot| slot.borrow_mut().take());
        let (thread, between) = started
            .borrow_mut()
            .take()
            .expect("the first call got as far as its release");
        (a, between, thread.join().expect("the second call returned"))
    }

    /// A commit and a mapping of the same 64 KiB page from two threads: the
    /// one that asked the host first also releases first. Without one lock
    /// over both steps the other gets in between, and the release then
    /// destroys memory it was told was not there.
    #[test]
    fn no_commit_or_mapping_gets_between_a_check_and_its_release() {
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            1 | 6 << 12,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let base = a as usize;
        let at = move |page: usize| base + page * PAGE;
        let kind = |page: usize| {
            let info = query(at(page) as *const c_void);
            (info.host.State, info.host.Type)
        };
        let commit = |at: usize| {
            let (kind, base) = (MEM_COMMIT | PAGES_64K, at as *mut c_void);
            XMemVirtualAlloc(base, PAGE, kind, GRAPHICS, PAGE_READWRITE) as usize
        };
        let map = |at: usize, page: usize| {
            XMemMapPhysicalPages(at as *mut c_void, 1, [page].as_ptr()) as usize
        };
        let pages = allocate(1);
        assert!(write_physical(&pages, 0, &[0x4d]));
        let page = pages[0];
        let first_byte = |at: usize| {
            // SAFETY: callers have seen the page committed.
            unsafe { (at as *const u8).read() }
        };

        // A mapping that has checked its page, and a commit of that page.
        let (mapped, between, _) = interleaved(|| map(at(0), page), move || commit(at(0)));
        assert!(
            !between,
            "a commit ran between the mapping's check and release"
        );
        assert_eq!(mapped, at(0));
        assert_eq!(
            kind(0),
            (MEM_COMMIT, MEM_MAPPED),
            "the view is what is there"
        );
        assert_eq!(first_byte(at(0)), 0x4d);

        // A commit that has checked its page, and a mapping of that page.
        let (committed, between, mapped) = interleaved(
            || {
                let committed = commit(at(1));
                if committed != 0 {
                    // SAFETY: committed read-write just now.
                    unsafe { (committed as *mut u8).write_bytes(0x6b, PAGE) };
                }
                committed
            },
            move || map(at(1), page),
        );
        assert!(
            !between,
            "a mapping ran between the commit's check and release"
        );
        assert_eq!(committed, at(1));
        assert_eq!(mapped, 0, "the mapping came second and found memory there");
        assert_eq!(kind(1), (MEM_COMMIT, MEM_PRIVATE));
        assert_eq!(first_byte(at(1)), 0x6b);
        assert_eq!(first_byte(at(1) + PAGE - 1), 0x6b);

        // A mapping that has checked a page the title decommitted, and a
        // commit the host takes at once (no placeholder is left there).
        assert_eq!(commit(at(2)), at(2));
        // SAFETY: memory of this test's reservation.
        let decommitted = unsafe {
            VirtualFree(
                at(2) as *mut c_void,
                PAGE,
                windows_sys::Win32::System::Memory::MEM_DECOMMIT,
            )
        };
        assert_ne!(decommitted, 0);
        assert_eq!(kind(2).0, MEM_RESERVE);
        let (mapped, between, _) = interleaved(|| map(at(2), page), move || commit(at(2)));
        assert!(
            !between,
            "a commit ran between the mapping's check and release"
        );
        assert_eq!(mapped, at(2));
        assert_eq!(kind(2), (MEM_COMMIT, MEM_MAPPED));
        assert_eq!(first_byte(at(2)), 0x4d);

        for page in [0, 2] {
            let at = at(page) as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null()), at);
        }
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    }

    static HOOKED_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static HOOKED_FREES: AtomicUsize = AtomicUsize::new(0);
    /// The last attributes each hook was called with.
    static HOOKED_WITH: Mutex<[u64; 2]> = Mutex::new([0; 2]);

    /// A title's allocation routine: counts, and allocates as we would.
    extern "system" fn title_alloc(size: usize, attributes: u64) -> *mut c_void {
        HOOKED_ALLOCS.fetch_add(1, Ordering::Relaxed);
        HOOKED_WITH.lock().unwrap()[0] = attributes;
        XMemAllocDefault(size, attributes)
    }

    extern "system" fn title_free(address: *mut c_void, attributes: u64) {
        HOOKED_FREES.fetch_add(1, Ordering::Relaxed);
        HOOKED_WITH.lock().unwrap()[1] = attributes;
        XMemFreeDefault(address, attributes);
    }

    #[test]
    fn allocation_hooks_are_called_one_by_one() {
        let calls = || {
            (
                HOOKED_ALLOCS.load(Ordering::Relaxed),
                HOOKED_FREES.load(Ordering::Relaxed),
            )
        };
        let (alloc, free) = (
            title_alloc as AllocRoutine as usize,
            title_free as FreeRoutine as usize,
        );
        let attributes = 6 << ALIGNMENT_SHIFT | 1 << 29;
        // Each table, and how many calls reach the title's two routines.
        for (table, (allocs, frees)) in [
            ([0, 0], (0, 0)),
            ([alloc, 0], (1, 0)),
            ([0, free], (0, 1)),
            ([alloc, free], (1, 1)),
        ] {
            let before = calls();
            let p = alloc_through(table[0], 200, attributes);
            assert!(!p.is_null(), "{table:x?}");
            assert_eq!(p as usize % 64, 0);
            // SAFETY: 200 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xcd, 200) };
            free_through(table[1], p, attributes);
            let after = calls();
            assert_eq!((after.0 - before.0, after.1 - before.1), (allocs, frees));
        }
        // The title's routines get the caller's arguments as they came.
        assert_eq!(*HOOKED_WITH.lock().unwrap(), [attributes; 2]);
        // A null block reaches the title's routine too: it is its to judge.
        let before = calls();
        free_through(free, std::ptr::null_mut(), 0);
        assert_eq!(calls().1 - before.1, 1);
    }

    #[test]
    fn the_hook_table_is_read_where_the_title_exports_it() {
        // A table as a title's exe exports it: two routine addresses.
        static TABLE: [usize; 2] = [0x1111_0000, 0x2222_0000];
        assert_eq!(read_hooks((&raw const TABLE) as usize), TABLE);
        // An image without the export has no table, and no table no hooks.
        // SAFETY: a module that is always loaded.
        let kernel32 = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(
                c"kernel32.dll".as_ptr().cast(),
            )
        };
        assert!(!kernel32.is_null());
        assert_eq!(hook_table(kernel32 as usize), 0);
        assert_eq!(read_hooks(0), [0; 2]);
        // No title is mapped in this process, so nothing was looked up and
        // the exports go to our own allocator.
        assert_eq!(crate::runtime::main_image(), None);
        assert_eq!(hooks(), [0; 2]);
        let p = XMemAlloc(64, 0);
        assert!(!p.is_null());
        XMemFree(p, 0);
        XMemFree(std::ptr::null_mut(), 0);
    }

    /// Pins where default blocks come from today: the host's heap, wherever
    /// that lies, not a reservation below 2^40 as on the console. Our
    /// shortcut, not asserted as right: change the test with it.
    #[test]
    fn default_blocks_come_from_the_host_heap_today() {
        for (size, attributes) in [(100, 0), (100, 1 << 29), (3 * MIB, 0xc << ALIGNMENT_SHIFT)] {
            let p = XMemAllocDefault(size, attributes);
            assert!(!p.is_null());
            let info = query(p);
            assert_eq!(info.host.State, MEM_COMMIT);
            // Inside an allocation of the heap's, behind our header.
            assert!((info.host.AllocationBase as usize) < p as usize, "{p:p}");
            assert_eq!(info.flags, GRAPHICS);
            // SAFETY: the header we wrote below the block.
            let [bytes, align] = unsafe { p.cast::<[usize; 2]>().sub(1).read() };
            assert_eq!(bytes, size + align);
            eprintln!(
                "a default block of {size} bytes: {p:p}, below 2^40: {}",
                (p as usize) < 1 << 40
            );
            XMemFreeDefault(p, attributes);
        }
        // A size that leaves no room for the header is no block.
        assert!(XMemAllocDefault(usize::MAX, 0).is_null());
        assert!(XMemAllocDefault(usize::MAX - 8, 0x1f << ALIGNMENT_SHIFT).is_null());
        XMemFreeDefault(std::ptr::null_mut(), 0);
    }

    /// Pins what the working-set statistics say today: every field filled
    /// whatever is asked for, and no use per bandwidth class. Our shortcut,
    /// not asserted as right: change the test with it.
    #[test]
    fn working_set_statistics_ignore_what_is_asked_today() {
        let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
        for which in [0, 1, 0x2000_0001, u32::MAX] {
            let mut stats = WorkingSet {
                tools_limit: 9,
                gpu_optimal_used: 9,
                ..Default::default()
            };
            assert_eq!(XMemGetWorkingSetStatistics(which, &mut stats), 1);
            assert_eq!(stats.gpu_optimal_limit, gpu_optimal, "{which:#x}");
            // Another test transfers memory meanwhile, so no exact figure.
            assert!(stats.title_limit <= gpu_optimal + standard, "{which:#x}");
            assert!(stats.title_limit > gpu_optimal, "{which:#x}");
            assert!(stats.title_used > 0, "{which:#x}");
            assert_eq!(stats.gpu_optimal_used, 0, "{which:#x}");
            let tools = (
                stats.tools_limit,
                stats.tools_used,
                stats.tools_gpu_optimal_limit,
                stats.tools_gpu_optimal_used,
            );
            assert_eq!(tools, (0, 0, 0, 0), "{which:#x}");
            let rest = (
                stats.stack_used,
                stats.physical_pages,
                stats.kernel_pool_used,
                stats.kernel_pool_limit,
            );
            assert_eq!(rest, (0, 0, 0, 0), "{which:#x}");
        }
        assert_eq!(XMemGetWorkingSetStatistics(0, std::ptr::null_mut()), 0);
    }

    #[test]
    fn virtual_alloc_replace_placeholder_and_fixed_base_paths() {
        // A title that splits its own 64 KiB-page reservation tells us of
        // each part: it stays the placeholder it is, whole, and the call
        // answers with the address it was given.
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let second = a.wrapping_byte_add(PAGE);
        let split = MEM_RESERVE | PAGES_64K | MEM_REPLACE_PLACEHOLDER;
        assert_eq!(
            XMemVirtualAlloc(second, PAGE, split, 0, PAGE_READWRITE),
            second
        );
        assert_eq!(query(second).host.State, MEM_RESERVE);
        let pages = allocate(2);
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a, "still one");
        assert_eq!(mine(a, 2), [(a as usize, 2 * PAGE)]);
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);

        // A commit at an address inside a reservation: that page and no
        // other, with the console's GPU access bits taken off the
        // protection, and no access where none of the host's is named.
        const GPU_READ_WRITE: u32 = 0x2000;
        let r = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!r.is_null());
        let at = |page: usize| r.wrapping_byte_add(page * PAGE);
        let rw = PAGE_READWRITE | GPU_READ_WRITE;
        assert_eq!(
            XMemVirtualAlloc(at(1), PAGE, MEM_COMMIT, GRAPHICS, rw),
            at(1)
        );
        assert_eq!(
            XMemVirtualAlloc(at(2), PAGE, MEM_COMMIT, GRAPHICS, GPU_READ_WRITE),
            at(2)
        );
        let states = [0, 1, 2, 3].map(|page| query(at(page)).host.State);
        assert_eq!(states, [MEM_RESERVE, MEM_COMMIT, MEM_COMMIT, MEM_RESERVE]);
        assert_eq!(query(at(1)).host.Protect, PAGE_READWRITE);
        assert_eq!(query(at(2)).host.Protect, PAGE_NOACCESS);
        // SAFETY: committed read-write.
        unsafe { at(1).cast::<u8>().write_bytes(1, PAGE) };
        // Past the reservation, or no bytes: the host's refusal, as null.
        let null = std::ptr::null_mut::<c_void>();
        assert_eq!(
            XMemVirtualAlloc(at(3), 2 * PAGE, MEM_COMMIT, GRAPHICS, rw),
            null
        );
        assert_eq!(XMemVirtualAlloc(at(3), 0, MEM_COMMIT, GRAPHICS, rw), null);
        assert_eq!(XMemVirtualAlloc(null, 0, MEM_RESERVE, GRAPHICS, rw), null);
        assert_eq!(query(at(3)).host.State, MEM_RESERVE);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(r, 0, MEM_RELEASE) }, 0);

        // A reservation at an address of the title's choice is made there
        // or not at all: the address limits of the flags are for addresses
        // we choose. An address above 4 GiB that was free a moment ago;
        // 32-bit memory is asked for. Other tests reserve meanwhile and may
        // take the address, so it is tried with another until one is free.
        until_not_taken("a reservation at a fixed base", || {
            let r = XMemVirtualAlloc(
                std::ptr::null_mut(),
                4 * PAGE,
                MEM_RESERVE,
                GRAPHICS,
                PAGE_NOACCESS,
            );
            assert!(!r.is_null() && r as usize >= 1 << 32, "{r:p}");
            // SAFETY: our own reservation.
            assert_ne!(unsafe { VirtualFree(r, 0, MEM_RELEASE) }, 0);
            let kind = MEM_RESERVE | MEM_COMMIT;
            let fixed = XMemVirtualAlloc(r, 4 * PAGE, kind, ADDRESS_32BIT, PAGE_READWRITE);
            if fixed.is_null() {
                return None;
            }
            assert_eq!(fixed, r);
            assert_eq!(query(r).flags, ADDRESS_32BIT);
            // SAFETY: our own reservation.
            assert_ne!(unsafe { VirtualFree(r, 0, MEM_RELEASE) }, 0);
            Some(())
        });
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // What the released address answers is the host's to change at once:
        // `memory_that_is_not_committed_reads_no_flags`.
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn query_reports_the_flags_it_was_given() {
        // Whatever the title said of a region is what it reads back, bit
        // for bit: no flags, a memory type alone, an address field, bits
        // we give no meaning.
        let given = [0, 1, GRAPHICS, GRAPHICS | 7 << 12, 2 | 5 << 12 | 1 << 20];
        let regions = given.map(|flags| {
            let p = XMemVirtualAlloc(
                std::ptr::null_mut(),
                2 * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                flags,
                PAGE_READWRITE,
            );
            assert!(!p.is_null(), "{flags:#x}");
            p
        });
        for (p, flags) in regions.into_iter().zip(given) {
            let last = p.wrapping_byte_add(2 * PAGE - 1);
            for at in [p, p.wrapping_byte_add(PAGE), last] {
                let info = query(at);
                assert_eq!(info.host.State, MEM_COMMIT);
                assert_eq!(info.flags, flags, "{at:p}");
                assert_eq!((info.tag_code, info.tag), (0, [0; 4]));
            }
        }
        // A region reserved and committed later, at its own address.
        let flags = 1 | 6 << 12;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE,
            flags,
            PAGE_READWRITE,
        );
        assert!(!p.is_null());
        assert_eq!(query(p).flags, 0, "reserved only");
        let second = p.wrapping_byte_add(PAGE);
        // The commit's own flags do not rename the region.
        assert_eq!(
            XMemVirtualAlloc(second, PAGE, MEM_COMMIT, GRAPHICS, PAGE_READWRITE),
            second
        );
        assert_eq!(query(second).flags, flags);
        assert_eq!(query(p).flags, 0, "still reserved only");
        // Freed, the addresses answer for no memory: from the host's own
        // records in `memory_that_is_not_committed_reads_no_flags`, since
        // the host may give a freed address away before it is asked.
        for q in regions.into_iter().chain([p]) {
            // SAFETY: our own reservations.
            assert_ne!(unsafe { VirtualFree(q, 0, MEM_RELEASE) }, 0);
        }
        // The same range handed out again answers with its new flags.
        // Other tests reserve meanwhile and may take the released base, so
        // it is tried with another region until the base is still free.
        until_not_taken("a region at a released base", || {
            let p = XMemVirtualAlloc(
                std::ptr::null_mut(),
                4 * PAGE,
                MEM_RESERVE,
                flags,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            let second = p.wrapping_byte_add(PAGE);
            assert_eq!(
                XMemVirtualAlloc(second, PAGE, MEM_COMMIT, GRAPHICS, PAGE_READWRITE),
                second
            );
            assert_eq!(query(second).flags, flags);
            // SAFETY: our own reservation.
            assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
            let kind = MEM_RESERVE | MEM_COMMIT;
            let again = XMemVirtualAlloc(p, 4 * PAGE, kind, 2, PAGE_READWRITE);
            if again.is_null() {
                return None;
            }
            assert_eq!(again, p, "a fixed base is that base or nothing");
            assert_eq!(query(second).flags, 2);
            // SAFETY: our own reservation.
            assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
            Some(())
        });
    }

    /// Reserves two 64 KiB pages with `XMemVirtualAlloc`, frees them with
    /// the host's `VirtualFree` (which this module does not see) and has
    /// the host allocate `size` bytes (eight pages at most) at the same
    /// address: that address. Other tests reserve meanwhile and may take
    /// the address, so it tries again with another.
    fn reallocated_by_the_host(flags: u64, size: usize, protect: u32) -> *mut c_void {
        let mut errors = Vec::new();
        for _ in 0..64 {
            // SAFETY: plain reservations and their release. The first
            // finds room for the longest allocation asked for.
            let again = unsafe {
                let room = VirtualAlloc(std::ptr::null(), 8 * PAGE, MEM_RESERVE, PAGE_NOACCESS);
                assert!(!room.is_null());
                assert_ne!(VirtualFree(room, 0, MEM_RELEASE), 0);
                let kind = MEM_RESERVE | MEM_COMMIT;
                let p = XMemVirtualAlloc(room, 2 * PAGE, kind, flags, PAGE_READWRITE);
                if p.is_null() {
                    errors.push(std::io::Error::last_os_error());
                    continue;
                }
                assert_eq!(p, room);
                assert_eq!(query(p).flags, flags);
                assert_ne!(VirtualFree(p, 0, MEM_RELEASE), 0);
                VirtualAlloc(p, size, kind, protect)
            };
            if again.is_null() {
                errors.push(std::io::Error::last_os_error());
                continue;
            }
            return again;
        }
        panic!("64 addresses running were taken meanwhile: {errors:?}");
    }

    #[test]
    fn a_region_the_host_released_leaves_its_flags_to_nothing_else() {
        let flags = 1 | 6 << 12;
        // Another allocation at the same base: longer, shorter, or of
        // another protection. None of them is the region the flags were
        // given for, so each answers as any memory of the host's does.
        for (size, protect) in [
            (4 * PAGE, PAGE_READWRITE),
            (PAGE, PAGE_READWRITE),
            (2 * PAGE, PAGE_READONLY),
        ] {
            let p = reallocated_by_the_host(flags, size, protect);
            let info = query(p);
            assert_eq!(info.host.State, MEM_COMMIT);
            assert_eq!(info.flags, GRAPHICS, "{size:#x} bytes, {protect:#x}");
            // SAFETY: the host's allocation above.
            assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        }
        // A size that is no multiple of a page: the region is what the
        // host reserved, to the end of its last page.
        let size = 2 * PAGE - 0x800;
        let kind = MEM_RESERVE | MEM_COMMIT;
        let p = XMemVirtualAlloc(std::ptr::null_mut(), size, kind, flags, PAGE_READWRITE);
        assert!(!p.is_null());
        for at in [
            p,
            p.wrapping_byte_add(size),
            p.wrapping_byte_add(2 * PAGE - 1),
        ] {
            assert_eq!(query(at).flags, flags, "{at:p}");
        }
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    /// Pins the one case a released region's flags outlive it today: the
    /// host allocates the same range again with the same protection, and
    /// nothing it tells of the two allocations differs. Our shortcut, not
    /// asserted as right: change the test with it.
    #[test]
    fn an_identical_host_allocation_reads_a_released_regions_flags_today() {
        let flags = 1 | 6 << 12;
        let p = reallocated_by_the_host(flags, 2 * PAGE, PAGE_READWRITE);
        assert_eq!(query(p).flags, flags);
        // Released, the address reads no flags, from whatever record: see
        // `memory_that_is_not_committed_reads_no_flags`.
        // SAFETY: the host's allocation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    /// Pins what a 32-bit default block reads back today: the flags of the
    /// reservation we make for it, address bits and all, where every block
    /// read plain graphics memory before regions kept their flags. Our
    /// choice of flags, not known console behaviour: change the test with
    /// it.
    #[test]
    fn a_32bit_default_block_reads_its_reservations_flags_today() {
        let p = XMemAllocDefault(100, ALLOC_32BIT);
        assert!(!p.is_null());
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.flags, GRAPHICS | ADDRESS_32BIT);
        XMemFreeDefault(p, ALLOC_32BIT);
    }

    /// Pins what `XMemVirtualQuery` answers today for memory that did not
    /// come from `XMemVirtualAlloc`. This is our shortcut, not known console
    /// behaviour: change the test with the shortcut.
    #[test]
    fn other_committed_memory_reports_graphics_and_no_tag_today() {
        // The host's own allocation.
        // SAFETY: a plain reservation of ours.
        let host = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        assert!(!host.is_null());
        // A block of the default allocator (the host's heap).
        let block = XMemAllocDefault(100, 0);
        assert!(!block.is_null());
        // Physical pages mapped at the start of a reservation whose flags
        // say something else: the view is not the reservation's memory.
        let pages = allocate(1);
        let reserved = XMemVirtualAlloc(
            std::ptr::null_mut(),
            PAGE,
            MEM_RESERVE | PAGES_64K,
            1,
            PAGE_READWRITE,
        );
        assert!(!reserved.is_null());
        assert_eq!(query(reserved).flags, 0, "a placeholder is not committed");
        assert_eq!(XMemMapPhysicalPages(reserved, 1, pages.as_ptr()), reserved);
        for at in [host, block, reserved] {
            let info = query(at);
            assert_eq!(info.host.State, MEM_COMMIT, "{at:p}");
            assert_eq!(info.flags, GRAPHICS, "{at:p}");
            assert_eq!((info.tag_code, info.tag), (0, [0; 4]), "{at:p}");
        }
        assert_eq!(
            XMemMapPhysicalPages(reserved, 1, std::ptr::null()),
            reserved
        );
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
        XMemFreeDefault(block, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(host, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn a_query_without_a_structure_fails() {
        let here = (&raw const PHYSICAL).cast::<c_void>();
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, here, std::ptr::null_mut()), 0);
        assert_eq!(query(here).host.State, MEM_COMMIT);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
        assert!(report.contains("4194304 bytes of physical pages"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
        assert!(fallback.contains("4194304 bytes of virtual memory"));
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Each mapping knows the other (the GPU side's change tracking).
        let others = |p: *mut c_void| {
            let mut out = Vec::new();
            aliases(p as usize, PAGE, &mut out);
            out.into_iter().map(|(_, a)| a).collect::<Vec<_>>()
        };
        assert_eq!(others(at(a, 2)), [at(b, 1) as usize]);
        assert_eq!(others(at(b, 1)), [at(a, 2) as usize]);
        assert!(others(a).is_empty());
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        let mut both = others(at(a, 2));
        both.sort_unstable();
        assert_eq!(both, [b as usize, at(b, 1) as usize]);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(
            others(at(a, 2)),
            [at(b, 1) as usize],
            "unmapped views are forgotten"
        );
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// Where the views are and how long: what a refused call must leave.
    fn views() -> Vec<(usize, usize)> {
        let physical = physical();
        physical.views.iter().map(|(at, v)| (*at, v.0)).collect()
    }

    /// A reservation of `n` 64 KiB pages with `pages` mapped at its start.
    fn mapped(n: usize, pages: &[usize]) -> *mut c_void {
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            n * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, pages.len(), pages.as_ptr()), a);
        a
    }

    /// The smallest page count whose length in bytes wraps to 0.
    const WRAPS: usize = usize::MAX / PAGE + 1;

    // The tests below hold the lock on nothing, and other tests map views of
    // their own meanwhile: each looks only at the views it made.
    fn mine(a: *mut c_void, n: usize) -> Vec<(usize, usize)> {
        let (from, to) = (a as usize, a as usize + n * PAGE);
        let all = views();
        all.into_iter()
            .filter(|(at, _)| (from..to).contains(at))
            .collect()
    }

    #[test]
    fn mapping_an_overflowing_page_count_changes_nothing() {
        let pages = allocate(2);
        let a = mapped(2, &pages);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(0x5c) };
        let before = mine(a, 2);
        assert_eq!(before, [(a as usize, 2 * PAGE)]);
        let null = std::ptr::null_mut::<c_void>();
        // A count whose bytes wrap to 0, to a few pages, or almost all the
        // way round: as an unmap and with page numbers.
        for n in [WRAPS, WRAPS + 1, WRAPS + 2, usize::MAX, usize::MAX / 2 + 1] {
            assert_eq!(XMemMapPhysicalPages(a, n, std::ptr::null()), null, "{n:#x}");
            assert_eq!(XMemMapPhysicalPages(a, n, pages.as_ptr()), null, "{n:#x}");
        }
        // Bytes that fit a count, from an address they do not fit after.
        let top = (usize::MAX - PAGE + 1) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(top, 2, std::ptr::null()), null);
        assert_eq!(XMemMapPhysicalPages(top, 1, std::ptr::null()), null);
        assert_eq!(XMemMapPhysicalPages(top, 2, pages.as_ptr()), null);
        let many = isize::MAX as usize / size_of::<usize>() + 1;
        // Scattered: counts no array of addresses or pages can have.
        let addresses = [a];
        for n in [many, usize::MAX] {
            assert_eq!(
                XMemMapPhysicalPagesScatter(addresses.as_ptr(), n, pages.as_ptr()),
                0
            );
            assert_eq!(
                XMemMapPhysicalPagesScatter(addresses.as_ptr(), n, std::ptr::null()),
                0
            );
        }

        assert_eq!(mine(a, 2), before);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 0x5c);
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[test]
    fn map_alias_near_the_top_of_the_address_space_is_refused() {
        let pages = allocate(2);
        let a = mapped(2, &pages);
        let b = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!b.is_null());
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(0x3d) };
        let (a64, b64, page) = (a as u64, b as u64, PAGE as u64);
        assert!(map_alias(b64, a64, page));
        let before = (mine(a, 2), mine(b, 2));
        assert_eq!(before.1, [(b as usize, PAGE)]);

        // A destination that ends past the last address.
        assert!(!map_alias(u64::MAX - page, a64, 2 * page));
        assert!(!map_alias(u64::MAX - page + 1, a64, page));
        assert!(!map_alias(u64::MAX, a64, 1));
        // The same as an unmap (`from` 0).
        assert!(!map_alias(u64::MAX - page, 0, 2 * page));
        // A length that carries a real destination past it: the view at `b`
        // lies in the wrapped range's way and must stay.
        assert!(!map_alias(b64, a64, u64::MAX - b64 + 1));
        assert!(!map_alias(b64, 0, u64::MAX - b64 + 1));
        assert!(!map_alias(b64, a64, u64::MAX));
        // A source that ends past the last address.
        assert!(!map_alias(b64, u64::MAX - page + 1, page));
        assert!(!map_alias(b64 + page, u64::MAX - page, 2 * page));

        assert_eq!((mine(a, 2), mine(b, 2)), before);
        // SAFETY: `b` still shows the first page.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 0x3d);
        assert!(map_alias(b64, 0, page));
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[test]
    fn freeing_an_overflowing_count_is_refused() {
        let pages = allocate(2);
        let a = mapped(2, &pages);
        let before = mine(a, 2);
        let known = || {
            let physical = physical();
            pages.iter().all(|p| physical.pages.contains_key(p))
        };
        // Counts whose array of page numbers no memory can hold.
        let many = isize::MAX as usize / size_of::<usize>() + 1;
        for n in [many, usize::MAX / size_of::<usize>() + 1, usize::MAX] {
            assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 0, "{n:#x}");
            assert!(known(), "{n:#x}: no page was forgotten");
        }
        // An array that would end past the last address.
        let top = (usize::MAX - 7) as *const usize;
        assert_eq!(XMemFreePhysicalPages(2, top), 0);
        assert_eq!(XMemFreePhysicalPages(1, std::ptr::null()), 0);
        // Allocation writes its page numbers to such an array as little.
        let mut n = 2;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, top.cast_mut()), 0);
        assert_eq!(mine(a, 2), before);
        assert!(known());
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        assert!(!known());
    }

    #[test]
    fn wine_detectors_agree() {
        // Ours and the one the network tests use must name the same host:
        // each switches a Wine-only branch.
        let by_export = crate::host::function("ntdll.dll", "wine_get_version").is_some();
        assert_eq!(on_wine(), by_export);
    }

    #[test]
    fn a_request_the_host_cannot_meet_fails_and_is_reported() {
        // Every count the host itself would refuse is refused before it is
        // asked (`a_page_count_past_the_page_numbers_is_refused`), so the
        // host's refusal is a section that cannot be made: error 8 here.
        let n = 12345;
        let (mut count, mut pages) = (n, [7usize; 1]);
        let asked = std::cell::Cell::new(0);
        let given = allocate_pages(&PHYSICAL, 0, &mut count, pages.as_mut_ptr(), |size| {
            asked.set(size);
            Err(std::io::Error::from_raw_os_error(8))
        });
        assert_eq!(given, 0);
        assert_eq!(asked.get(), n * PAGE, "the host was asked for the bytes");
        assert_eq!(count, 0, "pages given");
        assert_eq!(pages, [7], "no page number written");
        if on_wine() {
            // The Wine-only path: the section is where memory is asked
            // for, so its failure is what the crash log explains.
            let bytes = format!("{} bytes of physical pages", n * PAGE);
            let said = reported(&bytes);
            assert_eq!(said.len(), 1, "{said:?}");
            assert!(said[0].starts_with("VIctory: Wine could not create a section for"));
            assert!(said[0].contains("os error 8"), "{said:?}");
            assert!(said[0].contains("free memory and disk space"), "{said:?}");
        } else {
            // The failure itself was checked above; elsewhere memory is
            // asked for at each commit, which has its own report.
            crate::testing::skip("not Wine: only there is a refused section reported");
        }
        // A count that is no size at all is no failure of the host's:
        // nothing is asked of it and nothing reported.
        let (mut count, mut pages) = (WRAPS, [7usize; 1]);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
            0
        );
        assert_eq!((count, pages), (WRAPS, [7]));
    }

    #[test]
    fn a_page_count_past_the_page_numbers_is_refused() {
        // What a title keeps of a page number: 21 bits of `number >> 4`.
        let kept = |number: usize| (number >> 4 & 0x1f_ffff) << 4;
        assert_eq!(kept(NUMBER_END - FRAMES), NUMBER_END - FRAMES);
        assert_ne!(
            kept(NUMBER_END),
            NUMBER_END,
            "the first number that is lost"
        );
        assert_eq!(FIRST_NUMBER + MAX_PAGES * FRAMES, NUMBER_END);

        // One page more than there are numbers for, with room for every
        // number a call that went on would write.
        let n = MAX_PAGES + 1;
        let (mut count, mut numbers) = (n, vec![7usize; n]);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, numbers.as_mut_ptr()),
            0
        );
        assert_eq!(count, 0, "pages given");
        assert!(
            numbers.iter().all(|&number| number == 7),
            "no number written"
        );
        drop(numbers);
        // Counts the host was left to refuse before (Wine made a section of
        // 2^48 bytes when measured): not one reaches it now, so nothing is
        // allocated and nothing reported.
        for n in [1 << 21, 1 << 32, 1 << 40, usize::MAX / PAGE] {
            let (mut count, mut pages) = (n, [7usize; 1]);
            let asked = std::cell::Cell::new(false);
            let given = allocate_pages(&PHYSICAL, 0, &mut count, pages.as_mut_ptr(), |size| {
                asked.set(true);
                create_section(size)
            });
            assert_eq!(given, 0, "{n:#x}");
            assert!(!asked.get(), "{n:#x}: the host was asked for a section");
            assert_eq!((count, pages), (0, [7]), "{n:#x}");
            let mut count = n;
            assert_eq!(
                XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
                0,
                "{n:#x}"
            );
            assert_eq!((count, pages), (0, [7]), "{n:#x}");
            let bytes = format!(" {} bytes of physical pages", n * PAGE);
            assert_eq!(reported(&bytes), Vec::<String>::new(), "{n:#x}");
        }
    }

    #[test]
    fn page_numbers_run_out_cleanly() {
        let section = Arc::new(create_section(PAGE).expect("a section of one page"));
        // A page table of its own, two numbers from the end.
        let mut table = Physical {
            next_page: NUMBER_END - 2 * FRAMES,
            ..Default::default()
        };
        let state = |table: &Physical| (table.next_page, table.pages.len());
        let before = state(&table);
        // Too many by one, and counts whose frames wrap: nothing changes.
        for n in [3, MAX_PAGES + 1, usize::MAX / FRAMES + 1, usize::MAX] {
            assert_eq!(table.number(&section, n), None, "{n:#x}");
            assert_eq!(state(&table), before, "{n:#x}");
        }
        let first = table.number(&section, 2).expect("the last two numbers");
        assert_eq!(first, NUMBER_END - 2 * FRAMES);
        for (index, number) in [first, first + FRAMES].into_iter().enumerate() {
            assert_eq!((number >> 4 & 0x1f_ffff) << 4, number, "fits 21 bits");
            assert_eq!(table.pages.get(&number).map(|s| s.index), Some(index));
        }
        assert_eq!(state(&table), (NUMBER_END, 2));
        // None is left. A request for no pages takes none (the export
        // refuses it before it gets here).
        assert_eq!(table.number(&section, 1), None);
        assert_eq!(table.number(&section, 0), Some(NUMBER_END));
        assert_eq!(state(&table), (NUMBER_END, 2));

        // Through the export the same refusal reads: no page given.
        let n = 4;
        let (mut count, mut pages) = (n, [7usize; 4]);
        let full = Mutex::new(table);
        let given = allocate_pages(&full, 0, &mut count, pages.as_mut_ptr(), create_section);
        assert_eq!((given, count, pages), (0, 0, [7; 4]));
    }

    #[test]
    fn section_refusal_report_names_request_error_and_commit() {
        let error = std::io::Error::from_raw_os_error(8);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = section_refused_message(6 << 30, &error, Some(&info));
        assert!(report.contains("6442450944 bytes of physical pages"));
        assert!(report.contains("os error 8"));
        assert!(report.contains("System commit: 2 MiB of 3 MiB in use."));
        assert!(!section_refused_message(PAGE, &error, None).contains("System commit:"));
        // Figures that do not multiply still make a report.
        let absurd = PERFORMANCE_INFORMATION {
            CommitTotal: usize::MAX,
            CommitLimit: usize::MAX,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(PAGE, "physical pages", &error, Some(&absurd));
        assert!(report.contains("System commit: 17592186044415 MiB of"));
    }

    #[test]
    fn a_large_section_stays_sparse() {
        if !on_wine() {
            // Elsewhere the section is reserved and each view committed:
            // `physical_pages_map_alias_and_unmap` covers that path.
            crate::testing::skip("not Wine: only there is a section committed whole");
            return;
        }
        let resident = || {
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            // SAFETY: our own process and a sized structure.
            let ok =
                unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
            assert_ne!(ok, 0);
            counters.WorkingSetSize
        };
        const GIB: usize = 1 << 30;
        let n = GIB / PAGE;
        let before = resident();
        // Committed whole at creation there, with no commit per view.
        let pages = allocate(n);
        let a = mapped(n, &pages);
        // SAFETY: the whole gibibyte is mapped read-write.
        unsafe {
            a.cast::<u8>().write(0x11);
            a.cast::<u8>().add(GIB - 1).write(0x22);
        }
        assert_eq!(page_bytes(pages[0])[0], 0x11);
        assert_eq!(page_bytes(pages[n - 1])[PAGE - 1], 0x22);
        let grown = resident().saturating_sub(before);
        // Other tests allocate meanwhile; a section that was filled when
        // committed would show as the whole gibibyte.
        eprintln!("a 1 GiB section, two pages written: {grown} bytes more resident");
        assert!(grown < GIB / 4, "{grown} bytes more resident");
        assert_eq!(XMemMapPhysicalPages(a, n, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}
