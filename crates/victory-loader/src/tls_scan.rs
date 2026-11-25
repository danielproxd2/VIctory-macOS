//! Finds the TLS-array loads in an image's code and rewrites them.
//!
//! The part of the static TLS redirect (see `tls`) that only looks at
//! bytes, so it is built and tested on every host.
//!
//! shortcut: a byte match over every executable section, not a decode.
//!
//! Established: in Grand Theft Auto VI all 53,246 hits of the whole-section
//! match were instruction starts and none was missed (checked against a
//! full disassembly). That is the behaviour kept here: every match in an
//! executable section is rewritten.
//!
//! Not established: how many of those hits lie outside the functions the
//! image's exception directory lists. That was never recorded, and the PE
//! format does not say that every function is listed: an x64 leaf function
//! (a small thread-local getter is one) has no unwind entry. So a match
//! outside the listed functions cannot be taken for data, and leaving it
//! would make such a function read slot 0, another image's block, in
//! silence. The scan therefore rewrites those too and counts them apart
//! (`patched_outside_functions`), and the redirect logs both counts, so
//! that a run of a real title records the number.
//!
//! What a byte match can still get wrong, and no title is known to show:
//! the nine bytes in a table or constant inside an executable section, and
//! the nine bytes inside a longer instruction (an immediate). Decode the
//! code if a title shows either; a large outside count with a crash in
//! data-driven code would be the sign of the first.

use std::ops::Range;

/// `mov r64, gs:[0x58]` is `65 REX 8B ModRM 25 58 00 00 00`.
const LOAD_LEN: usize = 9;
/// Bytes between the thunks of two registers.
pub(crate) const THUNK_SIZE: usize = 32;
/// `RUNTIME_FUNCTION`: begin, end and unwind data, each an RVA.
const RUNTIME_FUNCTION_SIZE: usize = 12;

/// What a scan of code found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scan {
    /// Loads rewritten as calls inside the functions the image lists; for
    /// an image that lists none, all of them.
    pub(crate) patched: usize,
    /// Accesses to the TLS array that were recognised but not rewritten,
    /// inside listed functions or not: the 32-bit load, the `moffs` form, a
    /// load into `rsp`, one with a redundant REX bit, or a call out of
    /// `rel32` reach. Code that goes on to index the array from one of
    /// these reads the wrong thread-local block.
    pub(crate) unpatched: usize,
    /// Loads rewritten as calls between the functions of an image that
    /// lists its functions: leaf functions (one that needs no unwind data
    /// is not listed) or, which the bytes alone do not tell apart, data
    /// that holds the same nine bytes (see the module's note).
    pub(crate) patched_outside_functions: usize,
}

impl Scan {
    /// Every load rewritten, listed function or not.
    pub(crate) fn rewritten(&self) -> usize {
        self.patched + self.patched_outside_functions
    }
}

impl std::ops::AddAssign for Scan {
    fn add_assign(&mut self, other: Scan) {
        self.patched += other.patched;
        self.unpatched += other.unpatched;
        self.patched_outside_functions += other.patched_outside_functions;
    }
}

/// An access to the TLS array at the start of some bytes.
#[derive(Debug, PartialEq, Eq)]
enum Access {
    /// `mov r64, gs:[0x58]` into this register, [`LOAD_LEN`] bytes: the one
    /// form that is rewritten.
    Load(usize),
    /// Another instruction that reads `gs:[0x58]`, of this length.
    Other(usize),
}

/// The access to the TLS array `code` starts with, if it holds all of one.
fn access(code: &[u8]) -> Option<Access> {
    if *code.first()? != 0x65 {
        return None;
    }
    let (rex, at) = match *code.get(1)? {
        rex @ 0x40..=0x4F => (rex, 2),
        _ => (0, 1),
    };
    match *code.get(at)? {
        // mov r, gs:[disp32]: ModRM mod 00, r/m 100, then the SIB byte for
        // "no base, no index" and the displacement.
        0x8B => {
            let modrm = *code.get(at + 1)?;
            // With REX.X the SIB byte names r12 as an index: another address.
            if modrm & 0xC7 != 0x04
                || code.get(at + 2..at + 7)? != [0x25, 0x58, 0, 0, 0]
                || rex & 2 != 0
            {
                return None;
            }
            let reg = usize::from((modrm >> 3) & 7) | usize::from(rex & 4) << 1;
            if matches!(rex, 0x48 | 0x4C) && reg != 4 {
                Some(Access::Load(reg))
            } else {
                Some(Access::Other(at + 7))
            }
        }
        // mov (r|e)ax, gs:[moffs64]
        0xA1 => (code.get(at + 1..at + 9)? == [0x58, 0, 0, 0, 0, 0, 0, 0])
            .then_some(Access::Other(at + 9)),
        _ => None,
    }
}

/// Scans `code[range]`, which is at address `at + range.start`: loads
/// become `call thunks[r]; nop4`, counted as inside a listed function or
/// (`function` false) outside. An instruction is looked for inside the
/// range only.
fn scan(
    code: &mut [u8],
    range: Range<usize>,
    at: usize,
    thunks: usize,
    function: bool,
    found: &mut Scan,
) {
    let mut i = range.start;
    while i < range.end {
        i += match access(&code[i..range.end]) {
            Some(Access::Load(reg)) => {
                let target = (thunks + reg * THUNK_SIZE) as i128;
                let rel = i32::try_from(target - (at + i + 5) as i128);
                if let Ok(rel) = rel {
                    code[i] = 0xE8;
                    code[i + 1..i + 5].copy_from_slice(&rel.to_le_bytes());
                    code[i + 5..i + 9].copy_from_slice(&[0x0F, 0x1F, 0x40, 0x00]);
                    if function {
                        found.patched += 1;
                    } else {
                        found.patched_outside_functions += 1;
                    }
                } else {
                    found.unpatched += 1;
                }
                LOAD_LEN
            }
            Some(Access::Other(len)) => {
                found.unpatched += 1;
                len
            }
            None => 1,
        };
    }
}

/// Rewrites each `mov r64, gs:[0x58]` in `code` (at address `at`), all of
/// which is taken to be instructions, as `call thunks[r]; nop4`. `thunks`
/// should be within 2 GiB of `code`: a load it is not is left and counted.
pub(crate) fn patch(code: &mut [u8], at: usize, thunks: usize) -> Scan {
    let mut found = Scan::default();
    scan(code, 0..code.len(), at, thunks, true, &mut found);
    found
}

/// [`patch`] for code whose listed functions are `functions` (offsets into
/// `code`, sorted and apart, as [`within`] returns them). The bytes between
/// them are scanned as well, apart from the functions (a match does not
/// straddle a function's start or end), and what is rewritten there is
/// counted as outside.
pub(crate) fn patch_functions(
    code: &mut [u8],
    at: usize,
    thunks: usize,
    functions: &[Range<usize>],
) -> Scan {
    let mut found = Scan::default();
    let mut done = 0;
    for function in functions {
        let end = function.end.min(code.len()).max(done);
        let start = function.start.clamp(done, end);
        scan(code, done..start, at, thunks, false, &mut found);
        scan(code, start..end, at, thunks, true, &mut found);
        done = end;
    }
    scan(code, done..code.len(), at, thunks, false, &mut found);
    found
}

/// The functions an exception directory (`pdata`, an array of
/// `RUNTIME_FUNCTION`) lists, as RVA ranges, sorted, with those that touch
/// or overlap merged. An entry that is empty, reversed or ends past
/// `size_of_image` is left out (linkers pad the table with zeros), as are
/// bytes after the last whole entry.
pub(crate) fn functions(pdata: &[u8], size_of_image: u32) -> Vec<Range<u32>> {
    let rva = |entry: &[u8; RUNTIME_FUNCTION_SIZE], at: usize| {
        u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]])
    };
    let mut listed: Vec<Range<u32>> = pdata
        .as_chunks::<RUNTIME_FUNCTION_SIZE>()
        .0
        .iter()
        .map(|entry| rva(entry, 0)..rva(entry, 4))
        .filter(|f| f.start < f.end && f.end <= size_of_image)
        .collect();
    listed.sort_by_key(|f| (f.start, f.end));
    let mut merged: Vec<Range<u32>> = Vec::with_capacity(listed.len());
    for function in listed {
        match merged.last_mut() {
            Some(last) if function.start <= last.end => last.end = last.end.max(function.end),
            _ => merged.push(function),
        }
    }
    merged
}

/// The parts of `functions` (as [`functions`] returns them) inside
/// `section`, an RVA range, as offsets from the section's start.
pub(crate) fn within(functions: &[Range<u32>], section: Range<u32>) -> Vec<Range<usize>> {
    functions
        .iter()
        .map(|f| f.start.max(section.start)..f.end.min(section.end))
        .filter(|f| f.start < f.end)
        .map(|f| (f.start - section.start) as usize..(f.end - section.start) as usize)
        .collect()
}

#[cfg(test)]
// The functions of a test are often one range: a list of one is meant.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    /// `mov r64, gs:[0x58]` into register `reg`.
    fn load(reg: u8) -> [u8; 9] {
        let rex = 0x48 | (reg >> 3) << 2;
        [0x65, rex, 0x8B, (reg & 7) << 3 | 4, 0x25, 0x58, 0, 0, 0]
    }

    const AT: usize = 0x1_4000_1000;
    const THUNKS: usize = 0x1_3fff_0000;

    fn found(patched: usize, unpatched: usize, patched_outside_functions: usize) -> Scan {
        Scan {
            patched,
            unpatched,
            patched_outside_functions,
        }
    }

    #[test]
    fn patches_every_register_form() {
        for reg in (0..16u8).filter(|&r| r != 4) {
            // push rbx; the load; ret
            let mut code = vec![0x53];
            code.extend(load(reg));
            code.push(0xC3);
            assert_eq!(
                patch(&mut code, AT, THUNKS),
                found(1, 0, 0),
                "register {reg}"
            );
            // call rel32 to the register's thunk, from the call's end.
            assert_eq!((code[0], code[1], code[10]), (0x53, 0xE8, 0xC3));
            let rel = i32::from_le_bytes([code[2], code[3], code[4], code[5]]);
            let target = (AT + 1 + 5).wrapping_add_signed(rel as isize);
            assert_eq!(
                target,
                THUNKS + usize::from(reg) * THUNK_SIZE,
                "register {reg}"
            );
            assert_eq!(code[6..10], [0x0F, 0x1F, 0x40, 0x00], "a four-byte nop");
        }
        // Loads back to back, and one that ends with the code.
        let mut code = [load(0), load(9), load(15)].concat();
        assert_eq!(patch(&mut code, AT, THUNKS), found(3, 0, 0));
        assert_eq!((code[0], code[9], code[18]), (0xE8, 0xE8, 0xE8));
        // Cut off one byte short, a different offset, another segment.
        let mut cut = load(0)[..8].to_vec();
        assert_eq!(patch(&mut cut, AT, THUNKS), found(0, 0, 0));
        let mut other = load(0);
        other[5] = 0x30; // gs:[0x30], the TEB's own address
        assert_eq!(patch(&mut other, AT, THUNKS), found(0, 0, 0));
        let mut other = load(0);
        other[0] = 0x64; // fs
        assert_eq!(patch(&mut other, AT, THUNKS), found(0, 0, 0));
        assert_eq!(other, [0x64, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]);
    }

    /// A load between the listed functions (a leaf function has no unwind
    /// entry) is rewritten like one inside, and counted apart.
    #[test]
    fn a_load_outside_the_listed_functions_is_patched_and_counted_apart() {
        // A listed function with a load, a leaf that is not listed (a
        // load into rcx, a return and padding), then a second listed one.
        let mut code = Vec::new();
        code.extend(load(0));
        code.push(0xC3);
        let leaf = code.len();
        code.extend(load(1));
        code.push(0xC3);
        code.extend([0xCC; 6]);
        let second = code.len();
        code.extend(load(8));
        code.push(0xC3);
        let before = code.clone();
        let functions = [0..leaf, second..code.len()];
        let scan = patch_functions(&mut code, AT, THUNKS, &functions);
        assert_eq!(scan, found(2, 0, 1));
        assert_eq!(scan.rewritten(), 3);
        assert_eq!((code[0], code[leaf], code[second]), (0xE8, 0xE8, 0xE8));
        // The leaf's call goes to its own register's thunk.
        let rel = i32::from_le_bytes([
            code[leaf + 1],
            code[leaf + 2],
            code[leaf + 3],
            code[leaf + 4],
        ]);
        let target = (AT + leaf + 5).wrapping_add_signed(rel as isize);
        assert_eq!(target, THUNKS + THUNK_SIZE);
        assert_eq!(code[leaf + 9..second], before[leaf + 9..second]);
        // With no functions listed at all, every one is outside.
        let mut code = before.clone();
        assert_eq!(patch_functions(&mut code, AT, THUNKS, &[]), found(0, 0, 3));
        let whole = code.clone();
        // The same bytes as one stretch of code: the same rewrite, all
        // counted as inside.
        let mut code = before.clone();
        assert_eq!(patch(&mut code, AT, THUNKS), found(3, 0, 0));
        assert_eq!(code, whole);
        // Outside a function a thunk out of reach is counted as left too.
        let mut code = before.clone();
        let far = AT + (1 << 32);
        assert_eq!(
            patch_functions(&mut code, AT, far, &functions),
            found(0, 3, 0)
        );
        assert_eq!(code, before);
    }

    #[test]
    fn a_load_across_a_function_end_is_not_one() {
        // The nine bytes start in a function and end after it.
        let mut code = vec![0x90; 4];
        code.extend(load(0));
        code.extend([0x90; 4]);
        let before = code.clone();
        let scan = patch_functions(&mut code, AT, THUNKS, &[0..8]);
        assert_eq!((scan, &code), (found(0, 0, 0), &before));
        // Ranges past the code's end, empty or out of order do no harm.
        let odd = [2..2, 4..13, 9..11, 40..90, 30..usize::MAX];
        let scan = patch_functions(&mut code, AT, THUNKS, &odd);
        assert_eq!((scan, code[4]), (found(1, 0, 0), 0xE8));
    }

    /// Forms of the same access that the redirect does not rewrite. They
    /// are expected misses: nothing is known to use them (see the module's
    /// note), a call does not fit in the eight bytes of the 32-bit form,
    /// and `rsp` cannot be loaded by a call. They are counted, so that a
    /// title that does use one says so in the log instead of reading
    /// another image's thread-local block in silence.
    #[test]
    fn alternate_encodings_are_counted_not_patched() {
        let forms: [(&str, &[u8]); 6] = [
            (
                "mov eax, gs:[0x58]",
                &[0x65, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0],
            ),
            (
                "mov r9d, gs:[0x58]",
                &[0x65, 0x44, 0x8B, 0x0C, 0x25, 0x58, 0, 0, 0],
            ),
            (
                "mov rsp, gs:[0x58]",
                &[0x65, 0x48, 0x8B, 0x24, 0x25, 0x58, 0, 0, 0],
            ),
            (
                "mov rax, gs:[0x58], REX.B set",
                &[0x65, 0x49, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0],
            ),
            (
                "mov rax, gs:[moffs 0x58]",
                &[0x65, 0x48, 0xA1, 0x58, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                "mov eax, gs:[moffs 0x58]",
                &[0x65, 0xA1, 0x58, 0, 0, 0, 0, 0, 0, 0],
            ),
        ];
        for (name, form) in forms {
            let mut code = form.to_vec();
            code.push(0xC3);
            let before = code.clone();
            assert_eq!(patch(&mut code, AT, THUNKS), found(0, 1, 0), "{name}");
            assert_eq!(code, before, "{name}");
            // Between listed functions they may be a leaf function's:
            // counted there as well.
            let scan = patch_functions(&mut code, AT, THUNKS, &[]);
            assert_eq!(scan, found(0, 1, 0), "{name}");
            assert_eq!(code, before, "{name}");
        }
        // One of each after a load that is rewritten: counted apiece.
        let mut code = load(3).to_vec();
        for (_, form) in forms {
            code.extend(form);
        }
        assert_eq!(patch(&mut code, AT, THUNKS), found(1, 6, 0));
    }

    /// Ways to the TLS array that are not recognised at all, and so not
    /// even counted. Expected misses too: the first is two instructions
    /// that cannot be told from other uses of the TEB by their bytes, and
    /// the others read something else.
    #[test]
    fn accesses_that_are_not_recognised() {
        let forms: [(&str, &[u8]); 4] = [
            (
                "mov rax, gs:[0x30]; mov rax, [rax + 0x58]",
                &[
                    0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0, 0, 0, 0x48, 0x8B, 0x40, 0x58,
                ],
            ),
            (
                "mov rax, gs:[r12 + 0x58], REX.X set",
                &[0x65, 0x4A, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0],
            ),
            (
                "mov rax, gs:[0x158]",
                &[0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 1, 0, 0],
            ),
            (
                "mov gs:[0x58], rax",
                &[0x65, 0x48, 0x89, 0x04, 0x25, 0x58, 0, 0, 0],
            ),
        ];
        for (name, form) in forms {
            let mut code = form.to_vec();
            assert_eq!(patch(&mut code, AT, THUNKS), found(0, 0, 0), "{name}");
            assert_eq!(code, form, "{name}");
        }
    }

    /// What a byte match cannot tell: the nine bytes as part of a longer
    /// instruction are taken for a load. Telling them apart needs a decoder (see the module's
    /// note); this test is to change when one is used.
    #[test]
    fn known_limit_pattern_inside_an_imm64_is_still_patched() {
        // mov rax, 0x0000_5825_048B_4865; add bl, al
        let mut code = vec![0x48, 0xB8];
        code.extend(&load(0)[..8]);
        code.extend([0x00, 0xC3]);
        let end = code.len();
        let scan = patch_functions(&mut code, AT, THUNKS, &[0..end]);
        assert_eq!((scan, code[2]), (found(1, 0, 0), 0xE8));
    }

    #[test]
    fn a_thunk_out_of_reach_leaves_the_load() {
        let mut code = load(0);
        let far = AT + (1 << 31) + 5;
        assert_eq!(patch(&mut code, AT, far), found(0, 1, 0));
        assert_eq!(code, load(0));
        // The furthest thunks that are in reach, each way.
        let mut code = load(0);
        assert_eq!(
            patch(&mut code, AT, AT + 5 + i32::MAX as usize),
            found(1, 0, 0)
        );
        let mut code = load(0);
        assert_eq!(patch(&mut code, AT, AT + 5 - (1 << 31)), found(1, 0, 0));
        assert_eq!(code[1..5], i32::MIN.to_le_bytes());
        let mut code = load(0);
        assert_eq!(patch(&mut code, AT, AT + 4 - (1 << 31)), found(0, 1, 0));
    }

    fn pdata(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut table = Vec::new();
        for &(begin, end) in entries {
            table.extend(begin.to_le_bytes());
            table.extend(end.to_le_bytes());
            table.extend(0x9000_u32.to_le_bytes());
        }
        table
    }

    #[test]
    fn functions_are_read_from_the_exception_directory() {
        let table = pdata(&[(0x1040, 0x1080), (0x1000, 0x1010), (0x2000, 0x2100)]);
        assert_eq!(
            functions(&table, 0x3000),
            [0x1000..0x1010, 0x1040..0x1080, 0x2000..0x2100]
        );
        // Overlapping and touching entries (a function and its chained
        // parts) become one range.
        let table = pdata(&[
            (0x1000, 0x1040),
            (0x1020, 0x1030),
            (0x1040, 0x1050),
            (0x1051, 0x1060),
        ]);
        assert_eq!(functions(&table, 0x3000), [0x1000..0x1050, 0x1051..0x1060]);
        // Padding, an empty and a reversed entry, entries that end past
        // the image, and bytes after the last whole entry are left out.
        let mut table = pdata(&[
            (0, 0),
            (0x1000, 0x1000),
            (0x1080, 0x1040),
            (0x2f00, 0x3001),
            (0xffff_fff0, u32::MAX),
            (0x1100, 0x1200),
            (0x2f00, 0x3000),
        ]);
        table.extend([0x00, 0x10, 0x00, 0x00, 0x10, 0x10, 0x00, 0x00, 0x00]);
        assert_eq!(functions(&table, 0x3000), [0x1100..0x1200, 0x2f00..0x3000]);
        assert!(functions(&[], 0x3000).is_empty());
        assert!(functions(&[0xff; 11], 0x3000).is_empty());
    }

    #[test]
    fn functions_are_cut_to_a_section() {
        let listed = [
            0x0800..0x1010,
            0x1040..0x1080,
            0x1ff0..0x2010,
            0x2800..0x2900,
        ];
        assert_eq!(
            within(&listed, 0x1000..0x2000),
            [0..0x10, 0x40..0x80, 0xff0..0x1000]
        );
        assert_eq!(within(&listed, 0x2000..0x2800), [0..0x10]);
        assert!(within(&listed, 0x3000..0x4000).is_empty());
        assert!(within(&listed, 0x1010..0x1010).is_empty());
        assert_eq!(within(&[0..u32::MAX], 0xffff_f000..u32::MAX), [0..0xfff]);
    }
}
