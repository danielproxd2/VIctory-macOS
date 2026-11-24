//! What the crate's tests share: the host device behind the device tests'
//! lock ([`crate::test_support`], which other crates' tests use too), and
//! method slots looked up by an object's own layout.

use crate::object::This;
#[cfg(windows)]
use crate::test_support::Serial;
pub(crate) use crate::test_support::serial;

/// The host device with the device tests' lock held
/// ([`crate::test_support::gpu`]), for a test that needs a device; tests
/// keep the guard to their end. Without a device this is `None` after the
/// "skipped" line, and the test returns; with `VICTORY_REQUIRE_GPU=1` it
/// panics instead, which fails the test.
#[cfg(windows)]
pub(crate) fn gpu() -> Option<(Serial, &'static crate::host::Host)> {
    let guard = crate::test_support::gpu()?;
    crate::host::host().map(|host| (guard, host))
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
