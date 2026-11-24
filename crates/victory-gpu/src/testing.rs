//! What the crate's tests share: one lock for every test that reaches the
//! host device, and method slots looked up by an object's own layout.

use std::sync::{Mutex, MutexGuard};

use crate::object::This;

/// One device test at a time. The host device is one per process, and
/// victory-shader's device tests found that several test threads on one
/// adapter now and then leave every dispatch unfinished (the test binary
/// hangs) or end the process with an access violation; a host failure here
/// ends the process as well (`host::fail`).
pub(crate) static GPU_TESTS: Mutex<()> = Mutex::new(());

/// Takes [`GPU_TESTS`]. A test that failed while holding it has poisoned
/// it, which must not fail every device test after it too.
pub(crate) fn serial() -> MutexGuard<'static, ()> {
    GPU_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The host device with [`GPU_TESTS`] held, for a test that needs a
/// device; `None` without one. Tests keep the guard to their end.
#[cfg(windows)]
pub(crate) fn gpu() -> Option<(MutexGuard<'static, ()>, &'static crate::host::Host)> {
    let guard = serial();
    Some((guard, crate::host::host()?))
}

/// The list method that older lists have at `old_offset`, wherever
/// `list`'s own layout has it. Lists are made in the layout the device was
/// last created for (`device::newer`), which another test may switch for
/// the whole test binary at any time, so the object's table decides.
pub(crate) fn list_slot<F: Copy>(list: This, old_offset: usize) -> F {
    let slot = old_offset / 8;
    let slot = if crate::list::is_newer(list) {
        crate::list::newer_slot(slot)
    } else {
        slot
    };
    crate::object::slot(list, slot * 8)
}

/// As [`list_slot`], for a queue.
pub(crate) fn queue_slot<F: Copy>(queue: This, old_offset: usize) -> F {
    let slot = old_offset / 8;
    let slot = if crate::queue::is_newer(queue) {
        crate::queue::newer_slot(slot)
    } else {
        slot
    };
    crate::object::slot(queue, slot * 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[test]
    fn the_gpu_lock_survives_a_panicking_holder() {
        let holder = std::thread::spawn(|| {
            let _held = GPU_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            // Unwinds as a failed assertion does, without its message.
            std::panic::resume_unwind(Box::new("a device test failed"));
        });
        assert!(holder.join().is_err());
        // The holder died with the lock, and the next test still gets it.
        assert!(GPU_TESTS.is_poisoned());
        drop(serial());
        drop(serial());
    }

    /// ExecuteIndirect is at 0x1d8 in older lists and 0x1e8 in newer ones,
    /// a queue's Signal at 0x70 and 0x80: asked for by the older offset,
    /// both are found in either layout.
    #[test]
    fn slots_follow_the_objects_own_layout() {
        type Method = *const ();
        let (old, execute_indirect) = crate::list::test_layout(false);
        let (new, _) = crate::list::test_layout(true);
        let (old, new): (This, This) = (&old, &new);
        assert_eq!(list_slot::<Method>(old, 0x1d8), execute_indirect);
        assert_eq!(list_slot::<Method>(new, 0x1d8), execute_indirect);
        assert_eq!(slot::<Method>(new, 0x1e8), execute_indirect);
        assert_ne!(slot::<Method>(new, 0x1d8), execute_indirect);
        // Before the added methods nothing moved (Close, 0x48).
        assert_eq!(list_slot::<Method>(new, 0x48), slot::<Method>(old, 0x48));

        let (old, signal) = crate::queue::test_layout(false);
        let (new, _) = crate::queue::test_layout(true);
        let (old, new): (This, This) = (&old, &new);
        assert_eq!(queue_slot::<Method>(old, 0x70), signal);
        assert_eq!(queue_slot::<Method>(new, 0x70), signal);
        assert_eq!(slot::<Method>(new, 0x80), signal);
        assert_ne!(slot::<Method>(new, 0x70), signal);
        // Submission stays at 0x50.
        assert_eq!(queue_slot::<Method>(new, 0x50), slot::<Method>(old, 0x50));
    }
}
