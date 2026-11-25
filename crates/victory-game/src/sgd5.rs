//! `SGD5`: the engine's shader file. Each record holds one shader container,
//! deflate-packed. The header tables are not decoded yet, so records are
//! found by their marker and cross-checked by their sizes:
//!
//! ```text
//! u8 name length (with NUL), name, NUL, u8 kind,
//! u32 n, u32 0x000DDCE7, u32 n, u32 unpacked size, raw deflate (n - 12 bytes)
//! ```

pub const MAGIC: &[u8; 4] = b"SGD5";
const MARKER: [u8; 4] = 0x000D_DCE7_u32.to_le_bytes();
/// Containers are at most a few MiB; refuse to allocate for absurd sizes.
const MAX_UNPACKED: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// Offset of the marker in the file.
    pub offset: usize,
    /// Like `PS_<name>_<hash>_Wrapped`; `None` if the bytes before the
    /// record do not read as one.
    pub name: Option<&'a str>,
    /// Record type, meaning unknown (0x32 most shaders, 0x41 mesh shaders
    /// and some material families, 0x3c compute, 0x40, 0x42).
    pub kind: u8,
    packed: &'a [u8],
    unpacked_size: usize,
}

impl Record<'_> {
    /// The container, or `None` if the stream is corrupt.
    pub fn unpack(&self) -> Option<Vec<u8>> {
        miniz_oxide::inflate::decompress_to_vec_with_limit(self.packed, self.unpacked_size)
            .ok()
            .filter(|out| out.len() == self.unpacked_size)
    }
}

/// An SGD5 file, or a file holding one (an archive: records are found by
/// their marker, so no index is needed).
pub fn is_sgd5(file: &[u8]) -> bool {
    memchr::memmem::find(file, MAGIC).is_some()
}

/// Every record in an SGD5 file, in file order.
pub fn records(file: &[u8]) -> impl Iterator<Item = Record<'_>> {
    memchr::memmem::find_iter(file, &MARKER).filter_map(|at| record(file, at))
}

fn record(file: &[u8], at: usize) -> Option<Record<'_>> {
    let u32_at = |o: usize| {
        file.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
    };
    let n = u32_at(at.checked_sub(4)?)?;
    let unpacked_size = u32_at(at + 8)?;
    if u32_at(at + 4)? != n || n < 12 || unpacked_size > MAX_UNPACKED {
        return None;
    }
    let nul = at.checked_sub(6)?;
    Some(Record {
        offset: at,
        name: (file[nul] == 0).then(|| name_before(file, nul)).flatten(),
        kind: file[at - 5],
        packed: file.get(at + 12..at + n)?,
        unpacked_size,
    })
}

/// The printable name that ends at `nul` and follows its length byte.
fn name_before(file: &[u8], nul: usize) -> Option<&str> {
    let run = file[..nul]
        .iter()
        .rposition(|b| !b.is_ascii_graphic())
        .map_or(0, |p| p + 1);
    // From 32 characters on, the length byte is printable too.
    (run.saturating_sub(1)..nul)
        .find(|&l| usize::from(file[l]) == nul - l)
        .and_then(|l| std::str::from_utf8(&file[l + 1..nul]).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_bytes(name: &str, kind: u8, payload: &[u8]) -> Vec<u8> {
        let packed = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let n = (packed.len() + 12) as u32;
        let mut out = vec![0xF4, 0x01, 0, 0, name.len() as u8 + 1];
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&[0, kind]);
        for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&packed);
        out
    }

    #[test]
    fn reads_records() {
        let long = format!("PS_{}_Wrapped", "x".repeat(40));
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(&[1, 2, 3, 4]);
        file.extend(record_bytes(
            "VS_Blit_0011aabb_Wrapped",
            0x32,
            b"first container",
        ));
        // A marker whose sizes disagree is not a record.
        file.extend_from_slice(&[9, 0, 0, 0, 0xE7, 0xDC, 0x0D, 0, 8, 0, 0, 0]);
        file.extend(record_bytes(&long, 0x41, &[7; 300]));

        assert!(is_sgd5(&file));
        let found: Vec<_> = records(&file).collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, Some("VS_Blit_0011aabb_Wrapped"));
        assert_eq!(found[0].kind, 0x32);
        assert_eq!(found[0].unpack().as_deref(), Some(&b"first container"[..]));
        assert_eq!(found[1].name, Some(long.as_str()));
        assert_eq!(found[1].unpack(), Some(vec![7; 300]));

        // The same file inside an archive.
        let mut archive = vec![0x5A; 0x1000];
        archive.extend_from_slice(&file);
        assert!(is_sgd5(&archive));
        assert_eq!(records(&archive).count(), 2);
        assert!(!is_sgd5(&archive[..0x1000]));
    }

    fn xorshift(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// The fields around a marker: `n` twice, the unpacked size, `packed`.
    fn raw_record(before: &[u8], n: [u32; 2], unpacked: u32, packed: &[u8]) -> Vec<u8> {
        let mut out = before.to_vec();
        for v in [n[0], 0x000D_DCE7, n[1], unpacked] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(packed);
        out
    }

    /// Walks every record a file gives and unpacks it: whatever comes out
    /// has the size the record states, within the limit.
    fn walk(file: &[u8]) -> usize {
        let mut found = 0;
        for r in records(file) {
            found += 1;
            assert_eq!(&file[r.offset..r.offset + 4], &MARKER);
            if let Some(out) = r.unpack() {
                assert_eq!(out.len(), r.unpacked_size);
                assert!(out.len() <= MAX_UNPACKED);
            }
        }
        found
    }

    #[test]
    fn noise_around_the_marker_never_panics_or_over_allocates() {
        let packed = miniz_oxide::deflate::compress_to_vec(&[7; 64], 6);
        let n = packed.len() as u32 + 12;

        // The marker at every offset too near the start for the fields
        // before it, and the file cut at every length.
        for lead in 0..12 {
            let mut file = vec![0x41u8; lead];
            file.extend_from_slice(&MARKER);
            file.extend_from_slice(&n.to_le_bytes());
            file.extend_from_slice(&64u32.to_le_bytes());
            file.extend_from_slice(&packed);
            for cut in 0..=file.len() {
                walk(&file[..cut]);
            }
            if lead < 6 {
                assert_eq!(walk(&file), 0, "no room for a record before offset 6");
            }
        }
        let whole = record_bytes("PS_x_Wrapped", 0x32, &[7; 64]);
        assert_eq!(walk(&whole), 1);
        for cut in 0..whole.len() {
            assert_eq!(walk(&whole[..cut]), 0, "cut at {cut}");
        }

        // Sizes at the edges: a packed size smaller than its own fields,
        // one with nothing packed, and the largest a u32 holds.
        let before = [0u8, 0x32];
        for (n, records) in [(11, 0), (12, 1), (u32::MAX, 0)] {
            let file = raw_record(&before, [n, n], 64, &packed);
            assert_eq!(walk(&file), records, "n = {n}");
        }
        let empty = raw_record(&before, [12, 12], 0, &[]);
        assert_eq!(walk(&empty), 1);
        assert_eq!(super::records(&empty).next().unwrap().unpack(), None);

        // Unpacked sizes around the limit. At the limit the record stands
        // but its 64 bytes are not what it states, so nothing unpacks;
        // past it there is no record.
        let max = MAX_UNPACKED as u32;
        for (unpacked, records) in [(max - 1, 1), (max, 1), (max + 1, 0), (u32::MAX, 0)] {
            let file = raw_record(&before, [n, n], unpacked, &packed);
            assert_eq!(walk(&file), records, "unpacked = {unpacked:#x}");
            assert!(super::records(&file).all(|r| r.unpack().is_none()));
        }

        // A stream that unpacks to far more than the record states stops
        // at the stated size.
        let bomb = miniz_oxide::deflate::compress_to_vec(&vec![0; 4 << 20], 6);
        assert!(bomb.len() < 64 << 10);
        let n = bomb.len() as u32 + 12;
        for unpacked in [0, 1, 1000, (4 << 20) - 1, (4 << 20) + 1, max] {
            let file = raw_record(&before, [n, n], unpacked, &bomb);
            assert_eq!(walk(&file), 1);
            assert_eq!(super::records(&file).next().unwrap().unpack(), None);
        }
        let file = raw_record(&before, [n, n], 4 << 20, &bomb);
        assert_eq!(
            super::records(&file)
                .next()
                .unwrap()
                .unpack()
                .map(|v| v.len()),
            Some(4 << 20)
        );

        // Noise with markers dropped in, and a good file with bytes
        // changed: every walk returns.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut good = MAGIC.to_vec();
        good.extend(record_bytes("VS_a_Wrapped", 0x32, b"first container"));
        good.extend(record_bytes(&"n".repeat(40), 0x41, &[9; 500]));
        assert_eq!(walk(&good), 2);
        for round in 0..2000 {
            let len = xorshift(&mut seed) as usize % 600;
            let mut file: Vec<u8> = (0..len).map(|_| xorshift(&mut seed) as u8).collect();
            for _ in 0..round % 5 {
                let at = xorshift(&mut seed) as usize % (file.len() + 1);
                let n = (xorshift(&mut seed) % 64) as u32;
                // Sizes that agree, so the record is taken further.
                let record = raw_record(&[], [n, n], xorshift(&mut seed) as u32 % 4096, &[]);
                file.splice(at..at, record);
            }
            walk(&file);

            let mut file = good.clone();
            for _ in 0..1 + round % 4 {
                let at = xorshift(&mut seed) as usize % file.len();
                file[at] = xorshift(&mut seed) as u8;
            }
            let cut = file.len() - xorshift(&mut seed) as usize % 8;
            assert!(walk(&file[..cut]) <= 2);
        }
    }

    #[test]
    fn a_marker_in_other_data_is_not_a_record() {
        let marker = 0x000D_DCE7u32;
        // Alone, at the start, at the end, twice in a row.
        assert_eq!(records(&MARKER).count(), 0);
        assert_eq!(records(&[MARKER, MARKER].concat()).count(), 0);
        let mut file = vec![0x5a; 64];
        file.extend_from_slice(&MARKER);
        assert_eq!(records(&file).count(), 0);
        file.extend_from_slice(&[0x5a; 64]);
        assert_eq!(records(&file).count(), 0, "the sizes are 0x5a5a5a5a");

        let before = [0u8, 0x32];
        // The two packed sizes disagree.
        let file = raw_record(&before, [16, 17], 4, &[0; 16]);
        assert_eq!(records(&file).count(), 0);
        // They agree but the file ends before the packed bytes do.
        let file = raw_record(&before, [16, 16], 4, &[0; 3]);
        assert_eq!(records(&file).count(), 0);
        let file = raw_record(&before, [16, 16], 4, &[0; 4]);
        assert_eq!(records(&file).count(), 1);
        // A marker as the unpacked size of a record is not a second one.
        let file = raw_record(&before, [16, 16], marker, &[0; 4]);
        let found: Vec<_> = records(&file).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].offset, before.len() + 4);
        // Markers inside a real record's packed bytes do not add records
        // unless sizes around them agree, and the real one is still found.
        let mut file = record_bytes("CS_y", 0x3c, &[3; 32]);
        file.extend_from_slice(&MARKER);
        file.extend_from_slice(&[0; 2]);
        file.extend(record_bytes("CS_z", 0x3c, &[4; 32]));
        let found: Vec<_> = records(&file).collect();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].unpack(), Some(vec![3; 32]));
        assert_eq!(found[1].unpack(), Some(vec![4; 32]));
        assert_eq!(found[1].name, Some("CS_z"));
    }

    /// The length byte counts the name and its NUL. Up to 31 characters it
    /// is a control character or a space; from 32 on it is printable and
    /// reads like one more character of the name.
    #[test]
    fn names_of_31_32_and_33_bytes() {
        for len in [
            1usize, 2, 30, 31, 32, 33, 34, 93, 94, 95, 126, 127, 200, 254,
        ] {
            let name: String = (0..len).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            let file = record_bytes(&name, 0x32, b"x");
            let found: Vec<_> = records(&file).collect();
            assert_eq!(found.len(), 1, "{len}");
            assert_eq!(found[0].name, Some(name.as_str()), "{len}");
            assert_eq!(found[0].kind, 0x32);

            // With printable bytes right before the length byte the
            // name's start is a guess among the bytes that could be its
            // length: the name found then still ends with the real one.
            let mut archive = b"other data, printable".to_vec();
            archive.extend_from_slice(&file[4..]);
            let found: Vec<_> = records(&archive).collect();
            assert_eq!(found.len(), 1, "{len} after text");
            assert!(
                found[0].name.is_some_and(|n| n.ends_with(name.as_str())),
                "{len} after text: {:?}",
                found[0].name
            );
        }
        // No NUL where the name should end: a record without a name.
        let mut file = record_bytes("PS_named", 0x32, b"x");
        let nul = 4 + 1 + "PS_named".len();
        assert_eq!(file[nul], 0);
        file[nul] = b'!';
        assert_eq!(records(&file).next().unwrap().name, None);
        // A length byte that does not fit the name: no name either.
        let mut file = record_bytes("PS_named", 0x32, b"x");
        file[4] += 1;
        assert_eq!(records(&file).next().unwrap().name, None);
    }

    #[test]
    fn corrupt_stream_does_not_unpack() {
        let mut file = record_bytes("CS_x", 0x3c, &[1; 64]);
        file[27] = 0xFF; // first packed byte: reserved block type
        let r = records(&file).next().unwrap();
        assert_eq!(r.unpack(), None);
    }
}
