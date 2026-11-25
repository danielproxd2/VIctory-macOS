//! An image without base relocations can only run at the base it was
//! linked for. The Windows loader maps the prepared copy there when the
//! range is free; when it is not, loading fails and says so. Nothing
//! reserves that range ahead of time, so whether a fixed-base title loads
//! depends on what the process mapped first.
//!
//! The image is tests/fixtures/title-quiet (a DLL without a runtime, and
//! so without static TLS, whose directory holds addresses) with its
//! relocations taken away here, as a linker's `/FIXED` leaves an image, and
//! its preferred base set to a range this test picks. None of its code is
//! run: it was linked for another base.
#![cfg(windows)]

use std::ffi::c_void;

use victory_loader::prepare;
use victory_pe::layout::{
    DATA_DIRECTORY_SIZE, FILE_HEADER_CHARACTERISTICS, OPT_DLL_CHARACTERISTICS,
    data_directory_offset, dir, dll_flags, file_flags, file_header_offset, optional_header_offset,
};
use windows_sys::Win32::System::Memory::{
    MEM_FREE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualAlloc,
    VirtualFree, VirtualQuery,
};

mod common;
use common::fixture;

/// `IMAGE_OPTIONAL_HEADER64::ImageBase`.
const OPT_IMAGE_BASE: usize = 24;

/// title_quiet.dll as a linker writes an image that cannot be moved: no
/// relocation directory, `IMAGE_FILE_RELOCS_STRIPPED` set, not marked as
/// movable, and `preferred` as its base.
fn fixed_at(preferred: usize) -> Vec<u8> {
    let mut image = std::fs::read(fixture("title_quiet.dll")).unwrap();
    let relocations = data_directory_offset(&image, dir::BASERELOC).unwrap();
    image[relocations..relocations + DATA_DIRECTORY_SIZE].fill(0);
    fn patch16(image: &mut [u8], at: usize, change: impl Fn(u16) -> u16) {
        let old = u16::from_le_bytes([image[at], image[at + 1]]);
        image[at..at + 2].copy_from_slice(&change(old).to_le_bytes());
    }
    let optional = optional_header_offset(&image).unwrap();
    let characteristics = file_header_offset(&image).unwrap() + FILE_HEADER_CHARACTERISTICS;
    patch16(&mut image, characteristics, |flags| {
        flags | file_flags::RELOCS_STRIPPED
    });
    patch16(&mut image, optional + OPT_DLL_CHARACTERISTICS, |flags| {
        flags & !(dll_flags::DYNAMIC_BASE | dll_flags::HIGH_ENTROPY_VA)
    });
    image[optional + OPT_IMAGE_BASE..optional + OPT_IMAGE_BASE + 8]
        .copy_from_slice(&(preferred as u64).to_le_bytes());
    image
}

/// A range of `size` bytes that nothing uses, away from where the system
/// puts what it places itself; `nth` picks among them, so that each test
/// has its own.
fn free_range(size: usize, nth: usize) -> usize {
    let candidates = (0..64).map(|i| 0x5A00_0000_0000 + (nth * 64 + i) * 0x1000_0000);
    let free = |base: &usize| {
        // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid, and the query
        // reads the address space's bookkeeping only.
        unsafe {
            let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            VirtualQuery(*base as *const c_void, &mut info, size_of_val(&info)) != 0
                && info.State == MEM_FREE
                && info.RegionSize >= size
        }
    };
    let mut candidates = candidates.filter(free);
    candidates.next().expect("no free range for the image")
}

#[test]
fn stripped_image_loads_at_its_preferred_base_when_free() {
    let size = prepare(&fixed_at(0x1_8000_0000))
        .unwrap()
        .info
        .size_of_image as usize;
    let preferred = free_range(size, 0);
    let prepared = prepare(&fixed_at(preferred)).unwrap();
    assert!(!prepared.info.has_relocations);
    assert_eq!(prepared.info.image_base, preferred as u64);

    let cache = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, cache.path()).unwrap();
    assert_eq!(image.base(), preferred);
    // Its contents are there: the header, and an export where the file
    // says it is.
    // SAFETY: the first two bytes of a mapped image.
    assert_eq!(unsafe { (preferred as *const [u8; 2]).read() }, *b"MZ");
    let export = image.export("title_quiet_value").expect("the export");
    let listed = prepared.info.exports.iter();
    let rvas: Vec<_> = listed
        .filter(|e| e.name.as_deref() == Some("title_quiet_value"))
        .map(|e| e.rva)
        .collect();
    assert_eq!(rvas, [u32::try_from(export - preferred).ok()]);
    assert!(export - preferred < size);
    // SAFETY: none of its code ran, and no pointer into it is kept.
    unsafe { image.unload() };
}

#[test]
fn stripped_image_with_its_base_taken_fails_with_a_clear_error() {
    let size = prepare(&fixed_at(0x1_8000_0000))
        .unwrap()
        .info
        .size_of_image as usize;
    let preferred = free_range(size, 1);
    // SAFETY: reserves address space and touches no memory.
    let taken =
        unsafe { VirtualAlloc(preferred as *const c_void, size, MEM_RESERVE, PAGE_NOACCESS) };
    assert_eq!(taken as usize, preferred, "the range could not be reserved");

    let prepared = prepare(&fixed_at(preferred)).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let loaded = victory_loader::load(&prepared, cache.path());
    // SAFETY: the reservation made above, which nothing else used.
    assert_ne!(unsafe { VirtualFree(taken, 0, MEM_RELEASE) }, 0);

    let error = match loaded {
        Ok(image) => panic!("mapped at {:#x}", image.base()),
        Err(error) => format!("{error:#}"),
    };
    // It names the file, the reason and the address that was wanted.
    assert!(error.contains(&prepared.file_name()), "{error}");
    let cause = format!("image has no relocations; its preferred base {preferred:#x} may be taken");
    assert!(error.contains(&cause), "{error}");
}
