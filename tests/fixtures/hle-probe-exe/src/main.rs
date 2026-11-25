//! A stand-in for a title's executable, written by us: an entry point of
//! its own, static TLS reached the way an executable reaches it, a TLS
//! callback, threads it starts itself, and an import from a DLL that no PC
//! has. victory-loader's tests run it (crates/victory-loader/tests/load_exe.rs)
//! and read what it saw from the file `VICTORY_FIXTURE_LOG` names.
//!
//! It has no C runtime and no `std`: a runtime's start code ends in `exit`
//! and sets up state shared with the process that hosts it, and what a
//! runtime does at start differs between toolchains. Everything here is
//! ours, so the image is the same shape whoever links it. That includes the
//! TLS directory (`_tls_used`, the symbol both linkers take it from), which
//! a runtime would otherwise bring.
#![no_std]
#![no_main]

use core::arch::asm;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// What the thread-local `value` holds on a thread that has not written it.
const TEMPLATE_VALUE: u32 = 41;
/// What the main thread adds to its `value`, and the two threads to theirs.
const MAIN_ADDS: u32 = 1;
const WORKER_ADDS: [u32; 2] = [100, 200];
/// The entry point returns this plus what `XGameRuntimeInitialize` answered.
const EXIT_BASE: u32 = 7;
/// What it returns when it could not do its work, instead.
const FAILED_NO_LOG: u32 = 0xE001;
const FAILED_THREADS: u32 = 0xE002;
const FAILED_WRITE: u32 = 0xE003;
/// The process's exit code if this code panics (it has no reason to).
const PANICKED: u32 = 0xE0FF;

type Handle = *mut c_void;
type ThreadStart = unsafe extern "system" fn(*mut c_void) -> u32;
type TlsCallback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void);

// A console API: only our runtime (in the tests, their resolver) has it.
#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    fn XGameRuntimeInitialize() -> i32;
}

// By raw-dylib, so that linking this needs no import library of an SDK.
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn WriteFile(
        file: Handle,
        bytes: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetEnvironmentVariableW(name: *const u16, value: *mut u16, len: u32) -> u32;
    fn CreateThread(
        security: *const c_void,
        stack: usize,
        start: ThreadStart,
        parameter: *mut c_void,
        flags: u32,
        id: *mut u32,
    ) -> Handle;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    fn Sleep(milliseconds: u32);
    fn ExitProcess(code: u32) -> !;
}

/// This image's thread-local data: each thread gets a copy of [`TEMPLATE`].
#[repr(C)]
struct Local {
    value: u32,
    /// Written by nobody: a thread that finds something else here is not
    /// looking at a copy of the template.
    guard: u32,
}

const GUARD: u32 = 0x7E57_1E55;

/// The TLS template, in the section executables keep it in.
#[used]
#[unsafe(link_section = ".tls$")]
static mut TEMPLATE: Local = Local {
    value: TEMPLATE_VALUE,
    guard: GUARD,
};

/// Where the OS writes the index it gave this image's TLS.
static mut TLS_INDEX: u32 = 0;

/// `IMAGE_TLS_DIRECTORY64`.
#[repr(C)]
struct TlsDirectory {
    start: *const Local,
    end: *const Local,
    index: *mut u32,
    callbacks: *const Option<TlsCallback>,
    zero_fill: u32,
    characteristics: u32,
}

// SAFETY: read-only after linking; the pointers are addresses of statics.
unsafe impl Sync for TlsDirectory {}

static TLS_CALLBACKS: [Option<TlsCallback>; 2] = [Some(on_tls), None];

/// The image's TLS directory: both linkers point the header's TLS entry at
/// the symbol of this name.
#[used]
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
static _tls_used: TlsDirectory = TlsDirectory {
    start: &raw const TEMPLATE,
    // SAFETY: one past the end of the template, which is one `Local`.
    end: unsafe { (&raw const TEMPLATE).add(1) },
    index: &raw mut TLS_INDEX,
    callbacks: TLS_CALLBACKS.as_ptr(),
    zero_fill: 0,
    characteristics: 0,
};

/// This thread's copy of the template, reached as an executable's code
/// reaches it: the TLS array from the TEB, then its first entry, because an
/// executable's TLS index is always 0. No read of the index.
///
/// Mapped as a DLL the image has another index, and entry 0 is some other
/// image's block; the loader's redirect is what makes this right.
#[inline(never)]
fn local() -> *mut Local {
    let block: *mut Local;
    // SAFETY: reads TEB.ThreadLocalStoragePointer and the first pointer of
    // the array it names. Not `nostack`: the redirect turns the first
    // instruction into a call.
    unsafe {
        asm!(
            "mov {block}, gs:[0x58]",
            "mov {block}, [{block}]",
            block = out(reg) block,
            options(readonly, preserves_flags),
        );
    }
    block
}

/// The calls of the TLS callback, in order: reason in the low byte, whether
/// its third argument was not null in bit 8, the thread's id in the high
/// half.
static CALLS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The log file, once the entry point has opened it.
static LOG: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

const DLL_PROCESS_DETACH: u32 = 0;

/// A TLS callback, as titles have. It keeps what it was called with; told
/// that the process is ending, it writes all of it to the log.
unsafe extern "system" fn on_tls(_: *mut c_void, reason: u32, reserved: *mut c_void) {
    // SAFETY: a plain query of the calling thread.
    let thread = unsafe { GetCurrentThreadId() };
    let call = u64::from(thread) << 32 | u64::from(!reserved.is_null()) << 8 | u64::from(reason);
    if let Some(slot) = CALLS.get(CALL_COUNT.fetch_add(1, Ordering::SeqCst)) {
        slot.store(call, Ordering::SeqCst);
    }
    if reason != DLL_PROCESS_DETACH {
        return;
    }
    let count = CALL_COUNT.load(Ordering::SeqCst);
    let mut text = Text::new();
    text.pair("callback_count", count as u64);
    write_log(&text);
    for slot in CALLS.iter().take(count) {
        let call = slot.load(Ordering::SeqCst);
        let mut text = Text::new();
        text.str("callback=");
        text.number(call & 0xFF);
        text.str(",");
        text.number(call >> 8 & 1);
        text.str(",");
        text.number(call >> 32);
        text.str("\n");
        write_log(&text);
    }
    // The last line: a log that ends here was written to the end.
    let mut text = Text::new();
    text.str("detached=1\n");
    write_log(&text);
}

/// What a thread this image started saw.
struct Worker {
    thread: AtomicU32,
    initial: AtomicU32,
    guard: AtomicU32,
    last: AtomicU32,
    block: AtomicUsize,
}

static WORKERS: [Worker; 2] = [const {
    Worker {
        thread: AtomicU32::new(0),
        initial: AtomicU32::new(0),
        guard: AtomicU32::new(0),
        last: AtomicU32::new(0),
        block: AtomicUsize::new(0),
    }
}; 2];

/// Threads that have their block and have not ended yet.
static HOLDING: AtomicU32 = AtomicU32::new(0);

/// A thread of the title's own: reads its thread-local, adds to it, and
/// stays until the other one holds its block too, so that the two blocks
/// exist at the same time.
unsafe extern "system" fn worker(parameter: *mut c_void) -> u32 {
    let which = parameter as usize;
    let (Some(seen), Some(adds)) = (WORKERS.get(which), WORKER_ADDS.get(which)) else {
        return 1;
    };
    let block = local();
    // SAFETY: this thread's block, as large as the template.
    let (initial, guard) = unsafe { ((*block).value, (*block).guard) };
    // SAFETY: as above; only this thread uses its block.
    unsafe { (*block).value = initial.wrapping_add(*adds) };
    // SAFETY: a plain query of the calling thread.
    seen.thread
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    seen.initial.store(initial, Ordering::SeqCst);
    seen.guard.store(guard, Ordering::SeqCst);
    seen.block.store(block as usize, Ordering::SeqCst);
    HOLDING.fetch_add(1, Ordering::SeqCst);
    while (HOLDING.load(Ordering::SeqCst) as usize) < WORKERS.len() {
        // SAFETY: gives the processor away for a moment.
        unsafe { Sleep(1) };
    }
    // Read again through the TLS array: what this thread wrote, whatever
    // the others did in between.
    // SAFETY: this thread's block.
    seen.last
        .store(unsafe { (*local()).value }, Ordering::SeqCst);
    0
}

/// The entry point the header names (see build.rs): no arguments, and its
/// result is the process's exit code.
#[unsafe(no_mangle)]
pub extern "system" fn probe_exe_entry() -> u32 {
    let Some(log) = open_log() else {
        return FAILED_NO_LOG;
    };
    LOG.store(log, Ordering::SeqCst);

    // SAFETY: the import takes nothing; the loader bound it.
    let runtime = unsafe { XGameRuntimeInitialize() };

    let block = local();
    // SAFETY: this thread's block, as large as the template.
    let (initial, guard) = unsafe { ((*block).value, (*block).guard) };
    // SAFETY: as above.
    unsafe { (*block).value = initial.wrapping_add(MAIN_ADDS) };

    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: writes the two bounds of this thread's stack.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };

    let mut threads = [core::ptr::null_mut(); 2];
    for (which, thread) in threads.iter_mut().enumerate() {
        // SAFETY: `worker` takes its number as the parameter.
        *thread = unsafe {
            CreateThread(
                core::ptr::null(),
                0,
                worker,
                which as *mut c_void,
                0,
                core::ptr::null_mut(),
            )
        };
    }
    if threads.iter().any(|t| t.is_null()) {
        // One of them would wait for the other for ever.
        HOLDING.store(WORKERS.len() as u32, Ordering::SeqCst);
        return FAILED_THREADS;
    }
    for thread in threads {
        // SAFETY: a thread handle of ours, waited for without a time limit
        // and then closed.
        unsafe {
            WaitForSingleObject(thread, u32::MAX);
            CloseHandle(thread);
        }
    }

    let mut text = Text::new();
    // SAFETY: a plain query of the calling thread.
    text.pair("main_thread", unsafe { GetCurrentThreadId() }.into());
    text.pair("runtime", runtime as u32 as u64);
    text.pair("main_initial", initial.into());
    text.pair("main_guard", guard.into());
    // SAFETY: this thread's block; the other threads have ended.
    text.pair("main_last", unsafe { (*local()).value }.into());
    text.pair("main_block", block as u64);
    text.pair("stack_low", low as u64);
    text.pair("stack_high", high as u64);
    let mut written = write_log(&text);
    for (seen, name) in WORKERS.iter().zip(["a", "b"]) {
        let mut text = Text::new();
        let mut pair = |what: &str, value: u64| {
            text.str(name);
            text.str("_");
            text.pair(what, value);
        };
        pair("thread", seen.thread.load(Ordering::SeqCst).into());
        pair("initial", seen.initial.load(Ordering::SeqCst).into());
        pair("guard", seen.guard.load(Ordering::SeqCst).into());
        pair("last", seen.last.load(Ordering::SeqCst).into());
        pair("block", seen.block.load(Ordering::SeqCst) as u64);
        written &= write_log(&text);
    }
    if !written {
        return FAILED_WRITE;
    }
    EXIT_BASE.wrapping_add(runtime as u32)
}

/// `VICTORY_FIXTURE_LOG`, NUL-terminated, in UTF-16.
const LOG_VARIABLE: [u16; 20] = {
    let ascii = b"VICTORY_FIXTURE_LOG\0";
    let mut wide = [0u16; 20];
    let mut i = 0;
    while i < ascii.len() {
        wide[i] = ascii[i] as u16;
        i += 1;
    }
    wide
};

/// Opens the file `VICTORY_FIXTURE_LOG` names, to add to its end.
fn open_log() -> Option<Handle> {
    const FILE_APPEND_DATA: u32 = 4;
    const FILE_SHARE_READ_WRITE: u32 = 3;
    const OPEN_ALWAYS: u32 = 4;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    static mut PATH: [u16; 1024] = [0; 1024];
    let path = &raw mut PATH;
    // SAFETY: only the entry point calls this, once; the buffer is as long
    // as the call is told, and a longer value is refused below.
    let len = unsafe { GetEnvironmentVariableW(LOG_VARIABLE.as_ptr(), path.cast(), 1024) };
    if len == 0 || len >= 1024 {
        return None;
    }
    // SAFETY: a NUL-terminated path, as the call above left it.
    let file = unsafe {
        CreateFileW(
            path.cast(),
            FILE_APPEND_DATA,
            FILE_SHARE_READ_WRITE,
            core::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    // INVALID_HANDLE_VALUE is -1.
    (!file.is_null() && file as isize != -1).then_some(file)
}

/// Adds `text` to the log; false if there is no log or it took less.
fn write_log(text: &Text) -> bool {
    let log = LOG.load(Ordering::SeqCst);
    if log.is_null() || text.truncated {
        return false;
    }
    let mut written = 0;
    // SAFETY: an open file and `len` bytes of ours.
    let ok = unsafe {
        WriteFile(
            log,
            text.bytes.as_ptr(),
            text.len as u32,
            &mut written,
            core::ptr::null_mut(),
        )
    };
    ok != 0 && written as usize == text.len
}

/// A few lines of `name=number`, without a heap.
struct Text {
    bytes: [u8; 512],
    len: usize,
    truncated: bool,
}

impl Text {
    fn new() -> Text {
        Text {
            bytes: [0; 512],
            len: 0,
            truncated: false,
        }
    }

    fn byte(&mut self, byte: u8) {
        match self.bytes.get_mut(self.len) {
            Some(at) => {
                *at = byte;
                self.len += 1;
            }
            None => self.truncated = true,
        }
    }

    fn str(&mut self, text: &str) {
        for byte in text.bytes() {
            self.byte(byte);
        }
    }

    /// `value` in decimal.
    fn number(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut rest = value;
        loop {
            if let Some(digit) = digits.get_mut(count) {
                *digit = b'0' + (rest % 10) as u8;
            }
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        for digit in digits.iter().take(count).rev() {
            self.byte(*digit);
        }
    }

    fn pair(&mut self, name: &str, value: u64) {
        self.str(name);
        self.str("=");
        self.number(value);
        self.str("\n");
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: ends the process; nothing here can go on after a panic.
    unsafe { ExitProcess(PANICKED) }
}

/// `core`'s panic paths name this in their unwind data on the MinGW
/// targets, and `std`, which is not here, defines it. Nothing unwinds
/// (the profile says `panic = "abort"`), so nothing calls it.
#[cfg(not(target_env = "msvc"))]
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}
