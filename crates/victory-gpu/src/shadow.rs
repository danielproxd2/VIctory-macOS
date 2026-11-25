//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: read-only
//! guards and the runtime's reports), all of it where the memory is not the
//! runtime's. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! The ranges brought up to date are those draws are known to read (vertex
//! and index buffer views, constant buffers in tables, root descriptors,
//! buffer views in tables) and, for shaders that index the heap
//! themselves, every buffer range the heap's views reach
//! (`crate::bindless`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::holders::Holders;
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
    /// The pages changed since the copy took them (none: the memory is not
    /// the runtime's, copied whole each time).
    tracked: Option<crate::watch::Tracked>,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<std::collections::HashSet<(usize, usize)>>,
    /// The copy's bytes before the submission ran where its shaders write:
    /// what the GPU wrote differs. Disjoint segments (offset, bytes) in
    /// order, and the 64 KiB pieces (by title address) already compared.
    before: Mutex<Snapshot>,
    /// Its first bit in the buffer views' written-pages bitmap, one per
    /// 64 KiB title page it touches (tracked memory only): writes through
    /// views of it are marked there ([`Shadow::mark_entry`]).
    marks: Option<u32>,
}

/// Snapshot segments (offset, bytes) and the pieces compared.
type Snapshot = (Vec<(usize, Vec<u8>)>, std::collections::HashSet<usize>);

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

/// Returns a snapshot buffer to the pool, which keeps the largest up to
/// [`POOL_BYTES`] in all: a world submission snapshots several copies, and
/// fresh buffers cost a page fault per 4 KiB touched (millions per frame).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
    let mut total = 0;
    pool.retain(|v| {
        total += v.capacity();
        total <= POOL_BYTES
    });
}

/// Snapshot memory kept for reuse.
const POOL_BYTES: usize = 2 << 30;

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

    /// `n` dwords of the copy at title address `address` (tests).
    #[cfg(test)]
    pub(crate) fn read_host(&self, address: u64, n: usize) -> Vec<u32> {
        let at = (address - self.start) as usize;
        assert!(at + 4 * n <= self.len as usize);
        // SAFETY: inside the mapping (checked); the GPU is done with it.
        unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<u32>(), n).to_vec() }
    }

    /// Writes `bytes` into the copy at title address `address`, as shaders
    /// do (tests).
    #[cfg(test)]
    pub(crate) fn write_host(&self, address: u64, bytes: &[u8]) {
        let at = (address - self.start) as usize;
        assert!(at + bytes.len() <= self.len as usize);
        // SAFETY: inside the mapping (checked).
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.data.add(at), bytes.len()) };
    }

    /// Up to 64 bytes of the copy at title address `address`, as floats, and
    /// which copy it is (diagnostics: what shaders see there).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn peek(&self, address: u64, len: u64) -> String {
        let at = address.saturating_sub(self.start) as usize;
        let n = (len.min(64) as usize).min((self.len as usize).saturating_sub(at)) / 4;
        // SAFETY: inside the mapping (checked); a racing GPU write only makes
        // the logged value stale.
        let v = unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<f32>(), n) };
        format!("copy {:#x}+{:#x} {v:?}", self.start, self.len)
    }

    /// Offset of a title address from the buffer's start, within the
    /// buffer: 0 for an address before it, its length for one past it.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address.saturating_sub(self.start).min(self.len)
    }

    /// Where the title memory it copies starts, and its length.
    pub(crate) fn range(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    /// After a submission whose shaders wrote title memory at addresses they
    /// computed (`crate::memory`), somewhere in the 64 KiB page at `page`:
    /// the bytes of the page (cut to the copy) that differ from title memory
    /// go back into it, when the title has not written the page since the
    /// copy took it (then title memory is the copy as it was before the
    /// submission, and what differs is what the GPU wrote). Returns the
    /// bytes written; a page the title wrote meanwhile keeps the title's
    /// bytes (logged once).
    // shortcut: a page the copy holds only in part (at its ends) has no
    // "copied" mark and is compared without the title-write check.
    pub(crate) fn copy_back_page(&self, page: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (page.max(start), (page + PAGE).min(end));
        if from >= to {
            return 0;
        }
        if let Some(t) = &self.tracked
            && t.changed(page / PAGE)
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    page = format_args!("{page:#x}"),
                    "the title and translated shaders both wrote a page of title memory in one submission: the title's bytes kept (logged once)"
                )
            });
            return 0;
        }
        let n = to - from;
        // SAFETY: inside the mapping; only the submitting thread writes it.
        let host = unsafe { std::slice::from_raw_parts(self.data.add(from - start), n) };
        let mut title = vec![0u8; n];
        if !crate::host::read_into(from as *const u8, &mut title) || title == host {
            return 0;
        }
        {
            let _writing = crate::watch::writing(from, n);
            crate::host::write_mapped(from as *mut u8, host.as_ptr(), n);
        }
        // Our write is not the title's: the copy holds the page as it is.
        if let Some(t) = &self.tracked {
            t.taken(&[page / PAGE]);
        }
        n
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

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Arc<Shadow> {
        let tracked = crate::watch::track(start as usize, len as usize, true);
        let marks = tracked
            .as_ref()
            .and_then(|_| marks().take(pages(start, len)));
        let shadow = Arc::new(Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            tracked,
            pending: Mutex::default(),
            before: Mutex::default(),
            marks,
        });
        if let Some(base) = marks {
            self::marks().owners.insert(base, Arc::downgrade(&shadow));
        }
        shadow
    }

    /// The view-mark entry (`victory_shader::emit`, "Write marks") of a
    /// view of this copy whose first byte is at title address `start`: the
    /// bit of that byte's page and its offset in the page. None where
    /// writes through it are not marked (memory not tracked, or no bits
    /// left): such views declare what they write ([`Shadow::gpu_writes`]).
    pub(crate) fn mark_entry(&self, start: u64) -> Option<(u32, u32)> {
        let base = self.marks?;
        let page = (start >> 16).checked_sub(self.start >> 16)?;
        Some((base + u32::try_from(page).ok()?, (start & 0xFFFF) as u32))
    }

    /// Whether writes through views of this copy are marked.
    pub(crate) fn marked(&self) -> bool {
        self.marks.is_some()
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
    ///
    /// `marked`: the shaders mark the pages they write through this copy's
    /// views (`victory_shader::emit`, "Write marks"): no snapshot then, only
    /// the pages they marked are compared ([`Shadow::copy_back_marked`]).
    pub(crate) fn gpu_writes(
        self: &Arc<Self>,
        list: &HostList,
        address: u64,
        len: u64,
        marked: bool,
    ) {
        let (address, len) = self.cut(address, len);
        list.defer_gpu_write(
            self,
            address as usize,
            len as usize,
            marked && self.marked(),
        );
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// per copy and submission, segments covering the ranges the submission
    /// declares (the world declares overlapping views of its 174 MB heap
    /// dozens of times; one segment over all of them copied most of the
    /// heap), in reused memory (fresh snapshots cost ~2.3 million page
    /// faults per world frame).
    ///
    /// With `marked`, nothing is kept: the pages are armed instead, so that
    /// a title write there before the copy-back keeps the page's bytes from
    /// before it ([`crate::watch::Tracked::arm`]).
    pub(crate) fn writing(&self, address: usize, len: usize, marked: bool) {
        // (Saturating: a range a title made to pass the end of the address
        // space is cut to the copy below, not added up past 64 bits.)
        let end = address.saturating_add(len);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((address, end));
        if marked && let Some(t) = &self.tracked {
            t.arm(address, end);
            return;
        }
        let (from, to) = self.inside(address, len);
        if from >= to {
            return;
        }
        // SAFETY: inside the mapping.
        let host =
            |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let segments = &mut before.0;
        // The segments the range overlaps or touches.
        let first = segments.partition_point(|(at, bytes)| at + bytes.len() < from);
        let last = first
            + segments[first..]
                .iter()
                .take_while(|(at, _)| *at <= to)
                .count();
        if let [(at, bytes)] = &segments[first..last]
            && *at <= from
            && to <= at + bytes.len()
        {
            return;
        }
        let start = segments
            .get(first)
            .filter(|_| first < last)
            .map_or(from, |s| s.0.min(from));
        let end = segments[first..last]
            .last()
            .map_or(to, |(at, bytes)| (at + bytes.len()).max(to));
        #[cfg(feature = "diagnostics")]
        let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::SnapshotUs);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::SnapshotBytes, (end - start) as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::snapshot(self.start, self.len, (end - start) as u64);
        // Parts newly covered from the copy, the rest as it was taken.
        let mut grown = pooled(end - start);
        let mut at = start;
        for (taken, bytes) in segments.drain(first..last) {
            grown.extend_from_slice(host(at, taken));
            grown.extend_from_slice(&bytes);
            at = taken + bytes.len();
            give_back(bytes);
        }
        grown.extend_from_slice(host(at, end));
        segments.insert(first, (start, grown));
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_or_not(address, len, true)
    }

    /// After the submission ran: what its shaders wrote in the range stays
    /// in the copy and does not go into title memory (they computed it
    /// from memory with no copy, `crate::memory`).
    pub(crate) fn keep_writes(&self, address: usize, len: usize) {
        self.copy_back_or_not(address, len, false);
    }

    fn copy_back_or_not(&self, address: usize, len: usize, back: bool) -> usize {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let (segments, compared) = &mut *snapshot;
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = segments
            .iter()
            .find(|(at, bytes)| *at <= from && to <= at + bytes.len())
            && back
            && from < to
        {
            let before = &before[from - taken..to - taken];
            #[cfg(feature = "diagnostics")]
            let _timed =
                crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::CompareUs);
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
                // A whole piece is compared once per submission, whichever
                // declared ranges overlap it: then what the GPU wrote there
                // went back.
                if n == PIECE && !compared.insert(start + at) {
                    at += n;
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::CompareBytes, n as u64);
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
                        let _writing = crate::watch::writing(start + at, n);
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
            if let Some(t) = &self.tracked {
                let whole: Vec<usize> = synced
                    .into_iter()
                    .filter(|p| (first..last).contains(p))
                    .collect();
                t.taken(&whole);
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&(address, address.saturating_add(len)));
        if pending.is_empty() {
            compared.clear();
            for (_, bytes) in segments.drain(..) {
                give_back(bytes);
            }
            if let Some(t) = &self.tracked {
                t.disarm();
            }
        }
        written
    }

    /// After a submission whose shaders marked the 64 KiB title page at
    /// `page` as written through views of this copy: the bytes they changed
    /// go into title memory, and the copy takes the title's others. What
    /// they changed is what differs from the copy's bytes before the
    /// submission: title memory where the title has not written the page
    /// since the copy took it, else what the page held when the title first
    /// wrote it ([`crate::watch::Tracked::saved`], if the page was armed).
    /// Returns the bytes written.
    // shortcut: a page the title wrote that was not armed (a write through
    // the heap's views no list declared) keeps the title's bytes, losing
    // the shaders' there (counted).
    pub(crate) fn copy_back_marked(&self, page: usize) -> usize {
        let Some(t) = &self.tracked else {
            return 0;
        };
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (page.max(start), (page + PAGE).min(end));
        if from >= to {
            return 0;
        }
        // Our own write below must not save the page for this copy again.
        t.unarm(page / PAGE);
        let saved = t.saved(page / PAGE);
        if saved.is_none() && t.changed(page / PAGE) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::MarkedKept, 1);
            return 0;
        }
        let n = to - from;
        // SAFETY: inside the mapping; only the submitting thread writes it.
        let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(from - start), n) };
        let mut title = vec![0u8; n];
        if !crate::host::read_into(from as *const u8, &mut title) {
            return 0;
        }
        let written = match &saved {
            // The title's bytes are the copy's from before: what differs is
            // the shaders'.
            None => (*host != title[..]).then_some(host.as_ptr()),
            Some(before) => {
                let before = &before[from - page..to - page];
                let mut changed = false;
                for ((t, h), b) in title.iter_mut().zip(host.iter_mut()).zip(before) {
                    if *h != *b && *t != *h {
                        *t = *h;
                        changed = true;
                    }
                    *h = *t;
                }
                changed.then_some(title.as_ptr())
            }
        };
        if let Some(bytes) = written {
            let _writing = crate::watch::writing(from, n);
            crate::host::write_mapped(from as *mut u8, bytes, n);
        }
        // Equal to title memory now: taken, unless it holds the page in part.
        if from == page && to == page + PAGE {
            t.taken(&[page / PAGE]);
        }
        written.map_or(0, |_| n)
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
            // gaps whole cost ~1 s per frame). Under them, what the title
            // wrote since it was taken (`update_busy`).
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                let (a, e) = (a.max(from), e.min(to));
                if a > at {
                    bytes += self.update_free(at, a);
                }
                if e > a.max(at) {
                    bytes += self.update_busy(a.max(at), e);
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

    /// [`Shadow::update`] of `[from, to)` where a list submitted earlier
    /// declared that shaders write. A submission waits for the one before it,
    /// so none of this submission's shaders has run yet: the pages the title
    /// wrote since they were taken are copied as anywhere else. Skipped, the
    /// title's constants there stayed stale for the whole submission (a
    /// dispatch read its sizes as zeros and looped until the device was
    /// removed). The same bytes go into the snapshot `writing` took, so
    /// copy-back neither takes them for the GPU's nor puts them back over a
    /// later title write.
    fn update_busy(&self, from: usize, to: usize) -> usize {
        let Some(t) = &self.tracked else {
            return 0;
        };
        let runs = t.take(from, to);
        if runs.is_empty() {
            return 0;
        }
        let start = self.start as usize;
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let mut copied = 0;
        for (a, b) in runs {
            copied += self.copy(a, b);
            let (a, b) = (a - start, b - start);
            for (at, bytes) in before.0.iter_mut() {
                let (s, e) = ((*at).max(a), (*at + bytes.len()).min(b));
                if s < e {
                    // SAFETY: inside the mapping.
                    let host = unsafe { std::slice::from_raw_parts(self.data.add(s), e - s) };
                    bytes[s - *at..e - *at].copy_from_slice(host);
                }
            }
        }
        copied
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where no
    /// shader writes: the pages changed since taken, or all of it where the
    /// memory is not tracked.
    fn update_free(&self, from: usize, to: usize) -> usize {
        if let Some(t) = &self.tracked {
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            let runs = t.take(from, to);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(
                crate::copystat::C::WatchUs,
                started.elapsed().as_micros() as u64,
            );
            // Cut to the range: next to it shaders may be writing the copy.
            let copied: usize = runs.into_iter().map(|(a, b)| self.copy(a, b)).sum();
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::Watches, 1);
                add(C::Checked, (to - from) as u64);
                add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
            }
            return copied;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::WholeBytes, (to - from) as u64);
            add(C::WholeUs, started.elapsed().as_micros() as u64);
        }
        to - from
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

impl Drop for Shadow {
    fn drop(&mut self) {
        if let Some(base) = self.marks {
            let mut m = marks();
            m.owners.remove(&base);
            m.give_back(base, pages(self.start, self.len));
        }
    }
}

/// The 64 KiB title pages `[start, start + len)` touches.
fn pages(start: u64, len: u64) -> u32 {
    ((start + len.max(1) - 1) >> 16).saturating_sub(start >> 16) as u32 + 1
}

/// The bits of the buffer views' written-pages bitmap (`victory_shader::
/// emit`, "Write marks"): a run per tracked copy, a bit per title page it
/// touches, for as long as it lives.
#[derive(Default)]
struct Marks {
    /// Free runs below `end`, by first bit: their length.
    free: BTreeMap<u32, u32>,
    /// Bits handed out at some time: those past it are clear.
    end: u32,
    /// The copy holding each run, by its first bit.
    owners: BTreeMap<u32, std::sync::Weak<Shadow>>,
}

static MARKS: Mutex<Marks> = Mutex::new(Marks {
    free: BTreeMap::new(),
    end: 0,
    owners: BTreeMap::new(),
});

fn marks() -> std::sync::MutexGuard<'static, Marks> {
    MARKS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Marks {
    /// A run of `n` bits: its first. None when the bitmap is full (the last
    /// bit takes the marks of offsets past it, so it is never handed out).
    fn take(&mut self, n: u32) -> Option<u32> {
        if let Some((&at, &len)) = self.free.iter().find(|&(_, &len)| len >= n) {
            self.free.remove(&at);
            if len > n {
                self.free.insert(at + n, len - n);
            }
            return Some(at);
        }
        if self.end + n < victory_shader::emit::VIEW_WRITE_BITS {
            self.end += n;
            return Some(self.end - n);
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no bits left in the views' written-pages bitmap: copies made now declare their shaders' writes"));
        None
    }

    /// The run of `n` bits at `at` is free again (joined to its neighbours).
    fn give_back(&mut self, mut at: u32, mut n: u32) {
        if let Some((&before, &len)) = self.free.range(..at).next_back()
            && before + len == at
        {
            self.free.remove(&before);
            (at, n) = (before, n + len);
        }
        if let Some(len) = self.free.remove(&(at + n)) {
            n += len;
        }
        self.free.insert(at, n);
    }
}

/// The buffer views' written-pages bitmap: the buffer, its mapping (bits
/// as dwords) and host GPU address.
struct Bitmap {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: D3D12 resources are free-threaded; the mapping is read and cleared
// only between submissions, which run one at a time.
unsafe impl Send for Bitmap {}
// SAFETY: as above.
unsafe impl Sync for Bitmap {}

fn bitmap() -> Option<&'static Bitmap> {
    static BITMAP: std::sync::OnceLock<Option<Bitmap>> = std::sync::OnceLock::new();
    BITMAP
        .get_or_init(|| {
            let bytes = u64::from(victory_shader::emit::VIEW_WRITE_BITS / 8);
            let (buffer, data) = host()?.shared_buffer(bytes)?;
            crate::host::name(&buffer, || "views' written pages".into());
            // SAFETY: our mapped buffer of `bytes` bytes.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some(Bitmap {
                _buffer: buffer,
                data: data as usize,
                gpu,
            })
        })
        .as_ref()
}

/// The host GPU address of the buffer views' written-pages bitmap, for the
/// root UAV of shaders that mark their writes.
pub(crate) fn view_writes() -> Option<u64> {
    bitmap().map(|b| b.gpu)
}

/// After a submission: every page its shaders marked as written through
/// views goes back ([`Shadow::copy_back_marked`]), but for pages in
/// `withheld` (ranges of copies that lists whose writes stay out of title
/// memory declared); the bitmap is cleared. Before the copy-backs of
/// declared ranges, which disarm the copies.
// shortcut: marks do not tell lists apart: a page that a withheld list and
// another both wrote through views goes back unless the withheld list
// declared it.
pub(crate) fn copy_back_marks(withheld: &[(Arc<Shadow>, usize, usize)]) {
    let Some(b) = bitmap() else {
        return;
    };
    #[cfg(feature = "diagnostics")]
    let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::MarkedUs);
    let words = (victory_shader::emit::VIEW_WRITE_BITS / 32) as usize;
    // SAFETY: the mapped bitmap; the GPU is done with it (one submission
    // runs at a time).
    let bits = unsafe { std::slice::from_raw_parts_mut(b.data as *mut u32, words) };
    // Marks of offsets past the bitmap land in its last bit, no copy's.
    bits[words - 1] = 0;
    let mut marked = Vec::new();
    let used = (marks().end as usize).div_ceil(32).min(words);
    for (w, word) in bits[..used].iter_mut().enumerate() {
        let mut x = std::mem::take(word);
        while x != 0 {
            marked.push((w * 32) as u32 + x.trailing_zeros());
            x &= x - 1;
        }
    }
    if marked.is_empty() {
        return;
    }
    let pages: Vec<(Arc<Shadow>, usize)> = {
        let m = marks();
        marked
            .iter()
            .filter_map(|&n| {
                let (&base, copy) = m.owners.range(..=n).next_back()?;
                let copy = copy.upgrade()?;
                let k = n - base;
                (k < pages(copy.start, copy.len)).then(|| {
                    let page = (((copy.start >> 16) + u64::from(k)) << 16) as usize;
                    (copy, page)
                })
            })
            .collect()
    };
    for (copy, page) in pages {
        if withheld
            .iter()
            .any(|(c, a, n)| Arc::ptr_eq(c, &copy) && *a < page + PAGE && page < a + n)
        {
            continue;
        }
        let written = copy.copy_back_marked(page);
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::MarkedPages, 1);
            crate::copystat::add(crate::copystat::C::MarkedBytes, written as u64);
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = written;
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
struct Buffers {
    map: BTreeMap<(u64, usize), Entry>,
    /// The same keys with their ends, for finding the buffer that holds a
    /// range without walking past every buffer that does not.
    holders: Holders,
}

static BUFFERS: Mutex<Buffers> = Mutex::new(Buffers {
    map: BTreeMap::new(),
    holders: Holders::new(),
});

fn buffers() -> std::sync::MutexGuard<'static, Buffers> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts copies made and title buffers released: [`all`] may differ when
/// it changed.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The generation of [`all`]: read it before calling that.
pub(crate) fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

fn changed() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
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
    buffers.holders.insert((start, resource), end);
    buffers
        .map
        .insert((start, resource), Entry { end, shadow: None });
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    let mut buffers = buffers();
    buffers.holders.remove((start, resource));
    if buffers
        .map
        .remove(&(start, resource))
        .is_some_and(|e| e.shadow.is_some())
    {
        changed();
    }
}

/// Whether the buffer registered as (`resource`, `start`) has a host copy.
#[cfg(test)]
pub(crate) fn copied(resource: usize, start: u64) -> bool {
    let buffers = buffers();
    buffers
        .map
        .get(&(start, resource))
        .is_some_and(|e| e.shadow.is_some())
}

#[cfg(test)]
thread_local! {
    /// How many buffers the last [`find`] on this thread looked at to tell
    /// which one holds the range.
    static LOOKED_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut buffers = buffers();
    // The buffer that starts last at or below the address and reaches the
    // range's end. (This was a walk down from the address: first over at
    // most 64 buffers, which missed a buffer with more than that many
    // others inside it, then over all of them, which a title with many
    // buffers pays on every draw, under this lock. The tree skips the
    // buffers that end too early.)
    let found = buffers.holders.holder(address, end);
    #[cfg(test)]
    LOOKED_AT.set(found.looked_at);
    let Some(key) = found.key else {
        drop(buffers);
        return loose(address, end);
    };
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
        entry.shadow = Some(Shadow::new(buffer, data, gpu, key.0, len));
        changed();
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
    let shadow = Shadow::new(buffer, data, gpu, start, len);
    map.insert((start, len), shadow.clone());
    changed();
    Some(shadow)
}

/// The copy a buffer view of `len` bytes at `address`, whose elements are
/// `element` bytes, reads: the innermost title buffer's ([`find`]), unless
/// the view does not start on an element of that copy. A host view counts
/// its first element from its buffer's start; a console view starts at any
/// address, and the title places structured views at ring positions inside
/// its big GPU heap (the meshlet culling's 456-byte constants, the colour
/// bakes' parameter blocks: refused, their slots read zeros, every meshlet
/// culled). Such a view gets a copy of its own bytes, starting at its
/// address (`true` with it), which lives while a heap slot made from the
/// view or a list holds it.
pub(crate) fn for_view(address: u64, len: u64, element: u64) -> Option<(Arc<Shadow>, bool)> {
    let shadow = find(address, len)?;
    if element == 0 || shadow.offset(address).is_multiple_of(element) {
        return Some((shadow, false));
    }
    #[cfg(feature = "diagnostics")]
    VIEWS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    exact(address, len).map(|s| (s, true))
}

/// Asks for, and copies made of, views off their copy's elements, the
/// copies' bytes and the largest (diagnostics).
#[cfg(feature = "diagnostics")]
static VIEWS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Logs [`VIEWS`] per frame over `frames`.
#[cfg(feature = "diagnostics")]
pub(crate) fn log_views(frames: f64) {
    use std::sync::atomic::Ordering::Relaxed;
    let [asks, made, bytes, largest] = [0, 1, 2, 3].map(|i| VIEWS[i].swap(0, Relaxed));
    let live = EXACT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .1
        .values()
        .filter(|w| w.strong_count() > 0)
        .count();
    if asks > 0 {
        tracing::info!(
            "buffer views off their copy's elements: {:.0} asked for/frame, {:.0} copies made/frame ({:.1} KB), largest {largest} bytes, {live} live",
            asks as f64 / frames,
            made as f64 / frames,
            bytes as f64 / frames / 1e3
        );
    }
}

/// Copies of single views' bytes ([`for_view`]), by (start, length), held
/// by the heap slots made from them and the lists that use them: a copy is
/// found again for as long as a slot shows it, so draws refresh the copy
/// the GPU reads (a capped cache dropped copies slots still showed, and a
/// new copy of the same bytes took the refreshes). With the size the map
/// is next pruned of dead entries at.
#[allow(clippy::type_complexity)]
static EXACT: Mutex<(usize, BTreeMap<(u64, u64), std::sync::Weak<Shadow>>)> =
    Mutex::new((0, BTreeMap::new()));

fn exact(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let mut guard = EXACT.lock().unwrap_or_else(|e| e.into_inner());
    let (prune_at, map) = &mut *guard;
    if let Some(shadow) = map.get(&(address, len)).and_then(std::sync::Weak::upgrade) {
        return Some(shadow);
    }
    // Ring positions change every frame: entries die by the hundred.
    if map.len() >= *prune_at {
        map.retain(|_, w| w.strong_count() > 0);
        *prune_at = (2 * map.len()).max(1024);
    }
    let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(256))?;
    crate::host::name(&buffer, || format!("title view {address:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let shadow = Shadow::new(buffer, data, gpu, address, len);
    map.insert((address, len), Arc::downgrade(&shadow));
    #[cfg(feature = "diagnostics")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        VIEWS[1].fetch_add(1, Relaxed);
        VIEWS[2].fetch_add(len, Relaxed);
        VIEWS[3].fetch_max(len, Relaxed);
    }
    Some(shadow)
}

/// Every host copy of title memory made so far (buffers and windows).
pub(crate) fn all() -> Vec<Arc<Shadow>> {
    let mut all: Vec<Arc<Shadow>> = buffers()
        .map
        .values()
        .filter_map(|e| e.shadow.clone())
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned(),
    );
    all
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

    /// The same with thousands of buffers, through `find` itself: the big
    /// buffer is found, a range in no buffer becomes loose memory, and
    /// neither lookup looks at more than a few of the buffers below it.
    /// The addresses are in no test's memory: nothing here reads them.
    #[test]
    fn find_looks_at_few_of_many_buffers() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        // (Not the range `draw`'s rectangle test registers its buffer in.)
        const BASE: u64 = 0x7c00_0000_0000;
        const SMALL: u64 = 5000;
        const BIG: u64 = 1 << 20;
        let mut registered = Registered(Vec::new());
        registered.add(OUTER, BASE, BIG);
        for i in 0..SMALL {
            registered.add(INNER + i as usize, BASE + 16 * i, 16);
        }
        // The count is zeroed before each lookup and must be set by it: a
        // `find` that stops counting, or walks without counting, leaves 0.
        let counted = |address: u64| {
            LOOKED_AT.set(0);
            let found = find(address, 16).map(|s| (s.start, s.len));
            let looked_at = LOOKED_AT.get();
            assert!(
                (1..=120).contains(&looked_at),
                "{looked_at} buffers looked at for {address:#x}"
            );
            found
        };
        assert_eq!(counted(BASE + 16 * SMALL - 8), Some((BASE, BIG)));
        // Just past the big buffer, every one of them below the address.
        assert_eq!(
            counted(BASE + BIG - 8),
            Some((BASE, WINDOW)),
            "loose memory"
        );
        // A buffer the title released is not found any more.
        registered.0.retain(|&(resource, _)| resource != OUTER);
        unregister(OUTER, BASE);
        let found = counted(BASE + 16 * SMALL - 8);
        assert_eq!(found, Some((BASE, WINDOW)), "loose memory");
        drop(registered);
        assert_eq!(buffers().holders.holder(BASE + 8, BASE + 9).key, None);
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
        s.gpu_writes(&list, at + 1000, 64, false);
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
        assert!(s.before.lock().unwrap().0.is_empty());

        // A declared range outside the copy claims nothing and writes
        // nothing back.
        let list = host.new_list().unwrap();
        s.gpu_writes(&list, at + 9000, 64, false);
        s.gpu_writes(&list, u64::MAX - 8, 64, false);
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

    /// The title writes a constant under a range an earlier list of the
    /// submission declared as written by shaders: the refresh before the
    /// submission runs still brings it to the copy, a shader's write still
    /// goes back, and a title write made after the refresh is not undone.
    #[test]
    fn title_writes_under_a_busy_range_are_copied() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let title = |at: usize| (p + at) as *mut u8;
        let host = |at: usize| copy.read_host((p + at) as u64 & !3, 1)[0].to_le_bytes()[at % 4];
        copy.update(p, len);
        // An earlier list declares all of it as written by shaders.
        copy.writing(p, len, false);
        // SAFETY: (all title reads and writes below) bytes of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x55) };
        assert!(
            copy.update(p + PAGE + 16, 16) > 0,
            "the title's write is copied"
        );
        assert_eq!(
            host(PAGE + 20),
            0x55,
            "the title's constant reaches the copy"
        );
        // The shaders run: one byte written. The title writes meanwhile.
        copy.write_host((p + 10) as u64, &[0xaa]);
        // SAFETY: a byte of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x66) };
        copy.copy_back(p, len);
        // SAFETY: bytes of our pool.
        unsafe {
            assert_eq!(
                title(10).read_volatile(),
                0xaa,
                "the shader's write goes back"
            );
            assert_eq!(
                title(PAGE + 20).read_volatile(),
                0x66,
                "the title's later write stays"
            );
        }
        unregister(p, p as u64);
    }

    /// Ranges shaders write far apart are kept apart (not the copy between
    /// them); what the GPU wrote goes back once, whichever overlapping
    /// ranges declared it, and nothing else does.
    #[test]
    fn snapshots_cover_the_declared_ranges_and_writes_go_back_once() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let p = crate::watch::tests::pool(4);
        let len = 4 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        copy.update(p, len);
        let bytes = |s: &Shadow| {
            s.before
                .lock()
                .unwrap()
                .0
                .iter()
                .map(|(_, b)| b.len())
                .sum::<usize>()
        };
        copy.writing(p + 16, 64, false);
        copy.writing(p + 3 * PAGE, 100, false);
        assert_eq!(bytes(&copy), 164, "two segments");
        copy.writing(p + PAGE, 2 * PAGE, false);
        copy.writing(p + PAGE, PAGE, false);
        copy.writing(p + 80, 8, false);
        assert_eq!(
            bytes(&copy),
            72 + 2 * PAGE + 100,
            "merged where they overlap or touch"
        );
        // The GPU writes inside two ranges.
        copy.write_host((p + 20) as u64, &[1, 2, 3]);
        copy.write_host((p + PAGE + 5) as u64, &[9]);
        assert_eq!(
            copy.copy_back(p + 16, 64),
            64,
            "the range's part of the piece"
        );
        assert_eq!(
            copy.copy_back(p + PAGE, 2 * PAGE),
            PAGE,
            "the piece written"
        );
        assert_eq!(copy.copy_back(p + PAGE, PAGE), 0, "compared once");
        assert_eq!(copy.copy_back(p + 3 * PAGE, 100), 0);
        assert_eq!(copy.copy_back(p + 80, 8), 0);
        // SAFETY: our pool.
        let title = |at: usize| unsafe { (at as *const u8).read_volatile() };
        assert_eq!(
            [
                title(p + 20),
                title(p + 21),
                title(p + 22),
                title(p + PAGE + 5)
            ],
            [1, 2, 3, 9]
        );
        assert_eq!(bytes(&copy), 0, "the snapshot goes with the last range");
        unregister(p, p as u64);
    }

    /// Pages shaders marked (`copy_back_marks`, `victory_shader::emit`
    /// "Write marks") go back by the bytes the GPU changed. The title writes
    /// a marked page after the copy took it (the guard saves its bytes from
    /// before) and a page nobody marked: the shader's byte and the title's
    /// both stay in the first; the second keeps the title's, and the copy's
    /// byte there (no mark) is not put back. The bits are cleared.
    #[test]
    fn marked_pages_go_back_by_the_bytes_the_gpu_changed() {
        let Some(_gpu) = crate::testing::gpu() else {
            return;
        };
        let p = crate::watch::tests::pool(3);
        let len = 3 * PAGE;
        register(p, p as u64, len as u64);
        let (Some(copy), Some(b)) = (find(p as u64, len as u64), bitmap()) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let base = copy.marks.expect("tracked memory gets mark bits") as usize;
        assert_eq!(
            copy.mark_entry(p as u64 + PAGE as u64 + 8),
            Some((base as u32 + 1, 8))
        );
        copy.update(p, len);
        copy.writing(p, len, true);
        assert!(copy.before.lock().unwrap().0.is_empty(), "no snapshot");
        // The shaders write a byte in each page; the title writes pages 1
        // and 2 meanwhile.
        copy.write_host((p + 10) as u64, &[0xaa]);
        copy.write_host((p + PAGE + 20) as u64, &[0xbb]);
        copy.write_host((p + 2 * PAGE + 30) as u64, &[0xcc]);
        let title = |at: usize| (p + at) as *mut u8;
        // SAFETY: (title writes and reads below) bytes of our pool.
        unsafe {
            title(PAGE + 40).write_volatile(0x11);
            title(2 * PAGE + 50).write_volatile(0x22);
        }
        // The shaders marked pages 0 and 1.
        let bits = b.data as *mut u32;
        for n in [base, base + 1] {
            // SAFETY: a dword of the mapped bitmap.
            unsafe { *bits.add(n / 32) |= 1 << (n % 32) };
        }
        copy_back_marks(&[]);
        // SAFETY: bytes of our pool.
        let read = |at: usize| unsafe { title(at).read_volatile() };
        assert_eq!(read(10), 0xaa, "the shader's byte, page 0");
        assert_eq!(
            (read(PAGE + 20), read(PAGE + 40)),
            (0xbb, 0x11),
            "both writers' bytes, page 1"
        );
        assert_eq!(
            (read(2 * PAGE + 30), read(2 * PAGE + 50)),
            (0, 0x22),
            "page 2 not marked"
        );
        // SAFETY: dwords of the mapped bitmap.
        let left: u32 = (base / 32..=(base + 1) / 32)
            .map(|w| unsafe { *bits.add(w) })
            .fold(0, |a, w| a | w);
        assert_eq!(left, 0, "bits cleared");
        assert_eq!(copy.copy_back(p, len), 0, "nothing kept to compare");
        unregister(p, p as u64);
    }

    /// Mark bits are handed out in runs and come back joined.
    #[test]
    fn mark_runs_are_reused_and_joined() {
        let mut m = Marks::default();
        assert_eq!((m.take(3), m.take(2)), (Some(0), Some(3)));
        m.give_back(0, 3);
        assert_eq!(m.take(1), Some(0), "first fit");
        m.give_back(3, 2);
        assert_eq!(
            m.free.iter().collect::<Vec<_>>(),
            [(&1, &4)],
            "3-4 joins the free 1-2"
        );
        m.give_back(0, 1);
        assert_eq!(
            m.free.iter().collect::<Vec<_>>(),
            [(&0, &5)],
            "and 0 joins them"
        );
        assert_eq!(m.take(5), Some(0));
        assert_eq!(pages(0x1_fff0, 0x20), 2, "a range across a page boundary");
        assert_eq!(pages(0x2_0000, 0x1_0000), 1);
    }
}
