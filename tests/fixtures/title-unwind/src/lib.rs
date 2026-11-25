//! A stand-in title that unwinds (`title_unwind.dll`), written by us: it
//! panics and catches the panic, calls back out of a frame of its own,
//! writes to its own constants and throws the way MSVC's C++ runtime does.
//! The loader's tests run it as a prepared image, to see that unwinding and
//! crash reports work through title code that the Windows loader mapped
//! and moved.
//!
//! The exports a test starts a thread at, or that an exception leaves
//! through, are `extern "system-unwind"`: Rust ends the process itself when
//! anything unwinds out of a plain `extern "system"` function, and these
//! tests are about what the system and our crash reporter do.

use std::ffi::{CStr, c_void};
use std::hint::black_box;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// 1 if this DLL was built to unwind on a panic, 0 if to abort (a build
/// with the wrong profile): `unwind_catch` ends the process then.
#[unsafe(no_mangle)]
pub extern "system" fn unwind_strategy() -> u32 {
    u32::from(cfg!(panic = "unwind"))
}

/// Counts in `drops` when a frame it lives in is left.
struct Counted<'a>(&'a AtomicU32);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// `depth + 1` frames of this function, each with something to drop and
/// some stack of its own; the innermost panics with `payload`.
#[inline(never)]
fn descend(depth: u32, payload: u32, drops: &AtomicU32) -> u32 {
    let _counted = Counted(drops);
    let mut frame = [0u8; 64];
    black_box(&mut frame);
    if depth == 0 {
        std::panic::panic_any(payload);
    }
    // Not a tail call: the frame stays until the callee is done.
    black_box(descend(depth - 1, payload, drops)) + u32::from(frame[0])
}

/// Panics `depth + 1` frames down and catches the panic here, all in this
/// image. Returns the frames that were unwound (their destructors ran) in
/// the high half and the panic's payload in the low half.
#[unsafe(no_mangle)]
pub extern "system" fn unwind_catch(depth: u32, payload: u32) -> u64 {
    // This image's own runtime, so its own hook: the panic is expected and
    // need not be printed.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));
    let drops = AtomicU32::new(0);
    let caught = std::panic::catch_unwind(|| descend(depth, payload, &drops));
    let value = match caught {
        Ok(_) => 0,
        Err(thrown) => thrown.downcast_ref::<u32>().copied().unwrap_or(u32::MAX),
    };
    u64::from(drops.load(Ordering::SeqCst)) << 32 | u64::from(value)
}

/// What `unwind_callback` returns once the callback has.
const CALLED: u32 = 0xCA11_BAC0;

/// Calls `callback(argument)` from a frame of this image that has locals
/// and saved registers, so a stack walk from inside the callback needs this
/// image's unwind data to get past it.
#[unsafe(no_mangle)]
pub extern "system-unwind" fn unwind_callback(
    callback: extern "system-unwind" fn(*mut c_void),
    argument: *mut c_void,
) -> u32 {
    let mut frame = [0u32; 32];
    black_box(&mut frame);
    callback(argument);
    CALLED + black_box(frame[0])
}

/// A constant, in a read-only page of the image.
static TARGET: u32 = 0x5EED_0001;

struct Pointer(*const u32);
// SAFETY: a constant address, never written.
unsafe impl Sync for Pointer {}

/// The address of `TARGET` as data: 8 bytes that hold an address, which
/// only a base relocation makes right when the image is not mapped at the
/// base it was linked for.
static POINTER: Pointer = Pointer(&raw const TARGET);

/// Where `POINTER` is.
#[unsafe(no_mangle)]
pub extern "system" fn unwind_pointer() -> *const *const u32 {
    black_box(&raw const POINTER.0)
}

/// Where `TARGET` is, as the code computes it (relative to itself).
#[unsafe(no_mangle)]
pub extern "system" fn unwind_target() -> *const u32 {
    black_box(&raw const TARGET)
}

/// The faulting store, in a frame of its own under `unwind_fault`.
#[inline(never)]
fn store(at: *mut u32, value: u32) -> u32 {
    // SAFETY: none; this is the access violation the tests ask for.
    unsafe {
        at.write_volatile(value);
        at.read_volatile()
    }
}

/// Writes to `TARGET`, which is read-only: an access violation in title
/// code. A thread can start here.
#[unsafe(no_mangle)]
pub extern "system-unwind" fn unwind_fault(_: *mut c_void) -> u32 {
    let at = black_box(&raw const TARGET).cast_mut();
    // Not a tail call: this frame is in the report too.
    black_box(store(at, 1)) + 1
}

#[link(name = "kernel32")]
unsafe extern "system-unwind" {
    fn RaiseException(code: u32, flags: u32, count: u32, arguments: *const usize);
}

unsafe extern "C" {
    /// The start of this image, from the linker (MSVC's and lld both).
    static __ImageBase: u8;
}

/// The exception code of a C++ throw (`0xE0000000 | "msc"`), and the magic
/// number that is its first parameter.
const CPP_EXCEPTION: u32 = 0xE06D_7363;
const CPP_MAGIC: usize = 0x1993_0520;
const EXCEPTION_NONCONTINUABLE: u32 = 1;

// What MSVC's compiler emits for a `throw` on x64, as its runtime's public
// header (ehdata.h) declares it and as the crash reporter reads it: every
// reference is a 32-bit offset from the start of the image.

#[repr(C)]
struct ThrowInfo {
    attributes: u32,
    unwind: u32,
    forward_compat: u32,
    catchable_types: u32,
}

const MAX_TYPES: usize = 2;

#[repr(C)]
struct CatchableTypes {
    count: u32,
    types: [u32; MAX_TYPES],
}

#[repr(C)]
struct CatchableType {
    properties: u32,
    descriptor: u32,
    member_displacement: [i32; 3],
    size: u32,
    copy: u32,
}

const NAME_BYTES: usize = 32;

#[repr(C)]
struct TypeDescriptor {
    vftable: usize,
    spare: usize,
    name: [u8; NAME_BYTES],
}

struct Tables {
    throw_info: ThrowInfo,
    array: CatchableTypes,
    types: [CatchableType; MAX_TYPES],
    descriptors: [TypeDescriptor; MAX_TYPES],
}

const NO_TYPE: CatchableType = CatchableType {
    properties: 0,
    descriptor: 0,
    member_displacement: [0, -1, 0],
    size: 0,
    copy: 0,
};
const NO_DESCRIPTOR: TypeDescriptor = TypeDescriptor {
    vftable: 0,
    spare: 0,
    name: [0; NAME_BYTES],
};

/// The tables of the one throw this image makes, in the image's own data
/// (a throw's tables are found by offsets from its image) and filled in
/// when it throws: Rust has no way to write such an offset into a constant.
static TABLES: Mutex<Tables> = Mutex::new(Tables {
    throw_info: ThrowInfo {
        attributes: 0,
        unwind: 0,
        forward_compat: 0,
        catchable_types: 0,
    },
    array: CatchableTypes {
        count: 0,
        types: [0; MAX_TYPES],
    },
    types: [NO_TYPE; MAX_TYPES],
    descriptors: [NO_DESCRIPTOR; MAX_TYPES],
});

/// Throws `object`, of the types with the decorated `names` (most derived
/// first), as `_CxxThrowException` does. One throw at a time.
fn throw(object: *const c_void, size: u32, names: &[&CStr]) {
    let base = &raw const __ImageBase as usize;
    let rva = |p: usize| u32::try_from(p - base).expect("the tables are in this image");
    let throw_info = {
        let mut guard = TABLES.lock().unwrap_or_else(|e| e.into_inner());
        let tables = &mut *guard;
        for (i, name) in names.iter().enumerate() {
            let bytes = name.to_bytes_with_nul();
            tables.descriptors[i].name = [0; NAME_BYTES];
            tables.descriptors[i].name[..bytes.len()].copy_from_slice(bytes);
            tables.types[i] = CatchableType {
                descriptor: rva(&raw const tables.descriptors[i] as usize),
                size,
                ..NO_TYPE
            };
            tables.array.types[i] = rva(&raw const tables.types[i] as usize);
        }
        tables.array.count = names.len() as u32;
        tables.throw_info.catchable_types = rva(&raw const tables.array as usize);
        &raw const tables.throw_info as usize
    };
    let arguments = [CPP_MAGIC, object as usize, throw_info, base];
    // SAFETY: four parameters, as counted; nothing here is borrowed past
    // the call, which does not come back.
    unsafe {
        RaiseException(
            CPP_EXCEPTION,
            EXCEPTION_NONCONTINUABLE,
            arguments.len() as u32,
            arguments.as_ptr(),
        )
    };
}

/// What `unwind_throw(0)` throws: an `int`, as a title throws an HRESULT.
const THROWN_INT: u32 = 0x8007_0057;
/// What `unwind_throw(1)` throws says.
const THROWN_MESSAGE: &CStr = c"boom thrown";

/// Throws like C++ code built by MSVC, and nobody catches it: `kind` 0 an
/// `int`, anything else a `std::runtime_error`. A thread can start here.
#[unsafe(no_mangle)]
pub extern "system-unwind" fn unwind_throw(kind: *mut c_void) -> u32 {
    if kind.is_null() {
        let object = THROWN_INT;
        throw((&raw const object).cast(), 4, &[c".H"]);
    } else {
        // A `std::exception`: its table pointer (unused here), then its
        // message.
        let object = [0usize, THROWN_MESSAGE.as_ptr() as usize];
        throw(
            (&raw const object).cast(),
            16,
            &[c".?AVruntime_error@std@@", c".?AVexception@std@@"],
        );
    }
    0
}
