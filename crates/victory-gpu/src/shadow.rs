//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: write
//! watching, or read-only guards on physical-page views), all of it where
//! neither works. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! shortcut: only the ranges draws are known to read are brought up to
//! date (vertex and index buffer views, constant buffers in tables, root
//! descriptors, buffer views in tables); buffers read through directly
//! indexed heaps keep what their first use copied.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// Per page of title memory from `start`'s on: the write-watch asking
    /// after which it was last copied (0: never).
    copied: Mutex<Vec<u64>>,
    /// Whether the memory is write-watched: unknown, yes, no.
    watched: AtomicU8,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<Vec<(usize, usize)>>,
    /// The copy's bytes before the submission ran, from an offset on,
    /// covering the ranges its shaders write: what the GPU wrote differs.
    before: Mutex<Option<(usize, Vec<u8>)>>,
}

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool (at most 8 kept, the largest).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    if pool.len() > 8 {
        pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
        pool.truncate(8);
    }
}

const NOT_WATCHED: u8 = 2;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The buffer's end (which [`register`] and [`loose`] have checked to
    /// fit in 64 bits).
    fn end(&self) -> u64 {
        self.start + self.len
    }

    /// The host GPU address of a title address inside the buffer (its end
    /// included); 0, the null address, for one outside it. Callers pass
    /// addresses they found the copy with; a title's address is not
    /// trusted to stay there through their arithmetic.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        if address < self.start || address > self.end() {
            return 0;
        }
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end: none past the end,
    /// and for an address before the buffer, as many as there are from the
    /// buffer's start.
    pub(crate) fn left(&self, address: u64) -> u64 {
        self.end().saturating_sub(address).min(self.len)
    }

    /// Offset of a title address from the buffer's start, within the
    /// buffer: 0 for an address before it, its length for one past it.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address.saturating_sub(self.start).min(self.len)
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let (address, len) = self.cut(address, len);
        list.defer_refresh(self, address as usize, len as usize);
    }

    /// `[address, address + len)` cut to the buffer: where it starts in
    /// title memory and how long it is (nothing if it lies outside).
    fn cut(&self, address: u64, len: u64) -> (u64, u64) {
        let from = address.clamp(self.start, self.end());
        let to = address.saturating_add(len).clamp(from, self.end());
        (from, to - from)
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Shadow {
        let pages = (start + len).div_ceil(PAGE as u64) - start / PAGE as u64;
        Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            copied: Mutex::new(vec![0; pages as usize]),
            watched: AtomicU8::new(0),
            pending: Mutex::default(),
            before: Mutex::default(),
        }
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (
            from - start,
            address.saturating_add(len).clamp(from, end) - start,
        )
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let (address, len) = self.cut(address, len);
        list.defer_gpu_write(self, address as usize, len as usize);
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// one snapshot per copy and submission, grown to cover every range the
    /// submission declares (the world declares overlapping views of its
    /// 174 MB heap dozens of times), in reused memory (fresh snapshots cost
    /// ~2.3 million page faults per world frame).
    pub(crate) fn writing(&self, address: usize, len: usize) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((address, address.saturating_add(len)));
        let (from, to) = self.inside(address, len);
        // SAFETY: inside the mapping.
        let host =
            |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = match &*before {
            Some((at, bytes)) => ((*at).min(from), (at + bytes.len()).max(to)),
            None => (from, to),
        };
        if let Some((at, bytes)) = &*before
            && (*at, at + bytes.len()) == (start, end)
        {
            return;
        }
        // Parts newly covered from the copy, the rest as it was taken.
        let mut grown = pooled(end - start);
        match before.take() {
            Some((at, bytes)) => {
                grown.extend_from_slice(host(start, at));
                grown.extend_from_slice(&bytes);
                grown.extend_from_slice(host(at + bytes.len(), end));
                give_back(bytes);
            }
            None => grown.extend_from_slice(host(start, end)),
        }
        *before = Some((start, grown));
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = &*snapshot
            && *taken <= from
            && to <= taken + before.len()
            && from < to
        {
            let before = &before[from - taken..to - taken];
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            let mut title = vec![0u8; PIECE];
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let title = &mut title[..n];
                if host != old && crate::host::read_into((start + at) as *const u8, title) {
                    // Title memory with the GPU's bytes put in; the copy
                    // takes the title's bytes in turn.
                    let mut changed = false;
                    for ((t, h), o) in title.iter_mut().zip(host.iter_mut()).zip(old) {
                        if *h != *o && *t != *h {
                            *t = *h;
                            changed = true;
                        }
                        *h = *t;
                    }
                    if changed {
                        crate::watch::writing(start + at, n);
                        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                        written += n;
                    }
                    synced.push((start + at) / PAGE);
                }
                at += n;
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if first < last
                && !synced.is_empty()
                && self.watched.load(Ordering::Relaxed) != NOT_WATCHED
                && let Some((now, _)) = crate::watch::writes(first * PAGE, last * PAGE, true)
            {
                let base = start / PAGE;
                let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
                for page in synced.into_iter().filter(|p| (first..last).contains(p)) {
                    copied[page - base] = now;
                }
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|&r| r != (address, address.saturating_add(len)));
        if pending.is_empty()
            && let Some((_, bytes)) = snapshot.take()
        {
            give_back(bytes);
        }
        written
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), address.saturating_add(len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if !busy.is_empty() {
            // Around ranges shaders are writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame).
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                if a > at {
                    bytes += self.update_free(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_free(at, to);
            }
            return bytes;
        }
        self.update_free(from, to)
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes.
    fn update_free(&self, from: usize, to: usize) -> usize {
        if self.watched.load(Ordering::Relaxed) != NOT_WATCHED {
            if let Some((now, written)) = crate::watch::writes(from, to, true) {
                self.watched.store(1, Ordering::Relaxed);
                let copied = self.copy_stale(from, to, now, &written);
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Checked, (to - from) as u64);
                    add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
                }
                return copied;
            }
            self.watched.store(NOT_WATCHED, Ordering::Relaxed);
        }
        self.copy(from, to);
        to - from
    }

    /// Copies the pages of `[from, to)` written after their last copy
    /// (`written`: last writes by page, in page order; `now`: this
    /// asking), whole pages within the buffer.
    fn copy_stale(&self, from: usize, to: usize, now: u64, written: &[(usize, u64)]) -> usize {
        let base = self.start as usize / PAGE;
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let mut writes = written.iter().peekable();
        let (mut bytes, mut run) = (0, None);
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            let at = &mut copied[page - base];
            if *at == 0 || write > *at {
                // A page only partly in the range is copied in part: it
                // stays stale for the rest.
                let (start, end) = (self.start as usize, (self.start + self.len) as usize);
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    *at = now;
                }
                run.get_or_insert(page);
            } else if let Some(first) = run.take() {
                bytes += self.copy((first * PAGE).max(from), (page * PAGE).min(to));
            }
        }
        // Cut to the range: next to it shaders may be writing the copy.
        if let Some(first) = run {
            bytes += self.copy((first * PAGE).max(from), ((last + 1) * PAGE).min(to));
        }
        bytes
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        to - from
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
#[derive(Default)]
struct Buffers {
    map: BTreeMap<(u64, usize), Entry>,
    /// The length of the longest buffer there has been: no buffer that
    /// holds an address starts further below it.
    longest: u64,
}

static BUFFERS: Mutex<Buffers> = Mutex::new(Buffers {
    map: BTreeMap::new(),
    longest: 0,
});

fn buffers() -> std::sync::MutexGuard<'static, Buffers> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer exists at `start` (`len` bytes). One that would pass the
/// end of the address space is not a buffer a title can have and is not
/// kept (its uses find no copy, or one of loose memory).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    let Some(end) = start.checked_add(len) else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                start = format_args!("{start:#x}"),
                len,
                "a title buffer wraps around the address space; not copied to the host"
            )
        });
        return;
    };
    let mut buffers = buffers();
    buffers.longest = buffers.longest.max(len);
    buffers
        .map
        .insert((start, resource), Entry { end, shadow: None });
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    buffers().map.remove(&(start, resource));
}

/// How many buffers starting below an address are looked at before a
/// longer search is worth a line in the log.
const NEAR: usize = 64;

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut buffers = buffers();
    // Downwards from the address, as far as a buffer holding the range can
    // start. (This used to stop after 64 buffers: a buffer with more than
    // that many others inside it, below the address, was not found, and
    // its range got a second copy as loose memory.)
    let lowest = end.saturating_sub(buffers.longest).min(address);
    let found = buffers
        .map
        .range((lowest, 0)..=(address, usize::MAX))
        .rev()
        .enumerate()
        .find(|(_, (_, e))| e.end >= end)
        .map(|(i, (k, _))| (i, *k));
    let Some((passed, key)) = found else {
        drop(buffers);
        return loose(address, end);
    };
    if passed >= NEAR {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                address = format_args!("{address:#x}"),
                passed,
                "a title buffer found past many others inside it"
            )
        });
    }
    let entry = buffers.map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.checked_next_multiple_of(1 << 16)?)?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Arc::new(Shadow::new(buffer, data, gpu, key.0, len)));
    }
    entry.shadow.clone()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// The copy of loose memory `[address, end)`: one there is that holds all
/// of it, else a new one of the windows it touches.
///
/// shortcut: a new copy may overlap older ones (a range across two windows
/// after ranges inside each): the same title memory then has two host
/// copies, each brought up to date where it is used, and what shaders write
/// into one is not seen through the other. Logged once.
fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    // A range in the last window of the address space has no copy.
    let len = end.checked_next_multiple_of(WINDOW)? - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    // Few copies, so all that start at or below the address are looked at.
    if let Some(shadow) = map
        .range(..=(address, u64::MAX))
        .rev()
        .map(|(_, s)| s)
        .find(|s| s.end() >= end)
    {
        return Some(shadow.clone());
    }
    if map
        .values()
        .any(|s| s.start < start + len && s.end() > start)
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                start = format_args!("{start:#x}"),
                len,
                "loose title memory gets a second, overlapping host copy"
            )
        });
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Arc::new(Shadow::new(buffer, data, gpu, start, len));
    map.insert((start, len), shadow.clone());
    Some(shadow)
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .map
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Title buffers of a test, taken out of the map when it ends (also
    /// when it fails: the memory behind them is the test's).
    struct Registered(Vec<(usize, u64)>);

    impl Registered {
        fn add(&mut self, resource: usize, start: u64, len: u64) {
            register(resource, start, len);
            self.0.push((resource, start));
        }
    }

    impl Drop for Registered {
        fn drop(&mut self) {
            for &(resource, start) in &self.0 {
                unregister(resource, start);
            }
        }
    }

    /// Title memory of `len` bytes, each byte other than zero and than
    /// [`MARK`].
    fn title(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 199) as u8 + 1).collect()
    }

    /// What the tests fill a host copy with before a copy into it.
    const MARK: u8 = 0xee;

    /// `len` bytes of the host copy from `offset` on.
    fn host_bytes(s: &Shadow, offset: usize, len: usize) -> &mut [u8] {
        // SAFETY: inside the mapping (the tests stay within the copy's
        // length rounded up to 64 KiB); one test at a time uses it.
        unsafe { std::slice::from_raw_parts_mut(s.data.add(offset), len) }
    }

    /// Resource names for the tests' buffers: any numbers, the address
    /// tells buffers of different tests apart.
    const OUTER: usize = 1;
    const INNER: usize = 2;

    #[test]
    fn the_innermost_buffer_is_found() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let memory = title(8192);
        let at = memory.as_ptr() as u64;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, at, 8192);
        registered.add(INNER, at + 1024, 512);
        let extent = |s: Arc<Shadow>| (s.start - at, s.len);

        let inner = find(at + 1040, 32).map(extent);
        assert_eq!(inner, Some((1024, 512)));
        assert_eq!(find(at + 1024, 512).map(extent), Some((1024, 512)));
        // Past the inner buffer's end, before its start, and outside it.
        assert_eq!(find(at + 1024, 513).map(extent), Some((0, 8192)));
        assert_eq!(find(at + 1023, 2).map(extent), Some((0, 8192)));
        assert_eq!(find(at + 4096, 4096).map(extent), Some((0, 8192)));
        // One copy per buffer, whichever range asks.
        let (a, b) = (find(at + 1040, 1).unwrap(), find(at + 1500, 36).unwrap());
        assert!(Arc::ptr_eq(&a, &b));
        // An empty range is its first byte.
        assert_eq!(find(at + 1535, 0).map(extent), Some((1024, 512)));
        assert_eq!(find(at + 1536, 0).map(extent), Some((0, 8192)));
    }

    /// A buffer with more than 64 others inside it below an address is
    /// still the one found for a range only it holds (the search used to
    /// give up after 64 and make a second copy, of loose memory).
    #[test]
    fn deeply_nested_buffers_still_resolve_to_the_outer_one() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let memory = title(8192);
        let at = memory.as_ptr() as u64;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, at, 8192);
        for i in 0..80u64 {
            registered.add(INNER + i as usize, at + 16 * i, 16);
        }
        let found = find(at + 80 * 16 + 8, 64).map(|s| (s.start, s.len));
        assert_eq!(found, Some((at, 8192)));
        // And the small ones are found as themselves.
        let found = find(at + 16 * 70 + 4, 8).map(|s| (s.start, s.len));
        assert_eq!(found, Some((at + 16 * 70, 16)));
    }

    #[test]
    fn refresh_copies_title_bytes_to_the_host() {
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let mut memory = title(8192);
        let at = memory.as_ptr() as u64;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, at, 8192);
        let s = find(at + 100, 200).unwrap();
        // The copy is 64 KiB (rounded up); the buffer is its first 8192.
        host_bytes(&s, 0, 16384).fill(MARK);

        let submit = |ranges: &[(u64, u64)]| {
            let list = host.new_list().unwrap();
            for &(address, len) in ranges {
                s.refresh(&list, address, len);
            }
            list.close();
            host.execute(&[&list]);
        };
        submit(&[(at + 100, 200)]);
        assert_eq!(host_bytes(&s, 100, 200), &memory[100..300]);
        assert!(host_bytes(&s, 0, 100).iter().all(|&b| b == MARK));
        assert!(host_bytes(&s, 300, 16084).iter().all(|&b| b == MARK));

        // What the title writes later arrives with the next submission
        // that reads it (plain memory is not write-watched: all of the
        // range is copied).
        memory[150] = 0;
        submit(&[(at + 100, 200)]);
        assert_eq!(host_bytes(&s, 150, 1)[0], 0);
        assert_eq!(host_bytes(&s, 100, 200), &memory[100..300]);

        // A range that passes the buffer's end is cut to it; one that
        // starts past the end, or wraps, copies nothing.
        submit(&[
            (at + 8000, 1000),
            (at + 9000, 16),
            (at + 8192, 64),
            (u64::MAX - 4, 16),
            (at + 8100, u64::MAX),
        ]);
        assert_eq!(host_bytes(&s, 8000, 192), &memory[8000..]);
        assert!(host_bytes(&s, 8192, 8192).iter().all(|&b| b == MARK));
        assert!(host_bytes(&s, 300, 7700).iter().all(|&b| b == MARK));
        // Before the buffer: cut to its start.
        submit(&[(at - 50, 60)]);
        assert_eq!(host_bytes(&s, 0, 10), &memory[..10]);
        assert!(host_bytes(&s, 10, 90).iter().all(|&b| b == MARK));
    }

    /// What shaders write into a range declared with `gpu_writes` is
    /// copied back into title memory after the submission, and only the
    /// bytes they changed. A host step of the list stands in for the
    /// shaders: it writes the host copy after the range was handed over.
    #[test]
    fn gpu_writes_come_back_to_title_memory() {
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let memory = title(8192);
        let original = memory.clone();
        let at = memory.as_ptr() as u64;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, at, 8192);
        let s = find(at, 8192).unwrap();
        host_bytes(&s, 0, 8192).fill(MARK);

        let list = host.new_list().unwrap();
        s.refresh(&list, at + 1000, 64);
        s.gpu_writes(&list, at + 1000, 64);
        let copy = s.clone();
        list.defer_host(move || {
            // Inside the declared range, and outside it (which is not the
            // title's business: it does not come back).
            host_bytes(&copy, 1010, 4).copy_from_slice(&[0xa1, 0xa2, 0xa3, 0xa4]);
            host_bytes(&copy, 2000, 1)[0] = 0xa5;
        });
        list.close();
        host.execute(&[&list]);

        let mut expected = original.clone();
        expected[1010..1014].copy_from_slice(&[0xa1, 0xa2, 0xa3, 0xa4]);
        // Read through a pointer: the write came from outside the borrow.
        // SAFETY: the vector's own memory.
        let now = unsafe { std::slice::from_raw_parts(at as *const u8, 8192) }.to_vec();
        assert!(
            now == expected,
            "title memory differs from what is expected"
        );
        assert_eq!(host_bytes(&s, 1000, 64), &expected[1000..1064]);
        // The range is free again: the snapshot and the claim are gone.
        assert!(s.pending.lock().unwrap().is_empty());
        assert!(s.before.lock().unwrap().is_none());

        // A declared range outside the copy claims nothing and writes
        // nothing back.
        let list = host.new_list().unwrap();
        s.gpu_writes(&list, at + 9000, 64);
        s.gpu_writes(&list, u64::MAX - 8, 64);
        list.close();
        host.execute(&[&list]);
        assert!(s.pending.lock().unwrap().is_empty());
        // SAFETY: as above.
        let now = unsafe { std::slice::from_raw_parts(at as *const u8, 8192) }.to_vec();
        assert!(now == expected);
        drop(memory);
    }

    /// A buffer or a range that would pass the end of the address space is
    /// not registered and has no copy: nothing is computed past 64 bits.
    #[test]
    fn ranges_near_the_top_of_memory_do_not_wrap() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, u64::MAX - 8, 64);
        assert!(!buffers().map.contains_key(&(u64::MAX - 8, OUTER)));
        assert!(find(u64::MAX - 4, 16).is_none());
        assert!(find(u64::MAX - 4, 2).is_none());
        assert!(find(u64::MAX, 0).is_none());
        assert!(find(u64::MAX - 100, 16).is_none());
        assert!(find(0x1000, u64::MAX).is_none());
        // Up to the last byte is a buffer like any other (never read here).
        registered.add(INNER, u64::MAX - 64, 64);
        let found = find(u64::MAX - 32, 32).map(|s| (s.start, s.len));
        assert_eq!(found, Some((u64::MAX - 64, 64)));
    }

    /// Loose memory (outside any buffer) is copied by 4 MiB window; a
    /// range inside a copy there already is uses that copy. The addresses
    /// are in no test's memory: nothing here reads them.
    #[test]
    fn loose_ranges_in_one_window_share_a_copy() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        const BASE: u64 = 0x7a00_0000_0000;
        let a = find(BASE + 0x100, 64).unwrap();
        let b = find(BASE + 0x20_0000, 4096).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!((a.start, a.len), (BASE, WINDOW));
        // The window's last byte is in it, the next window's first is not.
        let last = find(BASE + WINDOW - 1, 1).unwrap();
        assert!(Arc::ptr_eq(&a, &last));
        let next = find(BASE + WINDOW, 1).unwrap();
        assert_eq!((next.start, next.len), (BASE + WINDOW, WINDOW));

        // A range across two windows gets one copy of both, and ranges
        // inside either then use it.
        const FAR: u64 = BASE + 16 * WINDOW;
        let both = find(FAR + WINDOW - 8, 16).unwrap();
        assert_eq!((both.start, both.len), (FAR, 2 * WINDOW));
        let first = find(FAR + 64, 64).unwrap();
        let second = find(FAR + WINDOW + 64, 64).unwrap();
        assert!(Arc::ptr_eq(&both, &first));
        assert!(Arc::ptr_eq(&both, &second));
        // shortcut (as documented at `loose`): the other way round, the
        // range across gets a second copy that overlaps the first two.
        let across = find(BASE + WINDOW - 8, 16).unwrap();
        assert_eq!((across.start, across.len), (BASE, 2 * WINDOW));
        assert!(!Arc::ptr_eq(&across, &a));
    }

    #[test]
    fn addresses_outside_a_copy_do_not_underflow() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let memory = title(4096);
        let at = memory.as_ptr() as u64;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, at, 4096);
        let s = find(at, 4096).unwrap();
        let gpu = s.gpu(at);
        assert_ne!(gpu, 0);
        assert_eq!(s.gpu(at + 16), gpu + 16);
        assert_eq!(s.gpu(at + 4096), gpu + 4096, "the end is an address");
        assert_eq!(s.gpu(at - 1), 0);
        assert_eq!(s.gpu(at + 4097), 0);
        assert_eq!(s.gpu(0), 0);
        assert_eq!(s.gpu(u64::MAX), 0);

        assert_eq!((s.offset(at), s.offset(at + 100)), (0, 100));
        assert_eq!((s.offset(at - 1), s.offset(0)), (0, 0));
        assert_eq!(s.offset(u64::MAX), 4096);

        assert_eq!((s.left(at), s.left(at + 100)), (4096, 3996));
        assert_eq!((s.left(at + 4096), s.left(u64::MAX)), (0, 0));
        assert_eq!((s.left(at - 1), s.left(0)), (4096, 4096));

        assert_eq!(s.cut(at + 4000, 200), (at + 4000, 96));
        assert_eq!(s.cut(at + 5000, 200), (at + 4096, 0));
        assert_eq!(s.cut(at - 10, 20), (at, 10));
        assert_eq!(s.cut(u64::MAX - 4, 16), (at + 4096, 0));
        assert_eq!(s.cut(at + 8, u64::MAX), (at + 8, 4088));

        // The same at submission, where addresses are `usize`.
        let top = usize::MAX - 4;
        assert_eq!(s.inside(top, 16), (4096, 4096));
        assert_eq!(s.inside(at as usize + 8, usize::MAX), (8, 4096));
        assert_eq!(s.update(top, 16), 0);
        assert_eq!(s.update(at as usize + 5000, 16), 0);
        assert_eq!(s.copy_back(top, 16), 0);
    }
}
