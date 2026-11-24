//! Arithmetic on resource states, formats and address ranges that the host
//! side needs and that asks nothing of the host: it builds, and is tested,
//! on every target.
// Its users are in the Windows-only modules.
#![cfg_attr(not(windows), allow(dead_code))]

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// `state` without the bits a host texture with resource flags `flags`
/// cannot be in: the console lets the title name states its own resources
/// allow (unordered access on a depth target, whose host copy cannot have
/// both).
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`. A range that would pass the end of the address space ends
/// there, and a `step` of zero cuts nothing (one piece).
pub(crate) fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start.saturating_add(len);
    let mut at = start;
    while at < end {
        let next = match step {
            0 => end,
            // The boundary after the last piece may itself be past the end
            // of the address space.
            _ => (at / step + 1)
                .checked_mul(step)
                .map_or(end, |n| n.min(end)),
        };
        f(at, next - at);
        at = next;
    }
}

/// The swap chain format (a DXGI format number) that a presented texture
/// of `format` can be copied into: its own family's. `None` for every
/// other format.
pub(crate) fn swap_format(format: u32) -> Option<u32> {
    const R8G8B8A8_UNORM: u32 = 28;
    const B8G8R8A8_UNORM: u32 = 87;
    const R10G10B10A2_UNORM: u32 = 24;
    const R16G16B16A16_FLOAT: u32 = 10;
    Some(match format {
        27..=32 => R8G8B8A8_UNORM,
        87 | 90 | 91 => B8G8R8A8_UNORM,
        23..=25 => R10G10B10A2_UNORM,
        9..=14 => R16G16B16A16_FLOAT,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_states_lose_buffer_only_bits() {
        // The console's bits from 25 up go, for buffers and textures.
        assert_eq!(pc_state(u32::MAX), 0x01ff_ffff);
        assert_eq!(pc_state(1 << 25 | 0x4), 0x4);
        // Generic read: vertex and constant buffer, index buffer, both
        // shader resource states, indirect argument, copy source.
        const GENERIC_READ: u32 = 0x1 | 0x2 | 0x40 | 0x80 | 0x200 | 0x800;
        assert_eq!(pc_state(GENERIC_READ), GENERIC_READ);
        assert_eq!(texture_state(GENERIC_READ), 0x40 | 0x80 | 0x800);
        // Stream out is a buffer's too; everything else stays.
        assert_eq!(texture_state(0x100), 0);
        assert_eq!(texture_state(u32::MAX), 0x01ff_fcfc);
        assert_eq!(texture_state(1 << 30 | 0x4), 0x4);
    }

    #[test]
    fn legal_states_follow_the_resource_flags() {
        const TARGET: u32 = 0x4;
        const UNORDERED: u32 = 0x8;
        const DEPTH: u32 = 0x10 | 0x20;
        const SHADER: u32 = 0x40 | 0x80;
        const COPY: u32 = 0x400 | 0x800;
        let all = TARGET | UNORDERED | DEPTH | SHADER | COPY;
        for flags in 0..16u32 {
            let mut want = COPY;
            if flags & 0x1 != 0 {
                want |= TARGET; // ALLOW_RENDER_TARGET
            }
            if flags & 0x2 != 0 {
                want |= DEPTH; // ALLOW_DEPTH_STENCIL
            }
            if flags & 0x4 != 0 {
                want |= UNORDERED; // ALLOW_UNORDERED_ACCESS
            }
            if flags & 0x8 == 0 {
                want |= SHADER; // not DENY_SHADER_RESOURCE
            }
            assert_eq!(legal_for(flags, all), want, "flags {flags:#x}");
            // Nothing is added, and the common state stays what it is.
            assert_eq!(legal_for(flags, 0), 0, "flags {flags:#x}");
            assert_eq!(legal_for(flags, COPY), COPY, "flags {flags:#x}");
        }
    }

    fn pieces(start: usize, len: usize, step: usize) -> Vec<(usize, usize)> {
        let mut all = Vec::new();
        for_pieces(start, len, step, |at, n| all.push((at, n)));
        all
    }

    #[test]
    fn pieces_cut_at_step_boundaries() {
        const PAGE: usize = 0x1000;
        // From the middle of a page, over two boundaries.
        assert_eq!(
            pieces(0x1800, 0x2000, PAGE),
            [(0x1800, 0x800), (0x2000, PAGE), (0x3000, 0x800)]
        );
        assert_eq!(pieces(0x1800, 0, PAGE), []);
        assert_eq!(pieces(0, 0, PAGE), []);
        // Shorter than a step: one piece, also when it ends on a boundary.
        assert_eq!(pieces(0x1800, 0x10, PAGE), [(0x1800, 0x10)]);
        assert_eq!(pieces(0x1800, 0x800, PAGE), [(0x1800, 0x800)]);
        // Exact multiples.
        assert_eq!(pieces(0x2000, PAGE, PAGE), [(0x2000, PAGE)]);
        assert_eq!(
            pieces(0, 3 * PAGE, PAGE),
            [(0, PAGE), (PAGE, PAGE), (2 * PAGE, PAGE)]
        );
        // No step: the range as it is.
        assert_eq!(pieces(0x1800, 0x2000, 0), [(0x1800, 0x2000)]);
        assert_eq!(pieces(0x1800, 0, 0), []);
    }

    /// The end of the range used to be `start + len`, which wraps in a
    /// release build (no pieces at all) and panics in a debug build.
    #[test]
    fn pieces_near_the_top_of_memory_do_not_wrap() {
        let start = usize::MAX - 10;
        assert_eq!(pieces(start, 100, 64 << 10), [(start, 10)]);
        // usize::MAX - 10 is one past a multiple of four.
        assert_eq!(
            pieces(start, 100, 4),
            [(start, 3), (start + 3, 4), (start + 7, 3)]
        );
        assert_eq!(pieces(start, 100, 0), [(start, 10)]);
        assert_eq!(pieces(usize::MAX, usize::MAX, 4 << 10), []);
        // A step that no multiple of fits below the end.
        assert_eq!(pieces(8, 16, usize::MAX), [(8, 16)]);
    }

    #[test]
    fn every_format_has_a_swap_family_or_none() {
        for format in 0..=132u32 {
            let family = |ids: &[u32], to: u32| ids.contains(&format).then_some(to);
            let want = family(&[27, 28, 29, 30, 31, 32], 28) // R8G8B8A8
                .or(family(&[87, 90, 91], 87)) // B8G8R8A8
                .or(family(&[23, 24, 25], 24)) // R10G10B10A2
                .or(family(&[9, 10, 11, 12, 13, 14], 10)); // R16G16B16A16
            assert_eq!(swap_format(format), want, "format {format}");
        }
        // A format number that is not one (a negative DXGI_FORMAT).
        assert_eq!(swap_format(u32::MAX), None);
    }
}
