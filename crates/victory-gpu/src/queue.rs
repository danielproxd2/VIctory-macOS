//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Queues the lists' host lists behind what the queue holds, and runs what
/// may run (`crate::hold`; decision 0007).
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() {
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        let mut held = held();
        for &l in lists {
            for segment in crate::list::segments(l) {
                let waits = segment.waits();
                held.push(this as usize, Item::Segment(segment, waits));
            }
        }
        account().submissions += 1;
        pump(&mut held, Why::Submission);
        return;
    }
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// What ran held work: the title's submission, a queue's Signal or Wait, a
/// CPU fence call, PresentX, or our timer.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Why {
    Submission,
    Signal,
    Wait,
    Fence,
    Present,
    Timer,
}

#[cfg(windows)]
use crate::hold::Item;

/// Every queue's held work.
#[cfg(windows)]
type Holding = crate::hold::Held<std::sync::Arc<crate::host::HostList>>;

#[cfg(windows)]
static HELD: std::sync::Mutex<Holding> = std::sync::Mutex::new(crate::hold::Held::new());

#[cfg(windows)]
fn held() -> std::sync::MutexGuard<'static, Holding> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs held work that may run now (`why` for the frame line).
pub(crate) fn pump_held(why: Why) {
    #[cfg(windows)]
    pump(&mut held(), why);
    #[cfg(not(windows))]
    let _ = why;
}

/// The same unless another thread is at it (calls that poll: a fence's
/// value, our timer).
pub(crate) fn pump_held_if_free(why: Why) {
    #[cfg(windows)]
    if let Ok(mut held) = HELD.try_lock() {
        pump(&mut held, why);
    }
    #[cfg(not(windows))]
    let _ = why;
}

/// How long a queue's head may wait before it runs anyway.
// shortcut: a safety valve against a wait nothing satisfies (a write we do
// not make), which would hold the title's fences until its 30 s watchdog
// ends the run; counted and logged, remove once runs show it never fires.
#[cfg(windows)]
const VALVE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(windows)]
fn pump(held: &mut Holding, why: Why) {
    if held.is_empty() {
        return;
    }
    let Some(host) = crate::host::host() else { return };
    let read = read_value;
    for (queue, waits) in held.force_older_than(VALVE) {
        account().forced += 1;
        tracing::warn!(queue = kind(queue), ?waits, "held work runs without its waits after {VALVE:?} (safety valve)");
    }
    let fence = |f: usize| crate::fence::value(f as This);
    let started = std::time::Instant::now();
    let mut ran = std::time::Duration::ZERO;
    let mut run = |_: usize, segments: Vec<std::sync::Arc<crate::host::HostList>>| {
        let lists: Vec<&crate::host::HostList> = segments.iter().map(|s| &**s).collect();
        let at = std::time::Instant::now();
        host.execute(&lists);
        ran += at.elapsed();
        drop(lists);
        segments.into_iter().for_each(crate::list::recycle);
    };
    let mut signal = |f: usize, value: u64| {
        crate::fence::set(f as This, value);
        crate::fence::let_go(f as This);
    };
    let pumped = held.pump(&mut crate::hold::World {
        read: &read,
        fence: &fence,
        run: &mut run,
        signal: &mut signal,
    });
    if pumped.segments > 0 || pumped.unknown > 0 {
        let mut a = account();
        a.pump += started.elapsed().saturating_sub(ran);
        a.pieces += pumped.pieces;
        a.segments += pumped.segments;
        a.unknown += pumped.unknown;
        for (queue, waited) in pumped.waited {
            a.released.push((kind(queue), waited, why));
        }
    }
    // Once work stays held, a timer tests it every 2 ms: memory the title
    // writes from the CPU, or the valve, may let it go with no call of ours.
    if held.blocked(&read).is_empty() {
        return;
    }
    static TIMER: std::sync::Once = std::sync::Once::new();
    TIMER.call_once(|| {
        let _ = std::thread::Builder::new().name("victory held work".into()).spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(2));
            pump_held_if_free(Why::Timer);
        });
    });
}

/// The value a wait tests (4 or 8 bytes), if the memory can be read.
#[cfg(windows)]
fn read_value(address: u64, bytes: u8) -> Option<u64> {
    let mut value = [0u8; 8];
    crate::host::read_into(address as *const u8, &mut value[..usize::from(bytes.min(8))]).then(|| u64::from_le_bytes(value))
}

/// A queue's type, for logs (0 graphics, 2 compute, 3 copy). Titles do not
/// release queues that hold work.
#[cfg(windows)]
fn kind(queue: usize) -> u32 {
    self::queue(queue as This).kind
}

/// What queues did with held work since the last presented frame.
#[cfg(windows)]
#[derive(Default)]
struct Account {
    submissions: u32,
    pieces: u32,
    segments: u32,
    /// Segments that waited: queue type, how long, what ran them.
    released: Vec<(u32, std::time::Duration, Why)>,
    unknown: u32,
    forced: u32,
    mappings: u32,
    /// Time in the pump outside host submissions.
    pump: std::time::Duration,
}

#[cfg(windows)]
fn account() -> std::sync::MutexGuard<'static, Account> {
    static ACCOUNT: std::sync::OnceLock<std::sync::Mutex<Account>> = std::sync::OnceLock::new();
    ACCOUNT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// The frame line of held work: what waited, how long and what released it,
/// and what is still held at the present, with the waits that do not hold.
#[cfg(windows)]
fn account_frame(frame: u64) {
    let (cuts, early) = crate::list::take_cuts();
    let (fence_waits, fence_waited) = crate::fence::take_waited();
    let a = std::mem::take(&mut *account());
    let blocked = held().blocked(&read_value);
    if a.released.is_empty() && blocked.is_empty() && a.forced == 0 && a.unknown == 0 && a.mappings == 0 {
        return;
    }
    let longest = a.released.iter().map(|r| r.1).max().unwrap_or_default();
    let mut by = [0u32; 6];
    for r in &a.released {
        by[r.2 as usize] += 1;
    }
    tracing::info!(
        frame,
        submissions = a.submissions,
        pieces = a.pieces,
        segments = a.segments,
        cuts,
        early_resets = early,
        waited = a.released.len(),
        longest_ms = longest.as_secs_f64() * 1e3,
        pump_ms = a.pump.as_secs_f64() * 1e3,
        fence_waits,
        fence_wait_ms = fence_waited.as_secs_f64() * 1e3,
        "held work this frame (submissions; host submissions, segments; lists cut at waits; lists reset or released \
         while held, each keeping its own host list; segments that waited, released by \
         submission/signal/wait/fence/present/timer {by:?}; time in the pump outside host submissions; events the          title waited on for fence values and their summed waits); held at the present: {}",
        blocked.len()
    );
    tracing::debug!(
        frame,
        "held work released this frame (queue type, ms, by): {:?}",
        a.released.iter().map(|(k, d, w)| (k, d.as_secs_f64() * 1e3, w)).collect::<Vec<_>>()
    );
    for (queue, items, waited, unmet) in &blocked {
        let level_warn = blocked.len() > 1;
        let unmet: Vec<String> = unmet
            .iter()
            .map(|w| format!("{:#x} cmp {} {:#x} (is {:?})", w.address, w.comparison, w.reference, read_value(w.address, w.bytes)))
            .collect();
        if level_warn {
            tracing::warn!(frame, queue = kind(*queue), items, waited_ms = waited.as_millis(), ?unmet, "two or more queues hold work at the present");
        } else {
            tracing::info!(frame, queue = kind(*queue), items, waited_ms = waited.as_millis(), ?unmet, "a queue holds work at the present");
        }
    }
    if a.forced > 0 || a.unknown > 0 || a.mappings > 0 {
        tracing::warn!(
            frame,
            forced = a.forced,
            unknown = a.unknown,
            mappings = a.mappings,
            "held work: run without its waits (valve), waits with an unknown comparison (taken as holding), \
             page mappings done ahead of held work"
        );
    }
}

/// The start of a present plane: frame token, then the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they run, so it signals as Signal does.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
/// At once, or behind the work the queue holds (its fence, and the memory
/// the title reads it at, move only once that work has run).
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    {
        let mut held = held();
        if held.holds_work(this as usize) {
            crate::fence::keep(fence);
            held.push(this as usize, Item::Signal(fence as usize, value));
        } else {
            crate::fence::set(fence, value);
        }
        // Another queue may wait for this fence.
        pump(&mut held, Why::Signal);
    }
    #[cfg(not(windows))]
    {
        let _ = this;
        crate::fence::set(fence, value);
    }
    S_OK
}
/// The queue's later work waits until the fence reaches `value`.
///
/// shortcut: the fence is not kept alive while the queue waits for it;
/// Grand Theft Auto VI makes no queue wait for a fence.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() && crate::fence::value(fence) < value {
        let mut held = held();
        held.push(this as usize, Item::FenceWait(fence as usize, value));
        pump(&mut held, Why::Wait);
    }
    #[cfg(not(windows))]
    let _ = (this, value);
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
extern "system" fn present(
    _: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    // What may run before the frame is shown does.
    // shortcut: PresentX does not wait behind work the queue still holds (it
    // shows the frame as far as it ran); the frame line names such work.
    pump_held(Why::Present);
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    #[cfg(windows)]
    account_frame(frame);
    #[cfg(feature = "diagnostics")]
    crate::frame::presented();
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        crate::capture::presented(frame);
        if crate::capture::listed(frame) || crate::capture::has_targets() {
            crate::capture::dump_targets(frame);
        }
    }
    #[cfg(windows)]
    if planes > 0
        // SAFETY: the title passes `planes` plane descriptions.
        && let Some(plane) = unsafe { plane_parameters.as_ref() }
        && plane.count > 0
        && !plane.resources.is_null()
        // SAFETY: the plane lists `count` resources.
        && let resource = unsafe { *plane.resources }
        && let Some(texture) = crate::resource::host_resource(resource)
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        let shown = host.present(&texture, state, frame);
        crate::resource::host_transition(resource, state);
        #[cfg(feature = "diagnostics")]
        blinks(frame, resource, state, shown);
        #[cfg(not(feature = "diagnostics"))]
        let _ = shown;
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    S_OK
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// Maps what `batches` say through `map` (`(to, from, len)`, `from` 0
/// unmaps; false on failure): whether every range was mapped. A range of
/// pool pages that is not inside the pool is not mapped.
///
/// `Err` if an address of any range does not fit in 64 bits (the title's
/// numbers are not trusted); nothing is mapped then, also not the ranges
/// before the one that wraps.
fn map_batches(
    map: &mut dyn FnMut(u64, u64, u64) -> bool,
    batches: &[PageBatch],
    pool: u64,
    pool_pages: u32,
) -> Result<bool, ()> {
    walk_batches(None, batches, pool, pool_pages)?;
    walk_batches(Some(map), batches, pool, pool_pages)
}

/// [`map_batches`], or with no `map` only its arithmetic.
fn walk_batches(
    mut map: Option<&mut dyn FnMut(u64, u64, u64) -> bool>,
    batches: &[PageBatch],
    pool: u64,
    pool_pages: u32,
) -> Result<bool, ()> {
    let dry = map.is_none();
    let mut call = |to: u64, from: u64, len: u64| match map.as_mut() {
        Some(map) => map(to, from, len),
        None => true,
    };
    let mut ok = true;
    for b in batches {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            // 32 bits of pages of 64 KiB: this cannot overflow.
            let len = u64::from(r.count) * PAGE;
            let end = at.checked_add(len).ok_or(())?;
            // Where `pages` pool pages from the range's first are, if they
            // are inside the pool.
            let source = |pages: u32| -> Result<Option<u64>, ()> {
                if r.start.checked_add(pages).is_none_or(|e| e > pool_pages) {
                    return Ok(None);
                }
                let source = pool.checked_add(u64::from(r.start) * PAGE).ok_or(())?;
                source.checked_add(u64::from(pages) * PAGE).ok_or(())?;
                Ok(Some(source))
            };
            ok &= match r.kind {
                0 => match source(r.count)? {
                    Some(source) => call(at, source, len),
                    None => false,
                },
                1 => match source(1)? {
                    // Below `end`, so these sums do not overflow. The
                    // pass without a mapper has nothing to do per page.
                    Some(source) if !dry => {
                        (0..u64::from(r.count)).all(|k| call(at + k * PAGE, source, PAGE))
                    }
                    Some(_) => true,
                    None => false,
                },
                2 => call(at, 0, len),
                3 => true,
                _ => false,
            };
            at = end;
        }
    }
    Ok(ok)
}

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. Grand Theft Auto VI streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done at once.
///
/// shortcut: done ahead of work the queue still holds (counted in the frame
/// line); queue it as an item like Signal if that count is not 0.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    // Upstream counts here, after its refusal of a null batch list
    // (`copy_mappings` refuses that one).
    #[cfg(windows)]
    if !(batches.is_null() && count > 0) && held().holds_work(this as usize) {
        account().mappings += 1;
    }
    #[cfg(not(windows))]
    let _ = this;
    copy_mappings(
        &mut |to, from, len| map(to, from, len),
        count,
        batches,
        pool,
        pool_pages,
    )
}

/// [`copy_page_mappings`] with the mapper `map`.
///
/// A mapping the mapper refuses is logged and the call still succeeds.
/// shortcut: what the console returns then is not known; titles seen do
/// not check for it. Batches whose addresses wrap are an argument error.
fn copy_mappings(
    map: &mut dyn FnMut(u64, u64, u64) -> bool,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
) -> HRESULT {
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let batches = if count > 0 {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    } else {
        &[]
    };
    let Ok(ok) = map_batches(map, batches, pool, pool_pages) else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                pool = format_args!("{pool:#x}"),
                "a page mapping batch wraps around the address space; refused"
            )
        });
        return E_INVALIDARG;
    };
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed")
        });
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from Grand Theft Auto VI's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
pub(crate) const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

/// Whether `this` queue has the newer layout, by its own table.
#[cfg(test)]
pub(crate) fn is_newer(this: This) -> bool {
    // SAFETY: one of our queues; its first field is the table.
    let table = unsafe { *this.cast::<*const *const ()>() };
    table == NEWER_FUNCTIONS.0.as_ptr()
}

/// A bare object with the older or the newer queue table, and where
/// Signal is, for the tests of slot lookup.
#[cfg(test)]
pub(crate) fn test_layout(newer: bool) -> (Object, *const ()) {
    let object = if newer {
        Object::new(&NEWER_FUNCTIONS)
    } else {
        Object::new(&FUNCTIONS)
    };
    (object, signal as *const ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        // Signal: newer layouts have it two slots later.
        type Signal = extern "system" fn(This, This, u64) -> HRESULT;
        let signal = crate::testing::queue_slot::<Signal>(q, 0x70);
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// Grand Theft Auto VI's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }

    type Call = (u64, u64, u64);

    fn range(kind: u32, start: u32, count: u32) -> PageRange {
        PageRange { kind, start, count }
    }

    /// What the mapper is asked for `ranges` from `destination` on, whether
    /// everything was mapped (`Err`: the addresses wrap), and what the
    /// queue's method returns for the same batch with a mapper that
    /// answers `mapper`.
    fn mapped(
        destination: u64,
        ranges: &[PageRange],
        pool: u64,
        pool_pages: u32,
        mapper: bool,
    ) -> (Vec<Call>, Result<bool, ()>, HRESULT) {
        let batch = [PageBatch {
            destination,
            count: ranges.len() as u32,
            ranges: ranges.as_ptr(),
        }];
        let mut calls = Vec::new();
        let mut record = |to, from, len| {
            // A test that would record a call per page of a huge range
            // stops here instead of filling memory.
            assert!(calls.len() < 1 << 16, "too many mapper calls");
            calls.push((to, from, len));
            mapper
        };
        let all = map_batches(&mut record, &batch, pool, pool_pages);
        let mut again = Vec::new();
        let hr = copy_mappings(
            &mut |to, from, len| {
                assert!(again.len() < 1 << 16, "too many mapper calls");
                again.push((to, from, len));
                mapper
            },
            1,
            batch.as_ptr(),
            pool,
            pool_pages,
        );
        if mapper {
            assert_eq!(again, calls, "the method maps what the batches say");
        }
        (calls, all, hr)
    }

    #[test]
    fn page_mapping_batches_reach_the_mapper_in_order() {
        const POOL: u64 = 0x40_0000_0000;
        const TO: u64 = 0x80_0000_0000;
        let ranges = [
            range(0, 2, 3), // three pool pages from the pool's third
            range(1, 5, 2), // the pool's sixth page, twice
            range(2, 9, 2), // two pages unmapped (the start means nothing)
            range(3, 0, 4), // four pages left alone
            range(0, 0, 1),
        ];
        let (calls, all, hr) = mapped(TO, &ranges, POOL, 8, true);
        assert_eq!(
            calls,
            [
                (TO, POOL + 2 * PAGE, 3 * PAGE),
                (TO + 3 * PAGE, POOL + 5 * PAGE, PAGE),
                (TO + 4 * PAGE, POOL + 5 * PAGE, PAGE),
                (TO + 5 * PAGE, 0, 2 * PAGE),
                (TO + 11 * PAGE, POOL, PAGE),
            ]
        );
        assert_eq!((all, hr), (Ok(true), S_OK));

        // Pool pages outside the pool are not mapped, and neither is an
        // unknown kind; what follows them still is, at its own place.
        let ranges = [
            range(0, 6, 3),
            range(1, 8, 1),
            range(0, u32::MAX, 2),
            range(7, 0, 1),
            range(2, 0, 1),
        ];
        let (calls, all, hr) = mapped(TO, &ranges, POOL, 8, true);
        assert_eq!(calls, [(TO + 7 * PAGE, 0, PAGE)]);
        // The call succeeds all the same (as it did before the batches
        // were checked): what the console returns is not known.
        assert_eq!((all, hr), (Ok(false), S_OK));
    }

    /// A mapper that fails is logged, not reported: existing behaviour,
    /// kept until the console's is known.
    #[test]
    fn a_failed_page_mapping_is_not_an_error() {
        let ranges = [range(1, 0, 3), range(2, 0, 1)];
        let (calls, all, hr) = mapped(0x1_0000_0000, &ranges, 0x2_0000_0000, 1, false);
        // The pages of one range stop at the first that fails.
        assert_eq!(
            calls,
            [
                (0x1_0000_0000, 0x2_0000_0000, PAGE),
                (0x1_0000_0000 + 3 * PAGE, 0, PAGE)
            ]
        );
        assert_eq!((all, hr), (Ok(false), S_OK));
    }

    /// Addresses that do not fit in 64 bits are an argument error, and
    /// nothing is mapped: not at a wrapped address, and not the ranges
    /// before either.
    #[test]
    fn page_mapping_batches_never_wrap() {
        const POOL: u64 = 0x40_0000_0000;
        let refused = (Vec::new(), Err(()), E_INVALIDARG);
        // The destination's second page would start at 0.
        let top = u64::MAX - PAGE;
        assert_eq!(mapped(top, &[range(2, 0, 2)], POOL, 8, true), refused);
        assert_eq!(mapped(top, &[range(0, 0, 2)], POOL, 8, true), refused);
        assert_eq!(mapped(top, &[range(3, 0, 2)], POOL, 8, true), refused);
        // One page per page, as many as 32 bits hold, near the top.
        let many = [range(1, 0, u32::MAX)];
        assert_eq!(mapped(u64::MAX << 40, &many, POOL, 8, true), refused);
        // A good range first: it is not mapped either.
        let ranges = [range(2, 0, 1), range(2, 0, 2)];
        assert_eq!(mapped(top - PAGE, &ranges, POOL, 8, true), refused);
        // The pool's pages would wrap.
        let ranges = [range(0, 0, 1)];
        assert_eq!(mapped(0x1000, &ranges, u64::MAX - 1, 8, true), refused);
        let ranges = [range(1, 7, 1)];
        assert_eq!(
            mapped(0x1000, &ranges, u64::MAX - 7 * PAGE, 8, true),
            refused
        );
        // Up to the last byte is fine.
        let (calls, all, hr) = mapped(top - 1, &[range(2, 0, 1)], POOL, 8, true);
        assert_eq!(calls, [(top - 1, 0, PAGE)]);
        assert_eq!((all, hr), (Ok(true), S_OK));
    }

    /// Without the runtime's page mapper (nothing in this test binary
    /// names one) the queue's method refuses every call.
    #[test]
    fn page_mappings_without_a_mapper_are_refused() {
        assert!(crate::resource::page_mapper().is_none());
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        type Copy = extern "system" fn(This, u32, *const PageBatch, u64, u32, u32) -> HRESULT;
        let copy = crate::testing::queue_slot::<Copy>(q, 0xe8);
        let ranges = [range(2, 0, 1)];
        let batch = PageBatch {
            destination: 0x1000_0000,
            count: 1,
            ranges: ranges.as_ptr(),
        };
        assert_eq!(copy(q, 1, &batch, 0x2000_0000, 8, 0), E_INVALIDARG);
        assert_eq!(copy(q, 0, std::ptr::null(), 0, 0, 0), E_INVALIDARG);
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// No batches where some are counted is an argument error, whatever
    /// the mapper; no batches at all is nothing to do.
    #[test]
    fn page_mappings_need_their_batches() {
        let mut calls = 0;
        let mut map = |_, _, _| {
            calls += 1;
            true
        };
        assert_eq!(
            copy_mappings(&mut map, 1, std::ptr::null(), 0, 0),
            E_INVALIDARG
        );
        assert_eq!(copy_mappings(&mut map, 0, std::ptr::null(), 0, 0), S_OK);
        // A batch without ranges is passed over.
        let batch = PageBatch {
            destination: 0,
            count: 5,
            ranges: std::ptr::null(),
        };
        assert_eq!(copy_mappings(&mut map, 1, &batch, 0, 0), S_OK);
        assert_eq!(calls, 0);
    }
}
