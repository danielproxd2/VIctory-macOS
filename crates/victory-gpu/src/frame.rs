//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: Grand Theft Auto VI polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

/// What the pacing clock reads and waits on: real time in a run. Tests
/// step a clock of their own, so that what they assert about ticks does
/// not depend on when the test thread gets to run.
trait Time {
    /// Time since some fixed moment.
    fn now(&self) -> Duration;
    /// Returns `wait` later (the real one: no earlier, and as much later
    /// as the scheduler makes it).
    fn sleep(&self, wait: Duration);
}

/// The process's clock.
struct Real;

impl Time for Real {
    fn now(&self) -> Duration {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
    fn sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

struct Clock {
    /// When the interval was set, on the pacer's [`Time`].
    start: Duration,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

/// The frame clock and what was handed out from it.
struct Pacer<T> {
    time: T,
    clock: Mutex<Option<Clock>>,
    /// Frames begun so far.
    frame: AtomicU64,
    /// Per event kind (origin, vblank, flip and later kinds): the last
    /// tick a wait for it ended on.
    last: [AtomicU64; 3],
}

static PACER: Pacer<Real> = Pacer::new(Real);

impl<T: Time> Pacer<T> {
    const fn new(time: T) -> Pacer<T> {
        Pacer {
            time,
            clock: Mutex::new(None),
            frame: AtomicU64::new(0),
            last: [const { AtomicU64::new(0) }; 3],
        }
    }

    fn clock(&self) -> std::sync::MutexGuard<'_, Option<Clock>> {
        self.clock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn last(&self, kind: u32) -> &AtomicU64 {
        &self.last[(kind as usize).min(self.last.len() - 1)]
    }

    /// A frame every `length` microseconds from now on.
    fn set_interval(&self, length: u32) {
        let interval = match length {
            0 => SIXTIETH,
            us => Duration::from_micros(u64::from(us)),
        };
        tracing::info!(?interval, "frame interval");
        // A new interval starts from now, after the last tick handed out.
        let first = self
            .last
            .iter()
            .map(|t| t.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        *self.clock() = Some(Clock {
            start: self.time.now(),
            interval,
            first,
        });
    }

    /// The tick a wait for `kind` ends on, and how long until it: the first
    /// one after the last a wait for `kind` took (an event happens once per
    /// tick: a wait woken a little early must not let the next one end on the
    /// same tick), the latest one passed if that is later. A wait begun after
    /// its tick passed (a frame slower than the interval) so ends at once, as
    /// an event signalled at the tick and left signalled until a wait takes it
    /// would (assumed: nothing we have says what a late wait does); waiting for
    /// the next tick instead cost the cutscene's frames 0-33 ms each (4 % of
    /// the frame thread, run perf3). A wait that
    /// comes within `limit` takes the tick here, under the clock's lock, so two
    /// threads waiting for one kind never take the same tick.
    fn next_tick(&self, kind: u32, limit: Duration) -> (u64, Duration) {
        let now = self.time.now();
        let mut clock = self.clock();
        let c = clock.get_or_insert_with(|| Clock {
            start: now,
            interval: SIXTIETH,
            first: 0,
        });
        let elapsed = now.saturating_sub(c.start).as_nanos();
        let interval = c.interval.as_nanos().max(1);
        let tick = (c.first + (elapsed / interval) as u64).max(self.last(kind).load(Ordering::Relaxed) + 1);
        let at = u128::from(tick - c.first) * interval;
        let wait = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
        if wait <= limit {
            self.last(kind).fetch_max(tick, Ordering::Relaxed);
        }
        (tick, wait)
    }

    /// Blocks until its tick (or the time-out); an origin wait then begins
    /// a frame, whose number is returned. Ticks are absolute (start plus
    /// whole intervals), so lateness never accumulates: after a wait that
    /// comes back late (the thread was not run), the next one ends at once
    /// on the latest tick passed, the ticks before it are left unused, and
    /// the one after that ends on a tick again.
    fn wait(&self, kind: u32, timeout_ms: u32, flags: u32) -> (HRESULT, Option<u64>) {
        let limit = Duration::from_millis(u64::from(timeout_ms));
        // The tick is only logged (diagnostics); taking it is what counts.
        #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
        let (tick, wait) = self.next_tick(kind, limit);
        let hr = if wait > limit {
            self.time.sleep(limit);
            TIMEOUT
        } else {
            self.time.sleep(wait);
            S_OK
        };
        #[cfg(feature = "diagnostics")]
        {
            WAITS[0].fetch_add(1, Ordering::Relaxed);
            if hr == S_OK && wait.is_zero() {
                WAITS[1].fetch_add(1, Ordering::Relaxed);
            }
        }
        // A frame begun on a time-out takes no tick: its flip still does.
        // Tokens count frames (consecutive), not ticks.
        let frame = (kind == ORIGIN && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0))
            .then(|| self.frame.fetch_add(1, Ordering::Relaxed) + 1);
        #[cfg(feature = "diagnostics")]
        tracing::debug!(
            kind,
            timeout_ms,
            flags,
            hr,
            tick,
            wait_us = wait.as_micros() as u64,
            frame = self.frame.load(Ordering::Relaxed),
            "frame wait"
        );
        (hr, frame)
    }
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    PACER.set_interval(length);
    S_OK
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// Frames begun so far (frame waits that ended).
pub(crate) fn current() -> u64 {
    PACER.frame.load(Ordering::Relaxed)
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let (hr, frame) = PACER.wait(kind, timeout_ms, flags);
    if let Some(frame) = frame
        && !token.is_null()
    {
        // SAFETY: checked non-null; the title passes its token.
        unsafe { token.write(frame) };
    }
    hr
}

/// Diagnostics: frame waits, and those that ended at once (their tick had
/// passed); presents whose flip tick was the previous present's.
#[cfg(feature = "diagnostics")]
static WAITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
#[cfg(feature = "diagnostics")]
static PRESENT_TICK: AtomicU64 = AtomicU64::new(0);

/// Diagnostics: a frame was presented (counts presents sharing a flip tick).
#[cfg(feature = "diagnostics")]
pub(crate) fn presented() {
    let flip = PACER.last[2].load(Ordering::Relaxed);
    if flip != 0 && PRESENT_TICK.swap(flip, Ordering::Relaxed) == flip {
        WAITS[2].fetch_add(1, Ordering::Relaxed);
    }
}

/// Diagnostics: logs [`WAITS`] since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn log() {
    let [waits, at_once, shared] = [0, 1, 2].map(|i| WAITS[i].swap(0, Ordering::Relaxed));
    tracing::info!("frame waits: {waits}, {at_once} of them ended at once; presents sharing a flip tick: {shared}");
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    const VBLANK: u32 = 1;
    const FLIP: u32 = 2;
    const MS: Duration = Duration::from_millis(1);

    /// A clock that only moves when a wait sleeps on it, by what the test
    /// says: exactly the time asked for, or with the next entry of `off`
    /// added (a thread run late) or taken away (one woken early).
    #[derive(Default)]
    struct Stepped {
        now: Mutex<Duration>,
        /// Signed nanoseconds, one entry per sleep to come.
        off: Mutex<std::collections::VecDeque<i64>>,
        slept: Mutex<Vec<Duration>>,
    }

    impl Time for Stepped {
        fn now(&self) -> Duration {
            *self.now.lock().unwrap()
        }
        fn sleep(&self, wait: Duration) {
            self.slept.lock().unwrap().push(wait);
            let off = self.off.lock().unwrap().pop_front().unwrap_or(0);
            let took = (wait.as_nanos() as i64 + off).max(0) as u64;
            *self.now.lock().unwrap() += Duration::from_nanos(took);
        }
    }

    impl Pacer<Stepped> {
        fn stepped() -> Pacer<Stepped> {
            Pacer::new(Stepped::default())
        }
        /// The next sleep takes this much longer than asked.
        fn late(&self, by: Duration) {
            let mut off = self.time.off.lock().unwrap();
            off.push_back(by.as_nanos() as i64);
        }
        /// The next sleep ends this much before its time.
        fn early(&self, by: Duration) {
            let mut off = self.time.off.lock().unwrap();
            off.push_back(-(by.as_nanos() as i64));
        }
        fn now(&self) -> Duration {
            self.time.now()
        }
        /// Time passes with nobody waiting (a frame slower than a tick).
        fn pass(&self, by: Duration) {
            *self.time.now.lock().unwrap() += by;
        }
        fn last_sleep(&self) -> Duration {
            *self.time.slept.lock().unwrap().last().unwrap()
        }
        fn ended_on(&self, kind: u32) -> u64 {
            self.last(kind).load(Ordering::Relaxed)
        }
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let p = Pacer::stepped();
        p.set_interval(2_000);
        assert_eq!(p.wait(ORIGIN, 1_000, 0), (S_OK, Some(1)));
        assert_eq!((p.now(), p.ended_on(ORIGIN)), (2 * MS, 1));
        // The next wait ends one interval later, on the next tick.
        assert_eq!(p.wait(ORIGIN, 1_000, 0), (S_OK, Some(2)));
        assert_eq!((p.now(), p.ended_on(ORIGIN)), (4 * MS, 2));
        // Out of time before the tick: no frame, no tick taken, and no
        // longer waited than the title allowed.
        assert_eq!(p.wait(ORIGIN, 0, 0), (TIMEOUT, None));
        assert_eq!((p.now(), p.ended_on(ORIGIN)), (4 * MS, 2));
        assert_eq!(p.wait(ORIGIN, 1, 0), (TIMEOUT, None));
        assert_eq!((p.last_sleep(), p.now()), (MS, 5 * MS));
        // Tokens count frames, a frame started early on a time-out too.
        assert_eq!(p.wait(ORIGIN, 0, TOKEN_ON_TIMEOUT), (TIMEOUT, Some(3)));
        assert_eq!(p.wait(ORIGIN, 1_000, 0), (S_OK, Some(4)));
        assert_eq!((p.now(), p.ended_on(ORIGIN)), (6 * MS, 3));
        // Only origin waits begin frames. No vblank wait has taken tick 3,
        // which the clock is on: this one ends on it, at once.
        assert_eq!(p.wait(VBLANK, 1_000, TOKEN_ON_TIMEOUT), (S_OK, None));
        assert_eq!(p.frame.load(Ordering::Relaxed), 4);
        assert_eq!((p.last_sleep(), p.now()), (Duration::ZERO, 6 * MS));
        assert_eq!(p.ended_on(VBLANK), 3);

        // The title's loop: an origin poll (which starts the frame, on a
        // time-out too), then a blocking flip wait. Each flip takes its own
        // tick, the one after the flip before it, and ticks go on across
        // the change of interval (the last one handed out was 3, at 6 ms,
        // which no flip wait had taken: the first one ends on it at once).
        // An origin poll times out while the origin's next tick is ahead,
        // and ends at once on the tick the flip before it ended on.
        p.set_interval(5_000);
        let mut polls = Vec::new();
        let flips: Vec<(u64, Duration)> = (0..4)
            .map(|frame| {
                let (poll, token) = p.wait(ORIGIN, 0, TOKEN_ON_TIMEOUT);
                assert_eq!(token, Some(5 + frame));
                polls.push((poll, p.ended_on(ORIGIN)));
                assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
                (p.ended_on(FLIP), p.now())
            })
            .collect();
        assert_eq!(polls, [(TIMEOUT, 3), (TIMEOUT, 3), (S_OK, 4), (S_OK, 5)]);
        assert_eq!(
            flips,
            [(3, 6 * MS), (4, 11 * MS), (5, 16 * MS), (6, 21 * MS)]
        );
        // Even when a flip wait begins on the tick another just ended on.
        assert_eq!(p.next_tick(FLIP, Duration::ZERO), (7, 5 * MS));
        // A flip wait begun after its tick passed (two ticks missed: 7 at
        // 26 ms and 8 at 31 ms) ends at once, on the latest tick passed;
        // the next one waits for the next.
        p.pass(12 * MS);
        assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
        assert_eq!((p.last_sleep(), p.now()), (Duration::ZERO, 33 * MS));
        assert_eq!(p.ended_on(FLIP), 8);
        assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
        assert_eq!((p.now(), p.ended_on(FLIP)), (36 * MS, 9));
    }

    /// A sleep may end before its time (timers are coarse). The wait still
    /// counts as its tick, and the next wait for the same event must not
    /// end on that tick again, although by the clock it has not passed.
    #[test]
    fn a_wait_woken_early_does_not_share_its_tick() {
        let p = Pacer::stepped();
        p.set_interval(5_000);
        p.early(MS);
        assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
        assert_eq!((p.now(), p.ended_on(FLIP)), (4 * MS, 1));
        // By the clock tick 1 is still ahead: another kind's event at it
        // has not been waited for and is 1 ms away.
        assert_eq!(p.next_tick(VBLANK, Duration::ZERO), (1, MS));
        // The flip's is tick 2.
        assert_eq!(p.next_tick(FLIP, Duration::ZERO), (2, 6 * MS));
        assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
        assert_eq!((p.now(), p.ended_on(FLIP)), (10 * MS, 2));
    }

    /// A thread that is not run for a while comes back from its wait late.
    /// The next flip wait then ends at once, on the latest tick passed
    /// (upstream's rule since 2026-11-25; before it, that wait went on to
    /// the next tick). The ticks slept through before that one are gone,
    /// flips still each take a later tick than the one before, the wait
    /// after the one that ended at once ends exactly on a tick again, and
    /// frames are still numbered one after another.
    #[test]
    fn a_wait_that_comes_back_late_ends_the_next_at_once_and_does_not_drift() {
        let p = Pacer::stepped();
        p.set_interval(5_000);
        // A frame of the title's loop, its flip wait coming back `late`.
        let frame = |late: Duration| {
            let (_, token) = p.wait(ORIGIN, 0, TOKEN_ON_TIMEOUT);
            p.late(late);
            assert_eq!(p.wait(FLIP, u32::MAX, 0), (S_OK, None));
            (token, p.ended_on(FLIP), p.now())
        };
        let on_time = Duration::ZERO;
        assert_eq!(frame(on_time), (Some(1), 1, 5 * MS));
        // Asked to sleep to 10 ms, run again at 22 ms: past ticks 3 and 4.
        assert_eq!(frame(12 * MS), (Some(2), 2, 22 * MS));
        // The next flip is tick 4 (at 20 ms, the latest passed), at once;
        // tick 3 is never handed out.
        assert_eq!(frame(on_time), (Some(3), 4, 22 * MS));
        assert_eq!(p.last_sleep(), Duration::ZERO);
        // The one after it is tick 5 at 25 ms, 3 ms away, and on time.
        assert_eq!(p.next_tick(FLIP, Duration::ZERO), (5, 3 * MS));
        assert_eq!(frame(on_time), (Some(4), 5, 25 * MS));
        // Late by less than an interval: no tick passed, so the next flip
        // is the very next tick, on time.
        assert_eq!(frame(2 * MS), (Some(5), 6, 32 * MS));
        assert_eq!(frame(on_time), (Some(6), 7, 35 * MS));
        assert_eq!(frame(on_time), (Some(7), 8, 40 * MS));
    }

    /// The device methods on the process's clock (the tick arithmetic is
    /// tested exactly on a stepped clock above). These are upstream's
    /// assertions, on its 50 ms ticks (a thread woken 15-30 ms late on a
    /// busy host, seen with 5 ms ticks, still makes its tick), with the
    /// fork's on top: frames are numbered one after another, a flip wait
    /// keeps the title's token, and a title may pass no token.
    #[test]
    fn the_device_methods_pace_on_the_real_clock() {
        let null = std::ptr::null();
        let list = std::ptr::null();
        assert_eq!(set_interval(null, null, 50_000, 1, 0), S_OK);
        assert_eq!(schedule(null, FLIP, 0, list, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, ORIGIN, 1_000, list, 0, &mut first), S_OK);
        assert_eq!(current(), first);
        let mut second = 0;
        assert_eq!(wait(null, ORIGIN, 1_000, list, 0, &mut second), S_OK);
        assert_eq!(second, first + 1);
        // The next tick is 50 ms ahead, so no time to wait is a time-out.
        assert_eq!(wait(null, ORIGIN, 0, list, 0, &mut second), TIMEOUT);
        assert_eq!(second, first + 1, "no frame, the token is left alone");
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(wait(null, ORIGIN, 0, list, flags, &mut early), TIMEOUT);
        assert_eq!(early, second + 1);
        // A title that passes no token still begins a frame.
        let none = std::ptr::null_mut();
        assert_eq!(wait(null, ORIGIN, 1_000, list, 0, none), S_OK);
        assert_eq!(current(), early + 1);
        assert!(PACER.last(ORIGIN).load(Ordering::Relaxed) >= 2);

        // The title's loop: an origin poll (which starts the frame), then
        // a blocking flip wait. Each flip takes its own tick.
        assert_eq!(set_interval(null, null, 50_000, 3, 0), S_OK);
        let mut token = 7;
        let flips: Vec<u64> = (0..4)
            .map(|_| {
                let _ = wait(null, ORIGIN, 0, list, TOKEN_ON_TIMEOUT, &mut token);
                let frame = token;
                assert_eq!(wait(null, FLIP, u32::MAX, list, 0, &mut token), S_OK);
                assert_eq!(token, frame, "a flip wait keeps the title's token");
                PACER.ended_on_flip()
            })
            .collect();
        assert!(flips.windows(2).all(|w| w[1] == w[0] + 1), "{flips:?}");
        assert_eq!(current(), early + 5);
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = PACER.next_tick(FLIP, Duration::ZERO);
        assert!(tick > flips[3]);
        // A flip wait begun after its tick passed (two ticks missed) ends at
        // once, on the latest tick passed; the next one waits for the next.
        std::thread::sleep(Duration::from_millis(110));
        let before = Instant::now();
        assert_eq!(wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token), S_OK);
        assert!(before.elapsed() < Duration::from_millis(10), "{:?}", before.elapsed());
        let late = PACER.ended_on_flip();
        assert!(late >= flips[3] + 2, "{late} after {}", flips[3]);
        assert_eq!(wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token), S_OK);
        assert_eq!(PACER.ended_on_flip(), late + 1);

        let mut count = 9;
        assert_eq!(
            statistics(null, 0, 0, &mut count, std::ptr::null_mut()),
            S_OK
        );
        assert_eq!(count, 0);
    }

    impl Pacer<Real> {
        fn ended_on_flip(&self) -> u64 {
            self.last(FLIP).load(Ordering::Relaxed)
        }
    }

    /// Two waits for one kind that begin together (two threads) take two
    /// ticks: the first takes its tick before it sleeps. A time-out takes
    /// none. The old code marked the tick after the sleep, so both got it.
    #[test]
    fn waits_begun_together_take_different_ticks() {
        let p = Pacer::stepped();
        let long = Duration::from_secs(1);
        let (first, _) = p.next_tick(VBLANK, long);
        let (second, _) = p.next_tick(VBLANK, long);
        assert!(second > first, "{second} after {first}");
        p.next_tick(VBLANK, Duration::ZERO);
        assert_eq!(p.ended_on(VBLANK), second);
    }
}
