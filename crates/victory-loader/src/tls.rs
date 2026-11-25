//! Static TLS of an executable mapped as a DLL.
//!
//! An exe always gets TLS index 0, so title code reaches its thread-local
//! block as `mov r64, gs:[0x58]` (the thread's TLS array) followed by
//! `[r64]`, without reading `_tls_index`. Mapped as a DLL, the image gets
//! another index. Every such load is turned into a call to a thunk that
//! yields `&array[index]` instead, and the image's `_tls_index` (which CRT
//! code does read, and adds to the array pointer) is set to 0.

use std::ffi::c_void;

use anyhow::{Result, bail};
use victory_pe::{ImageInfo, SectionInfo, layout};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
    VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::tls_scan::{self, Scan, THUNK_SIZE};

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const TLS64_ADDRESS_OF_INDEX: u64 = 16;

/// Redirects the image's TLS-array loads if the OS gave it an index other
/// than 0. Returns the number of loads patched.
///
/// # Safety
/// `base` is the image `info` describes, mapped and not yet running.
pub(crate) unsafe fn redirect(base: usize, info: &ImageInfo) -> Result<usize> {
    // SAFETY: forwarded from the caller.
    Ok(unsafe { redirect_image(base, info) }?.rewritten())
}

/// The bytes of `section` in memory: where a section says it has none, the
/// loader maps what the file holds for it.
fn mapped_len(section: &SectionInfo) -> u32 {
    match section.virtual_size {
        0 => section.file_size,
        size => size,
    }
}

/// Whether the `len` bytes at `rva` are inside one section of the image,
/// which is what makes them mapped. The headers of an image are the
/// title's: every RVA and pointer read from them goes through this before
/// it is used.
fn in_a_section(info: &ImageInfo, rva: u64, len: u64) -> bool {
    info.sections.iter().any(|s| {
        let (start, end) = (
            u64::from(s.rva),
            u64::from(s.rva) + u64::from(mapped_len(s)),
        );
        end <= u64::from(info.size_of_image)
            && rva >= start
            && rva.checked_add(len).is_some_and(|rva_end| rva_end <= end)
    })
}

/// The functions the mapped image's exception directory lists (see
/// [`tls_scan::functions`]); `None` for an image without one. They decide
/// only how a rewritten load is counted, not whether it is rewritten.
///
/// # Safety
/// As for [`redirect`].
unsafe fn listed_functions(
    base: usize,
    info: &ImageInfo,
) -> Result<Option<Vec<std::ops::Range<u32>>>> {
    // SAFETY: an image's first page holds its headers (as much of it as
    // the image has).
    let headers = unsafe {
        std::slice::from_raw_parts(base as *const u8, info.size_of_image.min(0x1000) as _)
    };
    let directory = layout::data_directory_offset(headers, layout::dir::EXCEPTION).and_then(|at| {
        Some((
            layout::read_u32(headers, at)?,
            layout::read_u32(headers, at + 4)?,
        ))
    });
    let Some((rva, size)) = directory.filter(|&(rva, size)| rva != 0 && size != 0) else {
        return Ok(None);
    };
    if !in_a_section(info, rva.into(), size.into()) {
        bail!(
            "the exception directory ({size:#x} bytes at RVA {rva:#x}) is outside the image's sections"
        );
    }
    // SAFETY: inside a section of the mapped image, as just checked.
    let pdata =
        unsafe { std::slice::from_raw_parts((base + rva as usize) as *const u8, size as _) };
    Ok(Some(tls_scan::functions(pdata, info.size_of_image)))
}

/// [`redirect`], with everything the scan found.
///
/// # Safety
/// As for [`redirect`].
unsafe fn redirect_image(base: usize, info: &ImageInfo) -> Result<Scan> {
    let Some(tls) = &info.tls else {
        return Ok(Scan::default());
    };
    let pointer_rva = u64::from(tls.directory_rva) + TLS64_ADDRESS_OF_INDEX;
    if !in_a_section(info, pointer_rva, 8) {
        bail!(
            "the TLS directory at RVA {:#x} is outside the image's sections",
            tls.directory_rva
        );
    }
    // SAFETY: inside a section of the mapped image, as just checked; the
    // OS relocated the pointer.
    let index_at = unsafe { ((base + pointer_rva as usize) as *const usize).read_unaligned() };
    if index_at == 0 {
        bail!("the TLS directory's index pointer is null");
    }
    let inside = index_at
        .checked_sub(base)
        .is_some_and(|rva| in_a_section(info, rva as u64, 4));
    if !inside {
        bail!("the TLS directory's index pointer {index_at:#x} is outside the image at {base:#x}");
    }
    let index_at = index_at as *mut u32;
    // SAFETY: inside a section of the mapped image, as just checked; the
    // OS wrote the index there.
    let index = unsafe { index_at.read_unaligned() };
    if index == 0 {
        return Ok(Scan::default());
    }
    displacement(index)?;
    let code: Vec<_> = info
        .sections
        .iter()
        .filter(|s| s.characteristics & SCN_MEM_EXECUTE != 0 && mapped_len(s) != 0)
        .collect();
    if let Some(s) = code
        .iter()
        .find(|s| !in_a_section(info, s.rva.into(), mapped_len(s).into()))
    {
        bail!(
            "the code section at RVA {:#x} ({:#x} bytes) is outside the image",
            s.rva,
            mapped_len(s)
        );
    }
    // SAFETY: forwarded from the caller.
    let functions = unsafe { listed_functions(base, info) }?;
    let thunks = thunks(near(base, info.size_of_image as usize)?, index)?;
    let mut found = Scan::default();
    for section in code {
        let start = base + section.rva as usize;
        let len = mapped_len(section) as usize;
        let mut old = 0;
        // SAFETY: the section is part of the mapped image.
        if unsafe { VirtualProtect(start as _, len, PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
            bail!(
                "VirtualProtect(code) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: writable now, and no title code runs yet.
        let code = unsafe { std::slice::from_raw_parts_mut(start as *mut u8, len) };
        found += match &functions {
            Some(functions) => {
                let end = section.rva + mapped_len(section);
                let inside = tls_scan::within(functions, section.rva..end);
                tls_scan::patch_functions(code, start, thunks, &inside)
            }
            None => tls_scan::patch(code, start, thunks),
        };
        // SAFETY: restoring the protection we changed; flushing our own code.
        unsafe {
            VirtualProtect(start as _, len, old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), start as _, len);
        }
    }
    // SAFETY: see `index_at`; `.data` is writable.
    unsafe { index_at.write_unaligned(0) };
    let Scan {
        patched,
        unpatched,
        patched_outside_functions,
    } = found;
    // Both counts, so that a run of a real title records how many loads
    // lie outside the functions its exception directory lists (leaf
    // functions, or data: see `tls_scan`); for an image that lists none,
    // all are in `patched_in_functions`.
    tracing::info!(
        index,
        patched_in_functions = patched,
        patched_outside_functions,
        unpatched,
        lists_functions = functions.is_some(),
        "static TLS redirected"
    );
    if unpatched != 0 {
        // Each of these still reads the TLS array itself, and so slot 0:
        // another image's block.
        tracing::warn!(
            unpatched,
            "TLS-array accesses were left as they are: forms that are not rewritten"
        );
    }
    Ok(found)
}

/// `index * 8` as the displacement of a thunk's `lea r, [r + disp32]`,
/// which the CPU sign-extends: an index it cannot hold is an error (the OS
/// hands out small ones; the number is read from the image's memory).
fn displacement(index: u32) -> Result<[u8; 4]> {
    match index.checked_mul(8).map(i32::try_from) {
        Some(Ok(disp)) => Ok(disp.to_le_bytes()),
        _ => bail!("TLS index {index:#x} is too large for the redirect's thunks"),
    }
}

/// Fills `block` with one thunk per register: `mov r, gs:[0x58];
/// lea r, [r + index * 8]; ret` (flags untouched, like the load it replaces).
fn thunks(block: *mut u8, index: u32) -> Result<usize> {
    let disp = displacement(index)?;
    for reg in 0..16u8 {
        let (r, low) = (reg >> 3, reg & 7);
        let mut code = vec![0x65, 0x48 | r << 2, 0x8B, low << 3 | 4, 0x25, 0x58, 0, 0, 0];
        code.extend([0x48 | r << 2 | r, 0x8D, 0x80 | low << 3 | low]);
        if low == 4 {
            code.push(0x24); // SIB: [r12 + disp32]
        }
        code.extend(disp);
        code.push(0xC3);
        // SAFETY: `block` holds 16 thunks and is still writable.
        unsafe {
            std::ptr::copy_nonoverlapping(
                code.as_ptr(),
                block.add(usize::from(reg) * THUNK_SIZE),
                code.len(),
            )
        };
    }
    let mut old = 0;
    // SAFETY: `block` is our allocation of one page.
    if unsafe { VirtualProtect(block as _, 4096, PAGE_EXECUTE_READ, &mut old) } == 0 {
        bail!(
            "VirtualProtect(TLS thunks) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(block as usize)
}

/// One writable page within `rel32` reach of `[base, base + size)`. Leaked:
/// title code calls into it for the life of the process.
fn near(base: usize, size: usize) -> Result<*mut u8> {
    const GRANULE: usize = 0x1_0000;
    // The search spans 1 GiB each side, so every call site stays in reach.
    if size >= 1 << 30 {
        bail!("image too large for TLS thunks within rel32 reach");
    }
    let above = (base + size).next_multiple_of(GRANULE);
    for k in 0..(1usize << 14) {
        for hint in [
            base.checked_sub((k + 1) * GRANULE),
            Some(above + k * GRANULE),
        ] {
            let Some(hint) = hint else { continue };
            // SAFETY: a fresh allocation at a free address, or null.
            let p = unsafe {
                VirtualAlloc(
                    hint as *const c_void,
                    4096,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if !p.is_null() {
                return Ok(p.cast());
            }
        }
    }
    bail!("no free page near the image for TLS thunks")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the thread's TLS array pointer the way title code does.
    fn tls_array() -> usize {
        let array: usize;
        // SAFETY: reads TEB.ThreadLocalStoragePointer.
        unsafe { std::arch::asm!("mov {}, gs:[0x58]", out(reg) array) };
        array
    }

    #[test]
    fn loads_become_calls_that_yield_the_title_slot() {
        let page = near(tls_array as *const () as usize, 0).unwrap();
        let thunks = thunks(near(page as usize, 4096).unwrap(), 3).unwrap();
        // (before the load, REX, ModRM, after the load): each case returns
        // the loaded register in rax; r12 is saved, being callee-saved.
        let cases: [(&[u8], u8, u8, &[u8]); 3] = [
            (&[], 0x48, 0x04, &[0xC3]),                   // rax
            (&[], 0x4C, 0x04, &[0x4C, 0x89, 0xC0, 0xC3]), // r8
            (
                &[0x41, 0x54],
                0x4C,
                0x24,
                &[0x4C, 0x89, 0xE0, 0x41, 0x5C, 0xC3],
            ), // r12
        ];
        for (before, rex, modrm, after) in cases {
            let mut code = before.to_vec();
            code.extend([0x65, rex, 0x8B, modrm, 0x25, 0x58, 0, 0, 0]);
            code.extend(after);
            // SAFETY: the page is ours and writable.
            let bytes = unsafe { std::slice::from_raw_parts_mut(page, code.len()) };
            bytes.copy_from_slice(&code);
            let scan = tls_scan::patch(bytes, page as usize, thunks);
            assert_eq!(scan.patched, 1);
            let mut old = 0;
            // SAFETY: our page, made executable to run the patched code.
            unsafe { VirtualProtect(page as _, 4096, PAGE_EXECUTE_READ, &mut old) };
            // SAFETY: the page holds the patched load and a return.
            let f: extern "system" fn() -> usize = unsafe { std::mem::transmute(page) };
            assert_eq!(f(), tls_array() + 3 * 8, "REX {rex:#x} ModRM {modrm:#x}");
            // SAFETY: our page again.
            unsafe { VirtualProtect(page as _, 4096, PAGE_READWRITE, &mut old) };
        }
    }

    use victory_pe::testing::{PeBuilder, scn};
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    /// `mov r64, gs:[0x58]` into register `reg`.
    fn load(reg: u8) -> [u8; 9] {
        let rex = 0x48 | (reg >> 3) << 2;
        [0x65, rex, 0x8B, (reg & 7) << 3 | 4, 0x25, 0x58, 0, 0, 0]
    }

    /// Where things are in [`title_code`], from the section's start.
    const TABLE: usize = 0x10;
    const SECOND: usize = 0x20;
    const SECOND_END: usize = 0x3B;
    const TEXT_RVA: u32 = 0x1000;
    const PDATA_RVA: u32 = 0x2000;

    /// A code section as a title's could be: a function with a load, a
    /// table between functions that holds the same nine bytes, and a
    /// function with a load and two forms that are not rewritten.
    fn title_code() -> Vec<u8> {
        let mut code = vec![0xCC; 0x40];
        let mut put = |at: usize, bytes: &[u8]| code[at..at + bytes.len()].copy_from_slice(bytes);
        put(0, &load(0));
        put(9, &[0xC3]);
        put(TABLE, &load(1));
        put(SECOND, &load(8));
        put(SECOND + 9, &[0x65, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0]); // mov eax, gs:[0x58]
        put(SECOND + 17, &[0x65, 0x48, 0x8B, 0x24, 0x25, 0x58, 0, 0, 0]); // into rsp
        put(SECOND_END - 1, &[0xC3]);
        code
    }

    /// The executable [`title_code`] is in: with static TLS and, if
    /// `listed`, an exception directory that lists the two functions.
    fn title(listed: bool) -> PeBuilder {
        let mut pdata = Vec::new();
        for (begin, end) in [(0, 10), (SECOND, SECOND_END)] {
            for rva in [TEXT_RVA + begin as u32, TEXT_RVA + end as u32, 0] {
                pdata.extend(rva.to_le_bytes());
            }
        }
        let builder = PeBuilder::new()
            .section(".text", scn::CODE, &title_code())
            .section(".pdata", scn::RDATA, &pdata)
            .tls_without_callbacks();
        if listed {
            builder.data_directory(layout::dir::EXCEPTION, PDATA_RVA, pdata.len() as u32)
        } else {
            builder
        }
    }

    /// An image in memory the way the loader leaves it: headers and
    /// sections at their RVAs, the TLS directory's index pointer
    /// relocated, and `index` where it points.
    struct Mapped {
        base: usize,
        info: ImageInfo,
    }

    impl Mapped {
        fn new(builder: &PeBuilder, index: u32) -> Mapped {
            let file = builder.build().expect("a synthetic image").bytes;
            let info = ImageInfo::parse(&file).expect("a synthetic image");
            let size = info.size_of_image as usize;
            // SAFETY: fresh pages of ours.
            let base = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    size,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            assert!(!base.is_null());
            // SAFETY: `size` writable bytes, zeroed.
            let image = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), size) };
            let headers = info.sections.iter().map(|s| s.file_offset).min().unwrap() as usize;
            image[..headers].copy_from_slice(&file[..headers]);
            for s in &info.sections {
                let len = s.virtual_size.min(s.file_size) as usize;
                let (from, to) = (s.file_offset as usize, s.rva as usize);
                image[to..to + len].copy_from_slice(&file[from..from + len]);
            }
            let mapped = Mapped {
                base: base as usize,
                info,
            };
            let preferred = mapped.index_pointer() as u64 - mapped.info.image_base;
            mapped.set_index_pointer(mapped.base + preferred as usize);
            // SAFETY: the index is in the image's TLS section.
            unsafe { (mapped.index_pointer() as *mut u32).write_unaligned(index) };
            mapped
        }

        fn index_pointer_at(&self) -> *mut usize {
            let rva = self.info.tls.as_ref().unwrap().directory_rva as usize;
            (self.base + rva + TLS64_ADDRESS_OF_INDEX as usize) as *mut usize
        }

        fn index_pointer(&self) -> usize {
            // SAFETY: the TLS directory is in our mapped image.
            unsafe { self.index_pointer_at().read_unaligned() }
        }

        fn set_index_pointer(&self, to: usize) {
            // SAFETY: as above.
            unsafe { self.index_pointer_at().write_unaligned(to) }
        }

        /// The index the image has now.
        fn index(&self) -> u32 {
            // SAFETY: tests that call this left the pointer in the image.
            unsafe { (self.index_pointer() as *const u32).read_unaligned() }
        }

        fn code(&self) -> &[u8] {
            let at = (self.base + TEXT_RVA as usize) as *const u8;
            // SAFETY: the code section of our mapped image.
            unsafe { std::slice::from_raw_parts(at, title_code().len()) }
        }

        fn redirect(&self) -> Result<Scan> {
            // SAFETY: mapped as `info` describes; nothing runs its code.
            unsafe { redirect_image(self.base, &self.info) }
        }

        /// What the redirect refused the image for.
        fn refusal(&self) -> String {
            match self.redirect() {
                Ok(scan) => format!("redirected: {scan:?}"),
                Err(e) => e.to_string(),
            }
        }
    }

    impl Drop for Mapped {
        fn drop(&mut self) {
            // SAFETY: releases the pages `new` allocated.
            unsafe { VirtualFree(self.base as _, 0, MEM_RELEASE) };
        }
    }

    fn found(patched: usize, unpatched: usize, patched_outside_functions: usize) -> Scan {
        Scan {
            patched,
            unpatched,
            patched_outside_functions,
        }
    }

    /// The load between the two listed functions (a leaf function has no
    /// unwind entry, so it would look like this) is rewritten too, and
    /// counted apart from the two inside them.
    #[test]
    fn redirect_patches_outside_the_listed_functions_and_counts_it_apart() {
        let image = Mapped::new(&title(true), 3);
        assert_eq!(image.redirect().unwrap(), found(2, 2, 1));
        let (code, before) = (image.code(), title_code());
        assert_eq!(
            (code[0], code[SECOND]),
            (0xE8, 0xE8),
            "the two loads in listed functions are calls"
        );
        assert_eq!(code[TABLE], 0xE8, "so is the one between them");
        assert_eq!(code[TABLE + 5..TABLE + 9], [0x0F, 0x1F, 0x40, 0x00]);
        assert_eq!(code[TABLE + 9..SECOND], before[TABLE + 9..SECOND]);
        assert_eq!(code[SECOND + 9..], before[SECOND + 9..], "the other forms");
        assert_eq!(image.index(), 0, "CRT code adds the index itself");
        // Redirected once: the index is 0 now, and there is nothing to do.
        assert_eq!(image.redirect().unwrap(), found(0, 0, 0));
        // The public entry reports the loads patched.
        let image = Mapped::new(&title(true), 3);
        // SAFETY: mapped as `info` describes; nothing runs its code.
        assert_eq!(unsafe { redirect(image.base, &image.info) }.unwrap(), 3);
    }

    /// Only executable sections are scanned: the same nine bytes in
    /// read-only or writable data are not code, whatever the exception
    /// directory lists.
    #[test]
    fn a_match_in_a_non_executable_section_is_never_patched() {
        let mut data = load(2).to_vec();
        data.extend(load(9));
        data.extend([0xAA; 14]);
        for listed in [true, false] {
            let builder = title(listed).section(".rdata", scn::RDATA, &data).section(
                ".data",
                scn::DATA,
                &data,
            );
            let image = Mapped::new(&builder, 3);
            let scan = image.redirect().unwrap();
            // What the code section alone holds (see the tests above).
            let expected = if listed {
                found(2, 2, 1)
            } else {
                found(3, 2, 0)
            };
            assert_eq!(scan, expected, "listed {listed}");
            let mut seen = 0;
            for s in &image.info.sections {
                if !(s.name.starts_with(".rdata") || s.name.starts_with(".data")) {
                    continue;
                }
                assert_eq!(s.characteristics & SCN_MEM_EXECUTE, 0);
                let at = (image.base + s.rva as usize) as *const u8;
                // SAFETY: a section of our mapped image.
                let mapped = unsafe { std::slice::from_raw_parts(at, data.len()) };
                assert_eq!(mapped, data, "{} (listed {listed})", s.name);
                seen += 1;
            }
            assert_eq!(seen, 2, "both data sections were looked at");
            assert_eq!(image.code()[0], 0xE8, "the code was redirected");
        }
    }

    #[test]
    fn an_image_that_lists_no_functions_is_scanned_whole() {
        let image = Mapped::new(&title(false), 3);
        assert_eq!(image.redirect().unwrap(), found(3, 2, 0));
        let code = image.code();
        assert_eq!((code[0], code[TABLE], code[SECOND]), (0xE8, 0xE8, 0xE8));
        // Index 0, the one an exe expects: nothing is touched.
        let image = Mapped::new(&title(false), 0);
        assert_eq!(image.redirect().unwrap(), found(0, 0, 0));
        assert_eq!(image.code(), title_code());
    }

    #[test]
    fn redirect_rejects_a_null_index_pointer() {
        let image = Mapped::new(&title(true), 3);
        image.set_index_pointer(0);
        assert_eq!(image.refusal(), "the TLS directory's index pointer is null");
        assert_eq!(image.code(), title_code(), "nothing was patched");
    }

    #[test]
    fn redirect_rejects_an_index_pointer_outside_the_image() {
        let image = Mapped::new(&title(true), 3);
        let (base, size) = (image.base, image.info.size_of_image as usize);
        // Memory of ours that is not the image's: it must not be read as
        // the index, nor set to 0.
        // A `Cell`, so that the check below reads memory and not a constant.
        let ours = std::cell::Cell::new(3u32);
        let outside = [
            ours.as_ptr() as usize,
            base - 4,
            base + size,
            base + size - 3,
            8,
            usize::MAX - 2,
        ];
        for pointer in outside {
            image.set_index_pointer(pointer);
            let expected = format!(
                "the TLS directory's index pointer {pointer:#x} is outside the image at {base:#x}"
            );
            assert_eq!(image.refusal(), expected);
        }
        assert_eq!(std::hint::black_box(&ours).get(), 3);
        assert_eq!(image.code(), title_code(), "nothing was patched");
    }

    #[test]
    fn redirect_rejects_directories_outside_the_sections() {
        // A TLS directory the headers put past the image's end.
        let mut image = Mapped::new(&title(true), 3);
        let size = image.info.size_of_image;
        image.info.tls.as_mut().unwrap().directory_rva = size - 8;
        let expected = format!(
            "the TLS directory at RVA {:#x} is outside the image's sections",
            size - 8
        );
        assert_eq!(image.refusal(), expected);
        image.info.tls.as_mut().unwrap().directory_rva = u32::MAX;
        assert!(
            image
                .refusal()
                .starts_with("the TLS directory at RVA 0xffffffff")
        );
        // An exception directory that runs out of its section.
        let image = Mapped::new(
            &title(true).data_directory(layout::dir::EXCEPTION, PDATA_RVA, 0x10_0000),
            3,
        );
        let expected = "the exception directory (0x100000 bytes at RVA 0x2000) is outside the \
                        image's sections";
        assert_eq!(image.refusal(), expected);
        assert_eq!(image.code(), title_code(), "nothing was patched");
        assert_eq!(image.index(), 3);
    }

    #[test]
    fn zero_virtual_size_section_is_scanned() {
        let mut image = Mapped::new(&title(true), 3);
        let text = &mut image.info.sections[0];
        assert!(text.name.starts_with(".text") && text.file_size as usize >= title_code().len());
        text.virtual_size = 0;
        assert_eq!(image.redirect().unwrap(), found(2, 2, 1));
        assert_eq!(image.code()[0], 0xE8);
    }

    #[test]
    fn large_index_does_not_overflow_the_displacement() {
        let page = near(tls_array as *const () as usize, 0).unwrap();
        // 8 * index must fit the positive half of a sign-extended disp32.
        for index in [0x1000_0000, 0x1fff_ffff, 0x2000_0000, 0x2000_0001, u32::MAX] {
            let refused = thunks(page, index).map(|_| ()).map_err(|e| e.to_string());
            let expected = format!("TLS index {index:#x} is too large for the redirect's thunks");
            assert_eq!(refused, Err(expected));
        }
        // SAFETY: `thunks` leaves the page writable when it refuses; the
        // largest index that fits is written as it stands.
        let lea = unsafe {
            assert!(thunks(page, 0x0fff_ffff).is_ok());
            std::slice::from_raw_parts(page.add(9), 7)
        };
        assert_eq!(lea, [0x48, 0x8D, 0x80, 0xf8, 0xff, 0xff, 0x7f]); // lea rax, [rax + 0x7ffffff8]
        // The image is refused before anything in it is changed.
        let image = Mapped::new(&title(true), 0x2000_0000);
        assert_eq!(
            image.refusal(),
            "TLS index 0x20000000 is too large for the redirect's thunks"
        );
        assert_eq!(
            (image.code(), image.index()),
            (&title_code()[..], 0x2000_0000)
        );
    }
}
