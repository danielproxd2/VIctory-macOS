//! Address and page arithmetic of console memory. No host calls, so it
//! builds and is tested everywhere.

/// XMem flag bit of 32-bit graphics memory.
pub(crate) const ADDRESS_32BIT: u64 = 4;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (Grand Theft
/// Auto VI's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
pub(crate) fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

/// A physical page: 64 KiB.
pub(crate) const PAGE: usize = 64 << 10;

/// The length in bytes of `n` pages at `base`, when `[base, base + n pages)`
/// fits the address space. The count is the title's: unchecked, a large one
/// wraps into a short range somewhere else.
pub(crate) fn page_span(base: usize, n: usize) -> Option<usize> {
    let len = n.checked_mul(PAGE)?;
    base.checked_add(len)?;
    Some(len)
}

/// `[base, base + len)` as addresses, when both ends are some.
pub(crate) fn byte_span(base: u64, len: u64) -> Option<(usize, usize)> {
    let (base, len) = (usize::try_from(base).ok()?, usize::try_from(len).ok()?);
    base.checked_add(len)?;
    Some((base, len))
}

/// Whether `n` elements of `size` bytes at `at` can be an array at all: no
/// object is larger than `isize::MAX` bytes or wraps the address space.
/// Says nothing about the memory being there.
pub(crate) fn array_fits(at: usize, n: usize, size: usize) -> bool {
    n.checked_mul(size)
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .is_some_and(|bytes| at.checked_add(bytes).is_some())
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. `slice_of` looks a page up; `follows`
/// says whether a slice comes right after a run (its first slice and
/// length). `None` if a page is unknown or `pages` ends early.
pub(crate) fn runs<'a, S: Clone + 'a>(
    pages: &[usize],
    offset: usize,
    len: usize,
    slice_of: impl Fn(&usize) -> Option<&'a S>,
    follows: impl Fn(&S, usize, &S) -> bool,
) -> Option<Vec<(S, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let mut runs: Vec<(S, usize)> = Vec::new();
    for page in pages {
        let slice = slice_of(page)?;
        match runs.last_mut() {
            Some((first, n)) if follows(first, *n, slice) => {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const GIB: usize = 1 << 30;

    #[test]
    fn address_range_fields_1_to_7() {
        // The field at bit 12 counts down from 46 address bits.
        for (field, bits) in [
            (1u64, 46),
            (2, 45),
            (3, 44),
            (4, 43),
            (5, 42),
            (6, 41),
            (7, 40),
        ] {
            assert_eq!(
                address_range(field << 12),
                (4 * GIB, (1usize << bits) - 1),
                "field {field}"
            );
        }
        // No field: 40 bits.
        assert_eq!(address_range(0), (4 * GIB, (1 << 40) - 1));
    }

    #[test]
    fn address_range_reads_only_its_own_bits() {
        // The memory-type bits below and whatever lies above the field.
        assert_eq!(address_range(3 | 5 << 12), address_range(5 << 12));
        assert_eq!(address_range(1 << 15 | 1 << 33), address_range(0));
        assert_eq!(address_range(0xff << 15 | 2 << 12), address_range(2 << 12));
    }

    #[test]
    fn address_range_of_32bit_memory_is_the_low_4_gib() {
        let low = (0, u32::MAX as usize);
        assert_eq!(address_range(ADDRESS_32BIT), low);
        // The address field does not widen it.
        for field in 1..=7u64 {
            assert_eq!(address_range(ADDRESS_32BIT | field << 12), low);
        }
        assert_eq!(address_range(ADDRESS_32BIT | 3), low);
    }

    #[test]
    fn page_spans_refuse_counts_and_ends_that_wrap() {
        assert_eq!(page_span(0x1_0000, 0), Some(0));
        assert_eq!(page_span(0x1_0000, 3), Some(3 * PAGE));
        // The last page of the address space ends at its end, not past it.
        let top = usize::MAX - PAGE + 1;
        assert_eq!(page_span(top - PAGE, 1), Some(PAGE));
        assert_eq!(page_span(top, 1), None);
        assert_eq!(page_span(top, 0), Some(0));
        // The smallest count whose bytes wrap to 0, and its neighbours.
        let wraps = usize::MAX / PAGE + 1;
        assert_eq!(page_span(0, wraps), None);
        assert_eq!(page_span(0x1_0000, wraps + 2), None);
        assert_eq!(page_span(0, wraps - 1), Some(usize::MAX - PAGE + 1));
        assert_eq!(page_span(PAGE, wraps - 1), None);
        assert_eq!(page_span(0x1_0000, usize::MAX), None);
    }

    #[test]
    fn byte_spans_refuse_ends_that_wrap() {
        assert_eq!(byte_span(0x1000, 0x2000), Some((0x1000, 0x2000)));
        assert_eq!(byte_span(0, 0), Some((0, 0)));
        assert_eq!(byte_span(u64::MAX, 0), Some((usize::MAX, 0)));
        assert_eq!(byte_span(u64::MAX, 1), None);
        assert_eq!(byte_span(u64::MAX - PAGE as u64, 2 * PAGE as u64), None);
        assert_eq!(byte_span(1, u64::MAX), None);
    }

    #[test]
    fn arrays_fit_only_below_the_object_size_limit() {
        assert!(array_fits(0x1000, 0, 8));
        assert!(array_fits(0x1000, 16, 8));
        assert!(array_fits(0, isize::MAX as usize / 8, 8));
        // More bytes than an object may have, or than a count can say.
        assert!(!array_fits(0, isize::MAX as usize / 8 + 1, 8));
        assert!(!array_fits(0x1000, usize::MAX / 8 + 1, 8));
        assert!(!array_fits(0x1000, usize::MAX, 8));
        // An end past the last address.
        assert!(!array_fits(usize::MAX - 15, 3, 8));
        assert!(array_fits(usize::MAX - 16, 2, 8));
    }

    /// A stand-in for a slice of a section: which section, which page of it.
    type Slice = (char, usize);

    /// Run lengths (and each run's first slice) over a page table where
    /// pages 0x10-0x12 are slices 0-2 of section `a`, 0x20 is slice 0 of
    /// `b`, and 0x30 is slice 1 of `b`.
    fn lengths(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
        let table: HashMap<usize, Slice> = [
            (0x10, ('a', 0)),
            (0x11, ('a', 1)),
            (0x12, ('a', 2)),
            (0x20, ('b', 0)),
            (0x30, ('b', 1)),
        ]
        .into();
        runs(
            pages,
            offset,
            len,
            |page| table.get(page),
            |first, n, next| first.0 == next.0 && next.1 == first.1 + n,
        )
    }

    #[test]
    fn runs_merge_adjacent_pages() {
        let a = [0x10, 0x11, 0x12];
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![(('a', 0), 3)]));
        // Pages of one section that follow on, though not allocated
        // together.
        assert_eq!(
            lengths(&[0x20, 0x30], 0, 2 * PAGE),
            Some(vec![(('b', 0), 2)])
        );
        // Out of order, or across sections: one run each.
        assert_eq!(
            lengths(&[0x10, 0x12, 0x11], 0, 3 * PAGE),
            Some(vec![(('a', 0), 1), (('a', 2), 1), (('a', 1), 1)])
        );
        assert_eq!(
            lengths(&[0x11, 0x12, 0x20], 0, 3 * PAGE),
            Some(vec![(('a', 1), 2), (('b', 0), 1)])
        );
        // A slice that follows on in another section starts a run.
        assert_eq!(
            lengths(&[0x20, 0x11], 0, 2 * PAGE),
            Some(vec![(('b', 0), 1), (('a', 1), 1)])
        );
        // The same page twice does not follow on from itself.
        assert_eq!(
            lengths(&[0x10, 0x10], 0, 2 * PAGE),
            Some(vec![(('a', 0), 1), (('a', 0), 1)])
        );
    }

    #[test]
    fn runs_cover_only_the_pages_the_bytes_reach() {
        let a = [0x10, 0x11, 0x12];
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![(('a', 1), 1)]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![(('a', 0), 2)]));
        assert_eq!(lengths(&a, 2 * PAGE, PAGE), Some(vec![(('a', 2), 1)]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        assert_eq!(lengths(&a, 3 * PAGE, 0), Some(vec![]));
        // A page the bytes do not reach may be unknown.
        assert_eq!(lengths(&[0x10, 0x99], 0, PAGE), Some(vec![(('a', 0), 1)]));
    }

    #[test]
    fn runs_refuse_unknown_pages_and_ranges_past_the_end() {
        let a = [0x10, 0x11, 0x12];
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&a, 3 * PAGE, 1), None);
        assert_eq!(lengths(&a, 4 * PAGE, 0), None);
        assert_eq!(lengths(&[0x10, 0x99], 0, 2 * PAGE), None);
        assert_eq!(lengths(&[], 0, 1), None);
        // The end of the range does not fit an address.
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(lengths(&a, 1, usize::MAX), None);
    }
}
