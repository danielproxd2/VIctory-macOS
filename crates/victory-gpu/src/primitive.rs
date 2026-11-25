//! Console primitive types the host lacks. A rectangle list gives three
//! corners per rectangle, the second the one between the other two, and
//! the hardware adds the fourth at `v0 + v2 - v1` (Grand Theft Auto VI's
//! full-screen rectangles come as top left, bottom left, bottom right); on
//! the host each rectangle becomes two triangles, with the fourth vertex
//! made from the other three on the CPU.
//!
//! shortcut: the fourth vertex is computed from vertex inputs, not from
//! vertex shader outputs as the hardware does; that is the same for vertex
//! shaders linear in their inputs (blits, 2D quads). Inputs in formats
//! [`components`] does not know keep the first vertex's value, and so do
//! components past the end of the vertex (an element that reaches into the
//! next one, which the layout allows).
//!
//! shortcut: integer inputs (UINT and SINT) get the same sum in integers,
//! wrapping at their width, and signed normalized ones are clamped to
//! -1..1 as the unsigned ones are to 0..1. What the hardware does when the
//! fourth corner of such an input leaves its range has not been checked.
// Its user, the draw path, is Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

/// Expands `rects` rectangles of `stride`-byte vertices from `src` into six
/// vertices each in `dst`: `(v0, v1, v2)` and `(v0, v2, v3)`, so both
/// triangles turn the same way. `elements` are (byte offset, format).
pub(crate) fn expand_rects(src: &[u8], dst: &mut [u8], stride: usize, elements: &[(u32, u32)]) {
    if stride == 0 {
        return;
    }
    let (Some(in_rect), Some(out_rect)) = (stride.checked_mul(3), stride.checked_mul(6)) else {
        return;
    };
    for (rect, out) in src
        .chunks_exact(in_rect)
        .zip(dst.chunks_exact_mut(out_rect))
    {
        let v = |i: usize| &rect[i * stride..(i + 1) * stride];
        for (slot, i) in [0, 1, 2, 0, 2].into_iter().enumerate() {
            out[slot * stride..(slot + 1) * stride].copy_from_slice(v(i));
        }
        let fourth = &mut out[5 * stride..];
        fourth.copy_from_slice(v(0));
        for &(offset, format) in elements {
            let at = offset as usize;
            let Some((n, kind)) = components(format) else {
                continue;
            };
            // Only the components inside the vertex: the rest belong to the
            // next one.
            let inside = stride.saturating_sub(at) / kind.bytes();
            for c in 0..n.min(inside) {
                match kind {
                    Kind::Real(real) => {
                        let get = |i: usize| real.read(&v(i)[at..], c);
                        let value = get(0) + get(2) - get(1);
                        real.write(&mut fourth[at..], c, value);
                    }
                    Kind::Int(size) => {
                        let get = |i: usize| {
                            let mut b = [0u8; 4];
                            b[..size].copy_from_slice(&v(i)[at + c * size..][..size]);
                            u32::from_le_bytes(b)
                        };
                        // The low bytes of a 32-bit wrapping sum are the
                        // narrower type's wrapping sum, signed or not.
                        let value = get(0).wrapping_add(get(2)).wrapping_sub(get(1));
                        fourth[at + c * size..][..size]
                            .copy_from_slice(&value.to_le_bytes()[..size]);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    /// Computed as a float.
    Real(Real),
    /// UINT or SINT of this many bytes (1, 2 or 4), computed as an integer.
    Int(usize),
}

#[derive(Clone, Copy)]
enum Real {
    F32,
    F16,
    Unorm8,
    Unorm16,
    Snorm8,
    Snorm16,
}

/// Component count and kind of the vertex formats rectangles use.
fn components(format: u32) -> Option<(usize, Kind)> {
    Some(match format {
        2 => (4, Kind::Real(Real::F32)),
        6 => (3, Kind::Real(Real::F32)),
        16 => (2, Kind::Real(Real::F32)),
        41 => (1, Kind::Real(Real::F32)),
        10 => (4, Kind::Real(Real::F16)),
        34 => (2, Kind::Real(Real::F16)),
        54 => (1, Kind::Real(Real::F16)),
        11 => (4, Kind::Real(Real::Unorm16)),
        35 => (2, Kind::Real(Real::Unorm16)),
        56 => (1, Kind::Real(Real::Unorm16)),
        28 | 29 | 87 | 91 => (4, Kind::Real(Real::Unorm8)),
        49 => (2, Kind::Real(Real::Unorm8)),
        61 => (1, Kind::Real(Real::Unorm8)),
        13 => (4, Kind::Real(Real::Snorm16)),
        37 => (2, Kind::Real(Real::Snorm16)),
        58 => (1, Kind::Real(Real::Snorm16)),
        31 => (4, Kind::Real(Real::Snorm8)),
        51 => (2, Kind::Real(Real::Snorm8)),
        63 => (1, Kind::Real(Real::Snorm8)),
        // UINT and SINT of 32, 16 and 8 bits.
        3 | 4 => (4, Kind::Int(4)),
        7 | 8 => (3, Kind::Int(4)),
        17 | 18 => (2, Kind::Int(4)),
        42 | 43 => (1, Kind::Int(4)),
        12 | 14 => (4, Kind::Int(2)),
        36 | 38 => (2, Kind::Int(2)),
        57 | 59 => (1, Kind::Int(2)),
        30 | 32 => (4, Kind::Int(1)),
        50 | 52 => (2, Kind::Int(1)),
        62 | 64 => (1, Kind::Int(1)),
        _ => return None,
    })
}

impl Kind {
    fn bytes(self) -> usize {
        match self {
            Kind::Real(real) => real.bytes(),
            Kind::Int(size) => size,
        }
    }
}

impl Real {
    fn bytes(self) -> usize {
        match self {
            Real::F32 => 4,
            Real::F16 | Real::Unorm16 | Real::Snorm16 => 2,
            Real::Unorm8 | Real::Snorm8 => 1,
        }
    }

    fn read(self, at: &[u8], c: usize) -> f32 {
        let b = &at[c * self.bytes()..];
        match self {
            Real::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Real::F16 => half_to_f32(u16::from_le_bytes([b[0], b[1]])),
            Real::Unorm16 => f32::from(u16::from_le_bytes([b[0], b[1]])) / 65535.0,
            Real::Unorm8 => f32::from(b[0]) / 255.0,
            // The most negative value is -1 too.
            Real::Snorm16 => (f32::from(i16::from_le_bytes([b[0], b[1]])) / 32767.0).max(-1.0),
            Real::Snorm8 => (f32::from(b[0] as i8) / 127.0).max(-1.0),
        }
    }

    fn write(self, at: &mut [u8], c: usize, v: f32) {
        let n = self.bytes();
        let b = &mut at[c * n..(c + 1) * n];
        match self {
            Real::F32 => b.copy_from_slice(&v.to_le_bytes()),
            Real::F16 => b.copy_from_slice(&f32_to_half(v).to_le_bytes()),
            Real::Unorm16 => {
                b.copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes())
            }
            Real::Unorm8 => b[0] = (v.clamp(0.0, 1.0) * 255.0).round() as u8,
            Real::Snorm16 => {
                b.copy_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
            }
            Real::Snorm8 => b[0] = (v.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8,
        }
    }
}

fn half_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    let bits = match (exp, man) {
        (0, 0) => sign,
        // Subnormal: scale the mantissa.
        (0, m) => {
            let v = m as f32 * 2f32.powi(-24);
            return if sign != 0 { -v } else { v };
        }
        (31, m) => sign | 0x7f80_0000 | m << 13,
        (e, m) => sign | (e + 112) << 23 | m << 13,
    };
    f32::from_bits(bits)
}

fn f32_to_half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let man = bits & 0x7f_ffff;
    if exp >= 31 {
        return sign | 0x7c00; // too large: infinity
    }
    if exp <= 0 {
        if exp < -10 {
            return sign; // below half the smallest subnormal: zero
        }
        // Half subnormals are `m * 2^-24`. An f32 in this range is still
        // normal (exponent field >= 1), so its significand has the implicit
        // leading 1; shift it into the half mantissa, rounding to even.
        let sig = (1u32 << 23) | man;
        let shift = (14 - exp) as u32;
        let halfway = 1u32 << (shift - 1);
        let q = sig >> shift;
        let r = sig & ((1u32 << shift) - 1);
        let m = q + u32::from(r > halfway || (r == halfway && (q & 1) != 0));
        return sign | m as u16;
    }
    // Rounded to nearest: a carry out of the mantissa is added into the
    // exponent (`2 - 2^-12` is 2.0); at most it reaches infinity, 0x7c00.
    let half = (exp as u32) << 10;
    sign | (half + ((man + 0x1000) >> 13)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle of float2 positions and RGBA8 colors, given as top
    /// left, bottom left, bottom right: the fourth corner completes the
    /// parallelogram at the top right.
    #[test]
    fn rectangles_get_their_fourth_corner() {
        let vertex = |x: f32, y: f32, r: u8| {
            let mut v = Vec::new();
            v.extend(x.to_le_bytes());
            v.extend(y.to_le_bytes());
            v.extend([r, 0, 0, 255]);
            v
        };
        let src: Vec<u8> = [
            vertex(-1.0, 1.0, 0),
            vertex(-1.0, -1.0, 100),
            vertex(1.0, -1.0, 150),
        ]
        .concat();
        let mut dst = vec![0u8; 6 * 12];
        expand_rects(&src, &mut dst, 12, &[(0, 16), (8, 28)]);
        assert_eq!(dst[..36], src[..]);
        assert_eq!(dst[36..48], src[..12]);
        assert_eq!(dst[48..60], src[24..36]);
        assert_eq!(dst[60..], vertex(1.0, 1.0, 50)[..]);
        assert_eq!(f32_to_half(1.5), 0x3e00);
        assert_eq!(half_to_f32(0x3e00), 1.5);
        assert_eq!(half_to_f32(0xc000), -2.0);
    }

    /// A rounding that carries out of the mantissa raises the exponent
    /// (the carry used to be or-ed into an odd exponent, halving the value).
    #[test]
    fn halves_round_up_into_the_next_power_of_two() {
        assert_eq!(f32_to_half(2.0 - 2f32.powi(-12)), 0x4000);
        assert_eq!(f32_to_half(0.5 - 2f32.powi(-13)), 0x3800);
        assert_eq!(f32_to_half(65520.0), 0x7c00, "past the largest half");
        // An R16_FLOAT element at 1.0, 2^-12 and 1.0: the fourth corner is
        // 2 - 2^-12, which rounds to 2.0.
        let src: Vec<u8> = [0x3c00_u16, 0x0c00, 0x3c00]
            .iter()
            .flat_map(|h| h.to_le_bytes())
            .collect();
        let mut dst = vec![0u8; 6 * 2];
        expand_rects(&src, &mut dst, 2, &[(0, 54)]);
        assert_eq!(dst[10..], 0x4000_u16.to_le_bytes());
    }

    /// An element that reaches past the stride (float4 at 4 in 8-byte
    /// vertices, RGBA8 at the stride itself): the components inside get
    /// the fourth corner, the rest keep the first vertex's bytes. The old
    /// code indexed past the vertex and panicked.
    #[test]
    fn elements_past_the_stride_are_left_alone() {
        let vertex = |x: f32, y: f32| [x.to_le_bytes(), y.to_le_bytes()].concat();
        let src = [vertex(0.0, 2.0), vertex(1.0, 3.0), vertex(5.0, 7.0)].concat();
        let mut dst = vec![0u8; 6 * 8];
        expand_rects(&src, &mut dst, 8, &[(0, 16), (4, 2), (8, 28)]);
        assert_eq!(dst[..40], [&src[..], &src[..8], &src[16..]].concat()[..]);
        assert_eq!(dst[40..], vertex(4.0, 6.0)[..]);
    }

    /// One element of each integer and signed normalized format, its
    /// components taking the cases of its kind in turn. The fourth corner
    /// is `v0 + v2 - v1`: integers wrap at their width, signed or not, and
    /// signed normalized values are clamped to -1..1.
    #[test]
    fn integer_and_snorm_corners_are_computed() {
        // (v0, v1, v2, fourth) per component, as the raw little-endian value.
        type Cases = [[u32; 4]; 4];
        const INT8: Cases = [
            [10, 3, 20, 27],
            [250, 1, 10, 3],       // 259 wraps
            [0, 1, 0, 0xff],       // -1, or 255
            [0x80, 0, 0x7f, 0xff], // -128 + 127 = -1
        ];
        const INT16: Cases = [
            [1000, 1, 2000, 2999],
            [0xfff0, 0, 0x20, 0x10],
            [0, 1, 0, 0xffff],
            [0x8000, 0x7fff, 0x7fff, 0x8000],
        ];
        const INT32: Cases = [
            [100_000, 1, 200_000, 299_999],
            [0xffff_fff0, 0, 0x20, 0x10],
            [0, 1, 0, 0xffff_ffff],
            [0x8000_0000, 0, 0x7fff_ffff, 0xffff_ffff],
        ];
        const SNORM8: Cases = [
            [10, 20, 30, 20],
            [0, 0x81, 0x7f, 0x7f],    // 0 + 1 - -1 = 2, clamped to 1
            [0x40, 0x7f, 0x81, 0x81], // 64/127 - 1 - 1, clamped to -1
            [0x80, 0, 0, 0x81],       // -128 is -1, written as -127
        ];
        const SNORM16: Cases = [
            [1000, 2000, 3000, 2000],
            [0, 0x8001, 0x7fff, 0x7fff],
            [100, 0x7fff, 0x8001, 0x8001],
            [0x8000, 0, 0, 0x8001],
        ];
        // (format, components, bytes per component, cases)
        let formats: [(u32, usize, usize, &Cases); 26] = [
            (3, 4, 4, &INT32),    // R32G32B32A32_UINT
            (4, 4, 4, &INT32),    // R32G32B32A32_SINT
            (7, 3, 4, &INT32),    // R32G32B32_UINT
            (8, 3, 4, &INT32),    // R32G32B32_SINT
            (17, 2, 4, &INT32),   // R32G32_UINT
            (18, 2, 4, &INT32),   // R32G32_SINT
            (42, 1, 4, &INT32),   // R32_UINT
            (43, 1, 4, &INT32),   // R32_SINT
            (12, 4, 2, &INT16),   // R16G16B16A16_UINT
            (14, 4, 2, &INT16),   // R16G16B16A16_SINT
            (36, 2, 2, &INT16),   // R16G16_UINT
            (38, 2, 2, &INT16),   // R16G16_SINT
            (57, 1, 2, &INT16),   // R16_UINT
            (59, 1, 2, &INT16),   // R16_SINT
            (30, 4, 1, &INT8),    // R8G8B8A8_UINT
            (32, 4, 1, &INT8),    // R8G8B8A8_SINT
            (50, 2, 1, &INT8),    // R8G8_UINT
            (52, 2, 1, &INT8),    // R8G8_SINT
            (62, 1, 1, &INT8),    // R8_UINT
            (64, 1, 1, &INT8),    // R8_SINT
            (13, 4, 2, &SNORM16), // R16G16B16A16_SNORM
            (37, 2, 2, &SNORM16), // R16G16_SNORM
            (58, 1, 2, &SNORM16), // R16_SNORM
            (31, 4, 1, &SNORM8),  // R8G8B8A8_SNORM
            (51, 2, 1, &SNORM8),  // R8G8_SNORM
            (63, 1, 1, &SNORM8),  // R8_SNORM
        ];
        for (format, n, size, cases) in formats {
            // Vertex `i` and the expected fourth one (`i` = 3).
            let vertex = |i: usize| -> Vec<u8> {
                cases[..n]
                    .iter()
                    .flat_map(|case| case[i].to_le_bytes()[..size].to_vec())
                    .collect()
            };
            let stride = n * size;
            let src = [vertex(0), vertex(1), vertex(2)].concat();
            let mut dst = vec![0u8; 6 * stride];
            expand_rects(&src, &mut dst, stride, &[(0, format)]);
            assert_eq!(dst[..3 * stride], src[..], "format {format}");
            assert_eq!(dst[5 * stride..], vertex(3)[..], "format {format}");
        }
        // With the later cases first in a one-component format, so that
        // every case of a kind is computed in some format.
        for (format, size, cases) in [
            (42, 4, &INT32),
            (57, 2, &INT16),
            (62, 1, &INT8),
            (58, 2, &SNORM16),
            (63, 1, &SNORM8),
        ] {
            for case in cases {
                let bytes = |i: usize| case[i].to_le_bytes()[..size].to_vec();
                let src = [bytes(0), bytes(1), bytes(2)].concat();
                let mut dst = vec![0u8; 6 * size];
                expand_rects(&src, &mut dst, size, &[(0, format)]);
                assert_eq!(dst[5 * size..], bytes(3)[..], "format {format} {case:x?}");
            }
        }
    }

    #[test]
    fn normalized_attributes_are_interpolated_in_their_own_range() {
        let vertex = |rg: [u8; 2], u16_value: u16, u8_value: u8, snorm8: i8, snorm16: i16| {
            [
                &rg[..],
                &u16_value.to_le_bytes(),
                &[u8_value, snorm8 as u8],
                &snorm16.to_le_bytes(),
            ]
            .concat()
        };
        let src = [
            vertex([3, 250], 1000, 10, i8::MIN, -23000),
            vertex([8, 200], 500, 3, -120, -18000),
            vertex([20, 253], 10000, 100, -120, -32000),
        ]
        .concat();
        let mut dst = [0; 6 * 8];
        expand_rects(
            &src,
            &mut dst,
            8,
            &[(0, 49), (2, 56), (4, 61), (5, 63), (6, 58)],
        );
        assert_eq!(dst[40..], vertex([15, 255], 10500, 107, -127, -32767));
    }

    /// `half_to_f32` reads subnormals, so an f16 rectangle output must be
    /// able to write them back instead of flushing every one to zero.
    #[test]
    fn half_subnormals_convert_in_both_directions() {
        assert_eq!(f32_to_half(2f32.powi(-24)), 0x0001);
        assert_eq!(f32_to_half(-2f32.powi(-24)), 0x8001);
        assert_eq!(f32_to_half(2f32.powi(-23)), 0x0002);
        assert_eq!(f32_to_half(1023.0 * 2f32.powi(-24)), 0x03ff);
        // Ties go to an even mantissa: 0/1 and 1/2.
        assert_eq!(f32_to_half(2f32.powi(-25)), 0x0000);
        assert_eq!(f32_to_half(3.0 * 2f32.powi(-25)), 0x0002);
        // Below half the smallest subnormal, and normal values, are unchanged.
        assert_eq!(f32_to_half(2f32.powi(-26)), 0x0000);
        assert_eq!(f32_to_half(1.5), 0x3e00);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
    }

    #[test]
    fn zero_or_overflowing_stride_is_ignored_without_panicking() {
        let mut dst = [0xAAu8; 16];
        expand_rects(&[1, 2, 3], &mut dst, 0, &[(0, 16)]);
        assert_eq!(dst, [0xAAu8; 16]);
        expand_rects(&[1, 2, 3], &mut dst, usize::MAX / 2, &[(0, 16)]);
        assert_eq!(dst, [0xAAu8; 16]);
    }

    #[test]
    fn unorm16_unorm8_clamping_and_unknown_formats_expand_correctly() {
        // Vertex layout (12 bytes):
        //   0..4: R16G16_UNORM (format 35)
        //   4..8: R8G8B8A8_UNORM (format 28) — tests clamping below 0 and above 1
        //   8..12: unknown format (999) — must keep v0's bytes verbatim
        let v = |u0: u16, u1: u16, c: [u8; 4], raw: [u8; 4]| {
            let mut b = Vec::with_capacity(12);
            b.extend(u0.to_le_bytes());
            b.extend(u1.to_le_bytes());
            b.extend(c);
            b.extend(raw);
            b
        };
        let src = [
            v(
                10_000,
                50_000,
                [10, 200, 100, 255],
                [0xDE, 0xAD, 0xBE, 0xEF],
            ),
            v(20_000, 10_000, [100, 50, 100, 255], [1, 2, 3, 4]),
            v(30_000, 20_000, [20, 200, 100, 255], [5, 6, 7, 8]),
        ]
        .concat();
        let mut dst = vec![0u8; 6 * 12];
        expand_rects(&src, &mut dst, 12, &[(0, 35), (4, 28), (8, 999)]);
        let fourth = &dst[60..72];
        // u0: 10000 + 30000 - 20000 = 20000; u1: 50000 + 20000 - 10000 = 60000
        assert_eq!(u16::from_le_bytes([fourth[0], fourth[1]]), 20_000);
        assert_eq!(u16::from_le_bytes([fourth[2], fourth[3]]), 60_000);
        // c0: 10 + 20 - 100 -> clamped to 0; c1: 200 + 200 - 50 -> clamped to 255
        assert_eq!(&fourth[4..8], &[0, 255, 100, 255]);
        // Unknown format keeps v0 bytes
        assert_eq!(&fourth[8..12], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn multiple_rectangles_and_f32_f16_vector_formats_expand_in_sequence() {
        // Two rectangles of R32G32B32_FLOAT (format 6, 12 bytes) + R16G16_FLOAT (format 34, 4 bytes) = 16 bytes
        let v = |p: [f32; 3], uv: [f32; 2]| {
            let mut b = Vec::with_capacity(16);
            for x in p {
                b.extend(x.to_le_bytes());
            }
            for x in uv {
                b.extend(f32_to_half(x).to_le_bytes());
            }
            b
        };
        let src = [
            // Rect 0
            v([0.0, 1.0, 0.5], [0.0, 0.0]),
            v([0.0, 0.0, 0.5], [0.0, 1.0]),
            v([2.0, 0.0, 0.5], [1.0, 1.0]),
            // Rect 1
            v([10.0, 20.0, 1.0], [0.25, 0.5]),
            v([10.0, 10.0, 1.0], [0.25, 1.0]),
            v([14.0, 10.0, 1.0], [0.75, 1.0]),
        ]
        .concat();
        let mut dst = vec![0u8; 12 * 16];
        expand_rects(&src, &mut dst, 16, &[(0, 6), (12, 34)]);
        assert_eq!(&dst[5 * 16..6 * 16], &v([2.0, 1.0, 0.5], [1.0, 0.0])[..]);
        assert_eq!(
            &dst[11 * 16..12 * 16],
            &v([14.0, 20.0, 1.0], [0.75, 0.5])[..]
        );
    }
}
