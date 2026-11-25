//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queued: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    key: String,
    used: AtomicBool,
    /// Makes [`Pending::make`] panic where the host pipeline would be made.
    #[cfg(test)]
    panics: AtomicBool,
    /// The name of the thread that made it.
    #[cfg(test)]
    made_by: OnceLock<String>,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        Pending {
            shaders,
            graphics,
            queued: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            #[cfg(test)]
            panics: AtomicBool::new(false),
            #[cfg(test)]
            made_by: OnceLock::new(),
        }
    }

    /// Makes the host pipeline, once, and wakes who waits for it. A panic
    /// on the way (shader translation and pipeline creation chew on title
    /// data) is a refused pipeline, logged once: unwinding out of here
    /// would end the worker with the pipeline never made and still counted
    /// as waiting, and every later first use would wait out its budget.
    ///
    /// This relies on panics unwinding (no profile of the workspace sets
    /// `panic = "abort"`; with it nothing here is contained).
    fn make(&self) {
        if self.made.get().is_none() {
            let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                #[cfg(test)]
                if self.panics.load(Ordering::Relaxed) {
                    panic!("test: making a host pipeline panics");
                }
                HostPipeline::new(&self.shaders, self.graphics.as_deref())
            }))
            .unwrap_or_else(|_| {
                tracing::warn!(
                    key = %self.key,
                    shaders = ?self.shaders,
                    "making a host pipeline panicked; it has none and its draws are dropped"
                );
                None
            });
            // `OnceLock::get_or_init` would leave the cell empty after a
            // panic; this fills it either way.
            let _ = self.made.set(made);
            #[cfg(test)]
            let _ = self
                .made_by
                .set(std::thread::current().name().unwrap_or("").to_owned());
        }
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.made.get().is_none() {
        start(p);
        wait(p, crate::frame::current());
    }
    let made = p.made.get()?.as_ref();
    if made.is_some() && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    made
}

/// Queues `p` for the workers, once.
fn start(p: &Arc<Pending>) {
    if !p.queued.swap(true, Ordering::Relaxed) {
        PEAK.fetch_max(
            WAITING.fetch_add(1, Ordering::Relaxed) + 1,
            Ordering::Relaxed,
        );
        queue(p.clone());
    }
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p);
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static WAITED_US: AtomicU64 = AtomicU64::new(0);
    if FRAME.swap(frame, Ordering::Relaxed) != frame {
        WAITED_US.store(0, Ordering::Relaxed);
    }
    let left = BUDGET.saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// One queued pipeline, counted in [`WAITING`] until it is dropped: also
/// when what makes it unwinds.
struct Counted;

impl Counted {
    /// The pipeline is made; whether it was the last one waiting.
    fn made(self) -> bool {
        std::mem::forget(self);
        WAITING.fetch_sub(1, Ordering::Relaxed) == 1
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        WAITING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A worker's turn for one pipeline. Nothing in it ends the worker: a
/// panic (which [`Pending::make`] already contains for the pipeline
/// itself) is logged and the worker takes the next pipeline.
fn work(p: &Pending) {
    let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let counted = Counted;
        p.make();
        if counted.made() {
            drained();
        } else {
            super::library::save(false);
        }
    }));
    if turn.is_err() {
        tracing::warn!(key = %p.key, "a pipeline worker's turn panicked; the worker goes on");
    }
}

/// Hands `p` to the workers (half the logical processors, 2 to 8), started
/// on first use.
fn queue(p: Arc<Pending>) {
    static SENDER: OnceLock<std::sync::mpsc::Sender<Arc<Pending>>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Arc<Pending>>();
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        for i in 0..workers {
            let receiver = receiver.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        let next = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(p) = next else { return };
                        work(&p);
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            }
        }
        sender
    });
    if let Err(e) = sender.send(p) {
        // No worker: made here instead.
        let counted = Counted;
        e.0.make();
        drop(counted);
    }
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert!(p.queued.load(Ordering::Relaxed));
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// Whether `p` is made within 10 s.
    fn made_soon(p: &Pending) -> bool {
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        p.made.get().is_some()
    }

    fn panicking() -> Arc<Pending> {
        let p = Pending::new(Vec::new(), None);
        p.panics.store(true, Ordering::Relaxed);
        Arc::new(p)
    }

    /// A panic while a host pipeline is made is a pipeline without one,
    /// not one that is never made.
    #[test]
    fn a_panicking_make_is_a_refused_pipeline() {
        let _serial = crate::test_support::serial();
        let p = panicking();
        assert!(ready(&p).is_none());
        assert!(made_soon(&p), "the pipeline was never made");
        assert!(matches!(p.made.get(), Some(None)));
        assert!(ready(&p).is_none());
    }

    /// Panics do not end workers or leave pipelines counted as waiting:
    /// after more panics than there are workers, an ordinary pipeline is
    /// still made by a worker and nothing waits.
    #[test]
    fn workers_survive_panics() {
        let _serial = crate::test_support::serial();
        // 3 x the most workers there can be.
        let bad: Vec<Arc<Pending>> = (0..24).map(|_| panicking()).collect();
        for p in &bad {
            start(p);
        }
        for (i, p) in bad.iter().enumerate() {
            assert!(made_soon(p), "panicking pipeline {i} was never made");
            assert!(matches!(p.made.get(), Some(None)));
        }
        let good = Arc::new(Pending::new(Vec::new(), None));
        start(&good);
        assert!(made_soon(&good), "no worker is left");
        let by = good.made_by.get().map(String::as_str).unwrap_or("");
        assert!(by.starts_with("victory-pipelines-"), "made by {by:?}");
        // Other tests queue pipelines too (without the lock), so this
        // waits for the count instead of reading it once.
        let started = Instant::now();
        while waiting() != 0 && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(waiting(), 0);
    }
}
