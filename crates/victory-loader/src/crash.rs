//! Names the faulting code when title code crashes.
//!
//! A vectored handler sees every exception first, so it only prints the
//! kinds that end a process (access violations and the like, and
//! breakpoints: titles assert with `int3`, fatal without a debugger) as
//! first-chance, since a handler may still take them (ntdll's loader does
//! with its own probes). What nobody handles, of any kind (an uncaught C++
//! exception too), is reported from the unhandled-exception filter
//! (`report_unhandled`, which the runtime calls ahead of the title's own)
//! and appended to the crash log, with the title's last debug output, which
//! often states the reason. Panics of ours go to the crash log too.

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_BREAKPOINT, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION, EXCEPTION_STACK_OVERFLOW, HMODULE,
};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS, M128A, RtlLookupFunctionEntry,
    RtlVirtualUnwind, SetUnhandledExceptionFilter, UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
/// Frames listed per report.
pub(crate) const MAX_FRAMES: usize = 40;

/// Memory saved to files when a crash is reported: (address, length, file).
#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> =
    std::sync::OnceLock::new();

/// Debugging aid: on a reported crash, also write each `[address, address
/// + len)` (readable title memory, e.g. an engine table) to its file.
#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

/// The file crash reports are appended to, besides being printed.
static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Also appends every unhandled-exception report and panic to `file`, so a
/// crash can be read after the console is gone.
pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "VIctory: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

/// Appends a failure that is not an exception (the host GPU's) to the crash
/// log, with the title's last debug output, and prints it.
pub fn log_crash(text: &str) {
    let text = format!(
        "{text}
{}",
        title_output()
    );
    eprint!("{text}");
    append_to_log(&text);
}

fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file);
    if let Ok(mut f) = opened {
        let _ = write!(
            f,
            "unix time {now}, process {}\n{text}\n",
            std::process::id()
        );
    }
}

/// The title's last lines of debug output, kept for crash reports.
static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

/// Keeps a line of the title's debug output for the next crash report.
pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

/// The kept lines, one per report line; nothing if another thread holds
/// them (it may be the one that faulted).
fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter()
        .map(|l| format!("  title said: {l}\n"))
        .collect()
}

/// Installs the handlers; call once before running title code.
pub fn report_crashes() {
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        unsafe { report_unhandled(info) };
        EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: registers handlers that only read the exception they are
    // given; the title's own filter, once it sets one, takes the place of
    // ours and calls `report_unhandled` first.
    unsafe {
        AddVectoredExceptionHandler(0, Some(handler));
        SetUnhandledExceptionFilter(Some(filter));
    }
}

/// What the vectored handler reports as it happens.
const FATAL: [i32; 6] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_BREAKPOINT,
];

/// The longest instruction is 15 bytes; a [`ClwbLoop`] is 14.
const CODE_BYTES: usize = 32;

/// The protection of the committed page `address` is in; `None` where no
/// memory is committed. Reads the address space's bookkeeping, never the
/// address.
fn protection_at(address: usize) -> Option<u32> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: writes `info`, whose size is passed.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    (n != 0 && info.State == MEM_COMMIT).then_some(info.Protect)
}

/// Whether reading the page `address` is in faults.
fn page_is_readable(address: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_NOACCESS};
    protection_at(address).is_some_and(|p| p & (PAGE_GUARD | PAGE_NOACCESS) == 0 && p & 0xff != 0)
}

/// Whether `clzero` may write the page `address` is in: writable memory, or
/// a guard page, where the write raises what the instruction would have
/// (a stack grows that way).
fn page_takes_a_write(address: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    const WRITABLE: u32 =
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    protection_at(address).is_some_and(|p| p & (WRITABLE | PAGE_GUARD) != 0)
}

/// The code bytes at `rip` and how many of them there are: up to
/// [`CODE_BYTES`], but none from the page after `rip`'s unless that page
/// can be read. An instruction may end with its page and the next page not
/// be mapped; one that runs on into the next page was fetched from both.
///
/// # Safety
/// `rip`'s page is readable (the CPU just fetched the instruction there).
unsafe fn code_at(rip: usize) -> ([u8; CODE_BYTES], usize) {
    let room = 0x1000 - (rip & 0xfff);
    let n = match rip.checked_add(room) {
        _ if room >= CODE_BYTES => CODE_BYTES,
        Some(next) if page_is_readable(next) => CODE_BYTES,
        _ => room,
    };
    let mut bytes = [0u8; CODE_BYTES];
    // SAFETY: `n` bytes from `rip` are in its page, which the caller says
    // is readable, or in the next, which was just found readable.
    unsafe { std::ptr::copy_nonoverlapping(rip as *const u8, bytes.as_mut_ptr(), n) };
    (bytes, n)
}

/// Console CPUs are Zen 2: titles use AMD-only instructions Intel CPUs
/// lack. Does their job instead: `clzero` (`0f 01 fc`) zeroes the 64-byte
/// line at `rax`; `monitorx`/`mwaitx` (`0f 01 fa`/`fb`) are wait hints, so
/// a spin loop around them works with mwaitx as a yield.
///
/// Everything it decodes and every register it uses is the title's: an
/// instruction it cannot read whole, or cannot do without faulting itself,
/// is not emulated (`false`), and is then reported as the illegal
/// instruction it is.
unsafe fn emulate_amd_only(info: *mut EXCEPTION_POINTERS) -> bool {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
    if record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION {
        return false;
    }
    // SAFETY: the CPU just fetched the faulting instruction from `Rip`.
    let (bytes, n) = unsafe { code_at(context.Rip as usize) };
    let code = &bytes[..n];
    let len = match code {
        [0x0f, 0x01, 0xfc, ..] => {
            let line = (context.Rax & !63) as usize;
            if !page_takes_a_write(line) {
                return false;
            }
            // SAFETY: a line is inside one page, which is writable (or a
            // guard page: see `page_takes_a_write`).
            unsafe { (line as *mut u8).write_bytes(0, 64) };
            3
        }
        [0x0f, 0x01, 0xfa, ..] => 3,
        [0x0f, 0x01, 0xfb, ..] => {
            std::thread::yield_now();
            3
        }
        _ => {
            if let Some(pass) = clwb_loop(code) {
                // A whole write-back loop in one exception (see `clwb_loop`).
                let limit = *gpr(context, pass.limit);
                let base = gpr(context, pass.base);
                let Some(end) = clwb_loop_end(*base, limit) else {
                    return false;
                };
                *base = end;
                let flags = cmp_flags(end, limit);
                context.EFlags = context.EFlags & !CMP_FLAGS | flags;
                pass.len
            } else if let Some(len) = clwb_len(code) {
                // A write-back hint: the PC's memory is coherent, so it has nothing to do.
                len
            } else {
                // SAFETY: the saved floating-point state is plain data.
                let xmm = unsafe { &mut context.Anonymous.FltSave.XmmRegisters };
                let Some(len) = sse4a(code, xmm) else {
                    return false;
                };
                len
            }
        }
    };
    context.Rip = context.Rip.wrapping_add(len as u64);
    true
}

/// `clwb m8` (`66 [rex] 0f ae /6`, a memory operand), which Intel CPUs before
/// Ice Lake lack: its length, or `None` for any other instruction and for
/// one `code` does not hold whole.
fn clwb_len(code: &[u8]) -> Option<usize> {
    let at = if code.get(1)? & 0xf0 == 0x40 { 2 } else { 1 };
    if code[0] != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let (mode, rm) = (modrm >> 6, modrm & 7);
    if mode == 3 || (modrm >> 3) & 7 != 6 {
        return None;
    }
    let mut len = at + 3;
    let mut base = rm;
    if rm == 4 {
        base = code.get(len)? & 7;
        len += 1;
    }
    len += match mode {
        0 if rm == 5 || (rm == 4 && base == 5) => 4,
        1 => 1,
        2 => 4,
        _ => 0,
    };
    (len <= code.len()).then_some(len)
}

/// A `clwb` write-back loop as the title's compilers build it, one cache line a
/// pass (`base` and `limit` any registers):
///
/// ```text
/// top: clwb [base]
///      add  base, 0x40
///      cmp  base, limit
///      jbe  top
/// ```
///
/// An exception costs about a microsecond and the title writes back whole
/// buffers this way while it loads, so the loop is done in one: `clwb` has
/// nothing to do on the PC, `base` ends past `limit` as the last pass leaves it.
struct ClwbLoop {
    base: u8,
    limit: u8,
    /// Bytes from `clwb` to the instruction after `jbe`.
    len: usize,
}

fn clwb_loop(code: &[u8]) -> Option<ClwbLoop> {
    // clwb [base]: 66 [40|41] 0f ae, ModRM mod 00, reg 6, no SIB or displacement.
    let (rex, at) = match code.get(1)? {
        0x40 | 0x41 => (code[1], 2),
        _ => (0, 1),
    };
    if *code.first()? != 0x66 || code.get(at..at + 2)? != [0x0f, 0xae] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    let rm = modrm & 7;
    if modrm >> 6 != 0 || (modrm >> 3) & 7 != 6 || rm == 4 || rm == 5 {
        return None;
    }
    let base = rm | (rex & 1) << 3;
    // add base, 0x40: REX.W(+B) 83 /0 ib.
    let add = at + 3;
    if *code.get(add..add + 4)? != [0x48 | base >> 3, 0x83, 0xc0 | (base & 7), 0x40] {
        return None;
    }
    // cmp base, limit: REX.W 39 /r with base in r/m.
    let cmp = add + 4;
    let &[cmp_rex @ 0x48..=0x4f, 0x39, cmp_modrm] = code.get(cmp..cmp + 3)? else {
        return None;
    };
    if cmp_modrm >> 6 != 3 || (cmp_modrm & 7 | (cmp_rex & 1) << 3) != base {
        return None;
    }
    let limit = (cmp_modrm >> 3) & 7 | (cmp_rex & 4) << 1;
    // jbe top.
    let jump = cmp + 3;
    let len = jump + 2;
    if *code.get(jump)? != 0x76 || i64::from(*code.get(jump + 1)? as i8) != -(len as i64) {
        return None;
    }
    (limit != base).then_some(ClwbLoop { base, limit, len })
}

/// `base` as a [`ClwbLoop`] leaves it: the first `base + 64 * k` (`k` at
/// least 1, every loop makes one pass) above `limit`. `None` where that is
/// past the end of the address space: there `base` wraps to a low address
/// and the real loop goes on from it, which is not a write-back of a buffer
/// and is not emulated. The registers are the title's, so no step of this
/// may overflow.
fn clwb_loop_end(base: u64, limit: u64) -> Option<u64> {
    let passes = match limit.checked_sub(base) {
        Some(distance) => distance / 64 + 1,
        None => 1,
    };
    base.checked_add(passes.checked_mul(64)?)
}

/// The general register numbered `n` as instructions encode it.
fn gpr(context: &mut CONTEXT, n: u8) -> &mut u64 {
    match n & 15 {
        0 => &mut context.Rax,
        1 => &mut context.Rcx,
        2 => &mut context.Rdx,
        3 => &mut context.Rbx,
        4 => &mut context.Rsp,
        5 => &mut context.Rbp,
        6 => &mut context.Rsi,
        7 => &mut context.Rdi,
        8 => &mut context.R8,
        9 => &mut context.R9,
        10 => &mut context.R10,
        11 => &mut context.R11,
        12 => &mut context.R12,
        13 => &mut context.R13,
        14 => &mut context.R14,
        _ => &mut context.R15,
    }
}

/// The status flags `cmp a, b` leaves (CF, PF, AF, ZF, SF, OF) as `EFLAGS` bits.
fn cmp_flags(a: u64, b: u64) -> u32 {
    let r = a.wrapping_sub(b);
    u32::from(a < b)
        | u32::from((r as u8).count_ones().is_multiple_of(2)) << 2
        | (((a ^ b ^ r) >> 4) as u32 & 1) << 4
        | u32::from(r == 0) << 6
        | ((r >> 63) as u32) << 7
        | ((((a ^ b) & (a ^ r)) >> 63) as u32) << 11
}
const CMP_FLAGS: u32 = 0x8d5;

/// SSE4a's `extrq`/`insertq` (register forms, the only ones there are) on
/// the saved XMM registers; returns the instruction's length. `None` for
/// any other instruction and for one `code` does not hold whole; `xmm` is
/// then untouched.
fn sse4a(code: &[u8], xmm: &mut [M128A; 16]) -> Option<usize> {
    let b = |i: usize| code.get(i).copied();
    let prefix = b(0)?;
    let (rex, at) = match b(1)? {
        rex if rex & 0xf0 == 0x40 => (rex, 2),
        _ => (0, 1),
    };
    let opcode = b(at + 1)?;
    if !matches!(prefix, 0x66 | 0xf2) || b(at)? != 0x0f || !matches!(opcode, 0x78 | 0x79) {
        return None;
    }
    let modrm = b(at + 2)?;
    if modrm >> 6 != 3 {
        return None;
    }
    let dst = usize::from((modrm >> 3) & 7 | (rex & 4) << 1);
    let src = usize::from(modrm & 7 | (rex & 1) << 3);
    let immediate = opcode == 0x78;
    let (fields, len) = if immediate {
        ((b(at + 3)?, b(at + 4)?), at + 5)
    } else {
        // extrq reads its fields from src's low half, insertq from its high.
        let f = if prefix == 0x66 {
            xmm[src].Low
        } else {
            xmm[src].High as u64
        };
        ((f as u8, (f >> 8) as u8), at + 3)
    };
    let (n, at_bit) = (u32::from(fields.0 & 63), u32::from(fields.1 & 63));
    let bits = if n == 0 { 64 } else { n };
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if prefix == 0x66 {
        // extrq: register form takes its fields from src, works on dst.
        if immediate && (modrm >> 3) & 7 != 0 {
            return None;
        }
        let target = if immediate { src } else { dst };
        xmm[target].Low = (xmm[target].Low >> at_bit) & mask;
    } else {
        let value = (xmm[src].Low & mask) << at_bit;
        xmm[dst].Low = xmm[dst].Low & !(mask << at_bit) | value;
    }
    Some(len)
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers.
    if unsafe { emulate_amd_only(info) } {
        return -1; // EXCEPTION_CONTINUE_EXECUTION
    }
    // SAFETY: as above.
    if FATAL.contains(&unsafe { (*(*info).ExceptionRecord).ExceptionCode }) {
        // SAFETY: as above.
        unsafe { report("first-chance exception", info, false) };
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// For an unhandled-exception filter: reports the exception that is about
/// to end the process, and appends it to the crash log.
///
/// # Safety
/// `info` comes from the system, for an exception being dispatched.
pub unsafe fn report_unhandled(info: *const EXCEPTION_POINTERS) {
    // SAFETY: forwarded from the caller.
    unsafe { report("unhandled exception", info, true) };
}

/// The bytes at `at` as hex, as many as the longest x86 instruction (15),
/// but none past the end of `at`'s page: the next page may not be mapped.
/// For an instruction the CPU refused, they name it (`0f 01 fd` is
/// `rdpru`), which the address alone does not.
///
/// # Safety
/// `at`'s page is readable.
unsafe fn instruction_bytes(at: usize) -> String {
    let n = (0x1000 - (at & 0xfff)).min(15);
    // SAFETY: `n` bytes from `at` stay inside its page, which the caller
    // says is readable.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, n) };
    let hex: Vec<_> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(" ")
}

/// MSVC's code for a C++ `throw`, and its magic first parameter.
const CPP_EXCEPTION: i32 = 0xE06D_7363_u32 as i32;
const CPP_MAGIC: usize = 0x1993_0520;

/// Catchable types looked at for `std::exception`; a class has one for
/// itself and one for each base it can be caught as.
const MAX_CATCHABLE_TYPES: usize = 64;
/// Bytes of a type name or a message quoted in a report.
const MAX_TEXT: usize = 256;

/// The `T` (no larger than a page) at `address`, if all of it can be read.
fn peek<T: Copy>(address: usize) -> Option<T> {
    let last = address.checked_add(size_of::<T>().checked_sub(1)?)?;
    if !page_is_readable(address) || !page_is_readable(last) {
        return None;
    }
    // SAFETY: the pages holding its first and last byte were just found
    // readable; a title thread unmapping them meanwhile is not ruled out,
    // as for everything a crash report reads.
    Some(unsafe { (address as *const T).read_unaligned() })
}

/// The NUL-terminated text at `address`, cut (and marked `...`) after
/// `limit` bytes; `None` if it runs into memory that cannot be read.
fn text_at(address: usize, limit: usize) -> Option<String> {
    let mut bytes = Vec::new();
    for i in 0..limit {
        let at = address.checked_add(i)?;
        if (i == 0 || at & 0xfff == 0) && !page_is_readable(at) {
            return None;
        }
        // SAFETY: `at`'s page was found readable when the text entered it.
        match unsafe { (at as *const u8).read() } {
            0 => return Some(String::from_utf8_lossy(&bytes).into_owned()),
            b => bytes.push(b),
        }
    }
    Some(format!("{}...", String::from_utf8_lossy(&bytes)))
}

/// A mapped image: the range the RVAs of a throw must stay in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Image {
    base: usize,
    size: usize,
}

impl Image {
    /// The image mapped at exactly `base`, by the size in its own header.
    fn loaded_at(base: usize) -> Option<Image> {
        /// `IMAGE_OPTIONAL_HEADER64::SizeOfImage`.
        const OPT_SIZE_OF_IMAGE: usize = 56;
        use victory_pe::layout::{DOS_E_LFANEW, FILE_HEADER_SIZE, NT_SIGNATURE_SIZE};
        let mut module: HMODULE = std::ptr::null_mut();
        // SAFETY: looks up the module containing `base`; no reference taken.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                base as *const u16,
                &mut module,
            )
        };
        if found == 0 || module as usize != base {
            return None;
        }
        let nt = peek::<u32>(base.checked_add(DOS_E_LFANEW)?)? as usize;
        let optional = base
            .checked_add(nt)?
            .checked_add(NT_SIGNATURE_SIZE + FILE_HEADER_SIZE)?;
        let size = peek::<u32>(optional.checked_add(OPT_SIZE_OF_IMAGE)?)? as usize;
        Some(Image { base, size })
    }

    /// The address of the `len` bytes at `rva`, if they are in the image.
    fn at(self, rva: usize, len: usize) -> Option<usize> {
        let end = rva.checked_add(len)?;
        if end > self.size {
            return None;
        }
        self.base.checked_add(rva)
    }

    /// The RVA stored at `rva`.
    fn rva_at(self, rva: usize) -> Option<usize> {
        peek::<u32>(self.at(rva, 4)?).map(|v| v as usize)
    }

    /// The text at `rva`, no further than the image's end.
    fn text(self, rva: usize) -> Option<String> {
        let at = self.at(rva, 1)?;
        text_at(at, MAX_TEXT.min(self.size - rva))
    }
}

/// What a C++ throw threw: the decorated name of its type, then its value
/// where that names the cause (an integer, e.g. an HRESULT; a standard
/// exception's message). `p` are the exception's parameters (`p[0]` the
/// magic). `None` if the throw's tables cannot be read: `p[3]` is not a
/// loaded image, an RVA leads outside it, or there is no type name.
fn thrown(p: &[usize; 15]) -> Option<String> {
    thrown_in(p, Image::loaded_at(p[3])?)
}

/// [`thrown`], the RVAs being relative to `image`. Everything here is the
/// title's and may be anything (a title can raise this exception code by
/// hand): no pointer is followed that was not checked, and no count is
/// believed.
fn thrown_in(p: &[usize; 15], image: Image) -> Option<String> {
    // The ThrowInfo at `p[2]` has the RVA of its catchable type array at
    // +12 (a count, then type RVAs); each type has its descriptor's RVA at
    // +4, and the descriptor its decorated name at +16. The object thrown
    // is at `p[1]`, anywhere (usually on the thrower's stack).
    let throw_info = p[2].checked_sub(image.base)?;
    let types = image.rva_at(throw_info.checked_add(12)?)?;
    let count = image.rva_at(types)?.min(MAX_CATCHABLE_TYPES);
    let name = |i: usize| {
        // `i` is below the cap and the others are 32-bit: no overflow.
        let descriptor = image.rva_at(image.rva_at(types + 4 + 4 * i)? + 4)?;
        image.text(descriptor + 16)
    };
    if count == 0 {
        return None;
    }
    // An empty name is what tables that are not a throw's read as.
    let first = name(0).filter(|name| !name.is_empty())?;
    let value = match &*first {
        // int, unsigned, long (HRESULT), unsigned long
        ".H" | ".I" | ".J" | ".K" => peek::<u32>(p[1]).map(|v| format!(" = {v:#010x}")),
        // std::exception keeps its message after the table pointer.
        _ if (0..count)
            .map_while(name)
            .any(|n| n == ".?AVexception@std@@") =>
        {
            p[1].checked_add(size_of::<usize>())
                .and_then(peek::<usize>)
                .filter(|&message| message != 0)
                .and_then(|message| text_at(message, MAX_TEXT))
                .map(|message| format!(": {message}"))
        }
        _ => None,
    };
    Some(format!("{first}{}", value.unwrap_or_default()))
}

/// # Safety
/// See [`report_unhandled`].
unsafe fn report(what: &str, info: *const EXCEPTION_POINTERS, to_file: bool) {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
    let code = record.ExceptionCode;
    let at = record.ExceptionAddress as usize;
    let p = record.ExceptionInformation;
    let detail = if code == EXCEPTION_ACCESS_VIOLATION {
        let kind = match p[0] {
            0 => "read",
            1 => "write",
            _ => "execute",
        };
        format!(" ({kind} {:#x}: {})", p[1], memory_at(p[1]))
    } else if code == CPP_EXCEPTION && record.NumberParameters >= 4 && p[0] == CPP_MAGIC {
        let what = thrown(&p).unwrap_or_else(|| "exception of an unreadable type".into());
        format!(" (C++ {what} thrown)")
    } else if code == EXCEPTION_ILLEGAL_INSTRUCTION || code == EXCEPTION_PRIV_INSTRUCTION {
        // SAFETY: the CPU refused the instruction at `at`, so it fetched it.
        format!(" (code {})", unsafe { instruction_bytes(at) })
    } else {
        String::new()
    };
    // Printed line by line (a fault while unwinding must not lose the
    // lines before it), and kept for the file.
    let mut text = String::new();
    let mut line = |l: String| {
        eprintln!("{l}");
        text.push_str(&l);
        text.push('\n');
    };
    line(format!(
        "VIctory: {what} {:#010x} at {}{detail}",
        code as u32,
        locate(at)
    ));
    line(format!(
        "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rsp {:#x} rbp {:#x}",
        context.Rax,
        context.Rbx,
        context.Rcx,
        context.Rdx,
        context.Rsi,
        context.Rdi,
        context.Rsp,
        context.Rbp
    ));
    line(format!(
        "  r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
        context.R8,
        context.R9,
        context.R10,
        context.R11,
        context.R12,
        context.R13,
        context.R14,
        context.R15
    ));
    for (i, pc) in unwind(*context).into_iter().enumerate() {
        line(format!("  #{i:<2} {}", locate(pc as usize)));
    }
    if to_file {
        let said = title_output();
        eprint!("{said}");
        text.push_str(&said);
        append_to_log(&text);
    }
    #[cfg(feature = "diagnostics")]
    for (address, len, file) in SAVE.get().into_iter().flatten() {
        // SAFETY: the caller of `save_on_crash` named readable memory.
        let bytes = unsafe { std::slice::from_raw_parts(*address as *const u8, *len) };
        let saved = std::fs::write(file, bytes);
        eprintln!(
            "  saved {address:#x}+{len:#x} to {} ({saved:?})",
            file.display()
        );
    }
}

/// Return addresses of the faulting thread's frames, from the faulting
/// instruction up, by the unwind data of each image (the Windows loader
/// registered it for title images too). Code without unwind data (our
/// thunks, leaf functions) is taken to have its return address at `rsp`.
fn unwind(context: CONTEXT) -> Vec<u64> {
    let (mut low, mut high) = (0, 0);
    // SAFETY: writes the current thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    let mut frames = Vec::with_capacity(MAX_FRAMES);
    unwind_into(context, low, high, &mut frames);
    frames
}

/// A `CONTEXT` at the 16-byte alignment the unwind and capture functions need.
#[repr(C, align(16))]
pub(crate) struct Aligned16(pub(crate) CONTEXT);

/// [`unwind`] for any thread of the process, its stack being `low..high`;
/// pushes no more than `MAX_FRAMES`, so it does not allocate when `frames`
/// has that capacity (another thread may be suspended holding the heap).
pub(crate) fn unwind_into(context: CONTEXT, low: usize, high: usize, frames: &mut Vec<u64>) {
    // `RtlVirtualUnwind` writes the context with 16-byte aligned stores, and
    // windows-sys declares `CONTEXT` without that alignment: a by-value copy on
    // the stack can land 8 bytes off and fault (seen in a debug build).
    let mut aligned = Aligned16(context);
    let context = &mut aligned.0;
    frames.push(context.Rip);
    while frames.len() < MAX_FRAMES {
        // Stop where reading the stack would fault inside this handler.
        let rsp = context.Rsp as usize;
        if rsp < low || rsp + 8 > high {
            break;
        }
        let mut image = 0;
        // SAFETY: looks up the function table entry covering `Rip`, if any.
        let entry = unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, null_mut()) };
        if entry.is_null() {
            // SAFETY: `rsp` is inside this thread's stack.
            context.Rip = unsafe { (rsp as *const u64).read() };
            context.Rsp += 8;
        } else {
            let (mut data, mut frame) = (null_mut(), 0);
            // SAFETY: `entry` belongs to `image`; the context is our copy.
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    entry,
                    context,
                    &mut data,
                    &mut frame,
                    null_mut(),
                )
            };
        }
        if context.Rip == 0 {
            break;
        }
        frames.push(context.Rip);
    }
}

/// `module+rva`, or the bare address.
pub fn locate(address: usize) -> String {
    module_of(address).unwrap_or_else(|| format!("{address:#x}"))
}

fn module_of(address: usize) -> Option<String> {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: looks up the module containing `address`; no reference taken.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 {
        return None;
    }
    let mut path = [0u16; 260];
    // SAFETY: `module` is a loaded module; the buffer length is passed.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) };
    let path = String::from_utf16_lossy(&path[..len as usize]);
    let name = path.rsplit('\\').next().unwrap_or(&path);
    Some(format!("{name}+{:#x}", address - module as usize))
}

/// What is at `address`, for an access violation there: nothing, memory
/// reserved but never committed, a guard page, or the protection the access
/// broke (with the module, for an image's pages). Each points at a different
/// cause: a wild pointer, an allocation whose commit failed, a stack
/// overrun, a write to code or constants.
fn memory_at(address: usize) -> String {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: reads the address space's bookkeeping, not `address` itself.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 {
        // Above the process's address space: Windows reports a
        // non-canonical address as all ones.
        return "outside the address space".into();
    }
    memory_named(&info, || module_of(address))
}

/// [`memory_at`] for what `VirtualQuery` said of an address; `module` names
/// the module the address is in, and is asked only for an image's pages.
/// Nothing here asks the system, so every answer can be had from a record
/// written by hand: memory that was released cannot be asked for by its
/// address without another thread taking the range first.
fn memory_named(
    info: &windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION,
    module: impl FnOnce() -> Option<String>,
) -> String {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_IMAGE, MEM_RESERVE, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        PAGE_WRITECOPY,
    };
    let protect = match info.State {
        MEM_FREE => return "nothing mapped there".into(),
        MEM_RESERVE => return "reserved, never committed".into(),
        _ if info.Protect & PAGE_GUARD != 0 => return "a guard page".into(),
        _ => match info.Protect & 0xff {
            PAGE_NOACCESS => "no access".into(),
            PAGE_READONLY => "read-only".into(),
            PAGE_READWRITE => "read-write".into(),
            PAGE_WRITECOPY => "copy-on-write".into(),
            PAGE_EXECUTE => "execute-only".into(),
            PAGE_EXECUTE_READ => "read-execute".into(),
            PAGE_EXECUTE_READWRITE => "read-write-execute".into(),
            PAGE_EXECUTE_WRITECOPY => "copy-on-write, executable".into(),
            _ => format!("protection {:#x}", info.Protect),
        },
    };
    if info.Type != MEM_IMAGE {
        return protect;
    }
    match module() {
        Some(module) => format!("{protect}, in {module}"),
        None => protect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_RECORD, RtlCaptureContext, RtlCaptureStackBackTrace,
    };

    #[test]
    #[inline(never)]
    fn unwinds_like_the_system_walk() {
        // SAFETY: zeroed CONTEXT is valid; both calls write our buffers.
        let (frames, walked) = unsafe {
            // `RtlCaptureContext` wants it 16-byte aligned too.
            let mut context = Aligned16(std::mem::zeroed());
            RtlCaptureContext(&mut context.0);
            let mut walked = [null_mut(); 8];
            let n = RtlCaptureStackBackTrace(0, 8, walked.as_mut_ptr(), null_mut());
            (unwind(context.0), walked[..usize::from(n)].to_vec())
        };
        // Frame 0 differs (two call sites in this function); callers agree.
        assert!(walked.len() >= 4, "{walked:?}");
        let ours: Vec<_> = frames[1..walked.len()]
            .iter()
            .map(|&f| f as usize)
            .collect();
        let system: Vec<_> = walked[1..].iter().map(|&f| f as usize).collect();
        assert_eq!(ours, system);
    }

    #[test]
    fn memory_at_names_what_an_access_hit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY,
            PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: reserves four pages of ours and commits three of them;
        // nothing reads or writes them.
        let base = unsafe {
            let base = VirtualAlloc(std::ptr::null(), 0x4000, MEM_RESERVE, PAGE_NOACCESS);
            assert!(!base.is_null());
            let page = |i: usize, protect| {
                let at = base.cast::<u8>().add(i * 0x1000).cast();
                assert!(!VirtualAlloc(at, 0x1000, MEM_COMMIT, protect).is_null());
            };
            page(1, PAGE_READONLY);
            page(2, PAGE_READWRITE | PAGE_GUARD);
            page(3, PAGE_NOACCESS);
            base as usize
        };
        assert_eq!(memory_at(base + 8), "reserved, never committed");
        assert_eq!(memory_at(base + 0x1008), "read-only");
        assert_eq!(memory_at(base + 0x2008), "a guard page");
        assert_eq!(memory_at(base + 0x3008), "no access");
        // SAFETY: releases the reservation made above.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
        // Free memory is asked for where nothing can ever be mapped, the
        // first 64 KiB, at both of its ends. The range released above is
        // not asked for: the other tests of this program run on their own
        // threads, and an allocation of theirs may take it at any moment.
        // What a released range is named as is tested from its record, in
        // `released_memory_is_named_from_its_record`.
        assert_eq!(memory_at(0), "nothing mapped there");
        assert_eq!(memory_at(0xfff8), "nothing mapped there");
        assert_eq!(memory_at(usize::MAX), "outside the address space");
        // This test's own code: an image page, named by its module.
        let code = memory_at(memory_at_names_what_an_access_hit as *const () as usize);
        let named = code.starts_with("read-execute, in victory_loader-");
        assert!(named, "{code}");
    }

    /// A record as `VirtualQuery` fills it in.
    fn queried(
        state: u32,
        protect: u32,
        kind: u32,
    ) -> windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION {
        // SAFETY: a zeroed MEMORY_BASIC_INFORMATION is valid.
        let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
            unsafe { std::mem::zeroed() };
        info.BaseAddress = 0x7ff6_1234_0000_usize as *mut _;
        info.RegionSize = 0x4000;
        info.State = state;
        info.Protect = protect;
        info.Type = kind;
        info
    }

    /// What `memory_at_names_what_an_access_hit` cannot ask the system for:
    /// memory that was released. `VirtualQuery` answers for it with a
    /// `MEM_FREE` record (no allocation base, `PAGE_NOACCESS`, no type),
    /// which is written here by hand; so are the other states, to pin the
    /// order they are told apart in.
    #[test]
    fn released_memory_is_named_from_its_record() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, PAGE_EXECUTE,
            PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD,
            PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
        };
        let never = || -> Option<String> { panic!("no module is asked for") };
        let module = || Some("title.exe+0x1234".to_owned());

        let released = queried(MEM_FREE, PAGE_NOACCESS, 0);
        assert!(released.AllocationBase.is_null());
        assert_eq!(memory_named(&released, never), "nothing mapped there");
        // The state decides, whatever the other fields were left as.
        let stale = queried(MEM_FREE, PAGE_READWRITE | PAGE_GUARD, MEM_IMAGE);
        assert_eq!(memory_named(&stale, never), "nothing mapped there");

        let reserved = queried(MEM_RESERVE, 0, MEM_PRIVATE);
        assert_eq!(memory_named(&reserved, never), "reserved, never committed");
        let reserved = queried(MEM_RESERVE, PAGE_GUARD, MEM_IMAGE);
        assert_eq!(memory_named(&reserved, never), "reserved, never committed");
        let guard = queried(MEM_COMMIT, PAGE_READWRITE | PAGE_GUARD, MEM_IMAGE);
        assert_eq!(memory_named(&guard, never), "a guard page");

        for (protect, name) in [
            (PAGE_NOACCESS, "no access"),
            (PAGE_READONLY, "read-only"),
            (PAGE_READWRITE, "read-write"),
            (PAGE_WRITECOPY, "copy-on-write"),
            (PAGE_EXECUTE, "execute-only"),
            (PAGE_EXECUTE_READ, "read-execute"),
            (PAGE_EXECUTE_READWRITE, "read-write-execute"),
            (PAGE_EXECUTE_WRITECOPY, "copy-on-write, executable"),
            (0, "protection 0x0"),
        ] {
            // Memory of the process's own and a mapped file have no module.
            let private = queried(MEM_COMMIT, protect, MEM_PRIVATE);
            assert_eq!(memory_named(&private, never), name);
            let mapped = queried(MEM_COMMIT, protect, MEM_MAPPED);
            assert_eq!(memory_named(&mapped, never), name);
            let image = queried(MEM_COMMIT, protect, MEM_IMAGE);
            assert_eq!(
                memory_named(&image, module),
                format!("{name}, in title.exe+0x1234")
            );
            // An image page whose module is gone by now.
            assert_eq!(memory_named(&image, || None), name);
        }
        // Modifier bits above the low byte do not change the name.
        const PAGE_NOCACHE: u32 = 0x200;
        let nocache = queried(MEM_COMMIT, PAGE_READWRITE | PAGE_NOCACHE, MEM_PRIVATE);
        assert_eq!(memory_named(&nocache, never), "read-write");
    }

    /// On an Intel CPU, the real instructions go through the vectored
    /// handler. On an AMD CPU they run natively, and the tests below test
    /// the emulation.
    #[test]
    fn clwb_loops() {
        // clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top
        let a = clwb_loop(&[
            0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
        ])
        .expect("rcx loop");
        assert_eq!((a.base, a.limit, a.len), (1, 0, 13));
        // clwb [r9]; add r9, 0x40; cmp r9, rax; jbe top
        let b = clwb_loop(&[
            0x66, 0x41, 0x0f, 0xae, 0x31, 0x49, 0x83, 0xc1, 0x40, 0x49, 0x39, 0xc1, 0x76, 0xf2,
        ])
        .expect("r9 loop");
        assert_eq!((b.base, b.limit, b.len), (9, 0, 14));
        // A different stride, a jump elsewhere, or a cut-off loop is not one.
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x20, 0x48, 0x39, 0xc1, 0x76, 0xf3
            ])
            .is_none()
        );
        assert!(
            clwb_loop(&[
                0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf0
            ])
            .is_none()
        );
        assert!(clwb_loop(&[0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40]).is_none());
        // The flags the last cmp leaves: base just past limit.
        assert_eq!(cmp_flags(0x1040, 0x1000) & 0x41, 0); // CF and ZF clear: the loop ends
        assert_eq!(cmp_flags(0x1000, 0x1000) & 0x40, 0x40);
    }

    /// clwb [rcx]; add rcx, 0x40; cmp rcx, rax; jbe top
    const CLWB_LOOP: [u8; 13] = [
        0x66, 0x0f, 0xae, 0x31, 0x48, 0x83, 0xc1, 0x40, 0x48, 0x39, 0xc1, 0x76, 0xf3,
    ];

    #[test]
    fn clwb_loop_advances_base_and_rip() {
        let (skipped, after) = emulate_with(&CLWB_LOOP, |c| {
            c.Rcx = 0x1000;
            c.Rax = 0x10ff;
            c.EFlags = 0x202 | CMP_FLAGS;
        });
        // Four lines: 0x1000, 0x1040, 0x1080 and 0x10c0; the loop leaves at
        // the first base above the limit, with the flags of that compare.
        assert_eq!(skipped, Some(13));
        assert_eq!((after.Rcx, after.Rax), (0x1100, 0x10ff));
        assert_eq!(after.EFlags, 0x202 | cmp_flags(0x1100, 0x10ff));
        assert_eq!(after.EFlags & 0x41, 0, "above: CF and ZF clear");
        // A base already above the limit: the one pass every loop makes.
        let (skipped, after) = emulate_with(&CLWB_LOOP, |c| {
            c.Rcx = 0x2000;
            c.Rax = 0x1000;
        });
        assert_eq!((skipped, after.Rcx), (Some(13), 0x2040));
        // Base and limit equal: one pass too.
        let (skipped, after) = emulate_with(&CLWB_LOOP, |c| {
            c.Rcx = 0x1000;
            c.Rax = 0x1000;
        });
        assert_eq!((skipped, after.Rcx), (Some(13), 0x1040));
    }

    /// Where the loop's last base is past the end of the address space, the
    /// real loop never leaves by its compare: nothing is emulated, and the
    /// registers stay as they were.
    #[test]
    fn clwb_loop_with_extreme_limit_does_not_overflow() {
        for (base, limit) in [
            (0, u64::MAX),
            (0x1000, u64::MAX - 63),
            (u64::MAX - 10, 5),
            (u64::MAX - 63, u64::MAX),
        ] {
            let (skipped, after) = emulate_with(&CLWB_LOOP, |c| {
                c.Rcx = base;
                c.Rax = limit;
                c.EFlags = 0x202;
            });
            assert_eq!(skipped, None, "base {base:#x}, limit {limit:#x}");
            assert_eq!((after.Rcx, after.Rax, after.EFlags), (base, limit, 0x202));
        }
        // Two passes that end in the last line of the address space.
        let (skipped, after) = emulate_with(&CLWB_LOOP, |c| {
            c.Rcx = u64::MAX - 0xff;
            c.Rax = u64::MAX - 0x80;
        });
        assert_eq!((skipped, after.Rcx), (Some(13), u64::MAX - 0x7f));
    }

    /// Two pages of ours, the second unreadable, so that a read past the
    /// end of the first faults.
    struct PageBeforeAGap(*mut u8);

    impl PageBeforeAGap {
        fn new() -> Self {
            use windows_sys::Win32::System::Memory::{
                MEM_COMMIT, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
                VirtualProtect,
            };
            // SAFETY: two fresh pages; the protection of the second is changed.
            unsafe {
                let page = VirtualAlloc(
                    std::ptr::null(),
                    0x2000,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_READWRITE,
                )
                .cast::<u8>();
                assert!(!page.is_null());
                let mut old = 0;
                let gap = page.add(0x1000).cast();
                assert_ne!(VirtualProtect(gap, 0x1000, PAGE_NOACCESS, &mut old), 0);
                PageBeforeAGap(page)
            }
        }

        /// Makes the second page readable and writable too.
        fn close_the_gap(&self) {
            use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
            let mut old = 0;
            // SAFETY: the second of our two pages.
            let done = unsafe {
                VirtualProtect(self.0.add(0x1000).cast(), 0x1000, PAGE_READWRITE, &mut old)
            };
            assert_ne!(done, 0);
        }

        /// Puts `code` where its last byte is `past_the_end` bytes into the
        /// second page (0: the first page's last byte); returns its address.
        fn end_with(&self, code: &[u8], past_the_end: usize) -> *const u8 {
            // SAFETY: inside our two pages; the caller made the second
            // writable if any byte goes there.
            unsafe {
                self.0.write_bytes(0xcc, 0x1000);
                let at = self.0.add(0x1000 + past_the_end - code.len());
                at.copy_from_nonoverlapping(code.as_ptr(), code.len());
                at
            }
        }
    }

    impl Drop for PageBeforeAGap {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            // SAFETY: releases the pages `new` allocated.
            unsafe { VirtualFree(self.0.cast(), 0, MEM_RELEASE) };
        }
    }

    const ILLEGAL: i32 = EXCEPTION_ILLEGAL_INSTRUCTION;

    /// insertq xmm0, xmm1, length 16, index 16
    const INSERTQ: [u8; 6] = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];

    fn insertq_registers(context: &mut CONTEXT) {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0);
        xmm[1] = reg(0xabcd, 0);
        context.Anonymous.FltSave.XmmRegisters = xmm;
    }

    fn xmm0(context: &CONTEXT) -> u64 {
        // SAFETY: the saved floating-point state is plain data.
        unsafe { context.Anonymous.FltSave.XmmRegisters[0].Low }
    }

    #[test]
    fn truncated_sse4a_at_a_page_end_is_declined() {
        let pages = PageBeforeAGap::new();
        // The instruction's first four bytes, its immediates cut off by
        // the unreadable page: declined, and nothing read from that page.
        let cut = pages.end_with(&INSERTQ[..4], 0);
        let (skipped, after) = emulate_at(ILLEGAL, cut, insertq_registers);
        assert_eq!((skipped, xmm0(&after)), (None, 0x1111_2222_3333_4444));
        for len in 1..4 {
            let cut = pages.end_with(&INSERTQ[..len], 0);
            assert_eq!(emulate_at(ILLEGAL, cut, insertq_registers).0, None);
        }
        // Whole, and ending with the page: emulated.
        let whole = pages.end_with(&INSERTQ, 0);
        let (skipped, after) = emulate_at(ILLEGAL, whole, insertq_registers);
        assert_eq!((skipped, xmm0(&after)), (Some(6), 0x1111_2222_abcd_4444));
    }

    #[test]
    fn a_short_instruction_at_a_page_end_is_declined() {
        let pages = PageBeforeAGap::new();
        // ud2, then the first one and two bytes of clzero.
        for code in [&[0x0f, 0x0b][..], &[0x0f], &[0x0f, 0x01]] {
            let at = pages.end_with(code, 0);
            assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, None, "{code:02x?}");
        }
        // monitorx as the page's last instruction.
        let at = pages.end_with(&[0x0f, 0x01, 0xfa], 0);
        assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, Some(3));
    }

    #[test]
    fn single_clwb_is_skipped() {
        // clwb [rax]; clwb [r9]; clwb [rax + 0x40]
        for code in [
            &[0x66, 0x0f, 0xae, 0x30][..],
            &[0x66, 0x41, 0x0f, 0xae, 0x31],
            &[0x66, 0x0f, 0xae, 0x70, 0x40],
        ] {
            let (skipped, after) = emulate_with(code, |c| c.Rax = 0x1234);
            assert_eq!(skipped, Some(code.len() as u64), "{code:02x?}");
            assert_eq!(after.Rax, 0x1234);
        }
    }

    #[test]
    fn clwb_loop_stops_at_the_page_end() {
        let pages = PageBeforeAGap::new();
        // The loop's clwb and add; its compare and jump are on the page
        // that cannot be read. Only the clwb is skipped: the loop is not
        // taken as one, and its registers are left alone.
        let at = pages.end_with(&CLWB_LOOP[..8], 0);
        let (skipped, after) = emulate_at(ILLEGAL, at, |c| {
            c.Rcx = 0x1000;
            c.Rax = 0x10ff;
        });
        assert_eq!((skipped, after.Rcx), (Some(4), 0x1000));
        // A clwb that ends with the page, and one cut off by it.
        let at = pages.end_with(&[0x66, 0x0f, 0xae, 0x70, 0x40], 0);
        assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, Some(5));
        let at = pages.end_with(&[0x66, 0x0f, 0xae, 0x70], 0);
        assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, None);
        let at = pages.end_with(&[0x66, 0x0f, 0xae], 0);
        assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, None);
    }

    #[test]
    fn an_instruction_across_two_readable_pages_is_emulated() {
        let pages = PageBeforeAGap::new();
        pages.close_the_gap();
        for past_the_end in 1..INSERTQ.len() {
            let at = pages.end_with(&INSERTQ, past_the_end);
            let (skipped, after) = emulate_at(ILLEGAL, at, insertq_registers);
            assert_eq!(skipped, Some(6), "{past_the_end} bytes on the next page");
            assert_eq!(xmm0(&after), 0x1111_2222_abcd_4444);
        }
        for past_the_end in 1..CLWB_LOOP.len() {
            let at = pages.end_with(&CLWB_LOOP, past_the_end);
            let (skipped, after) = emulate_at(ILLEGAL, at, |c| {
                c.Rcx = 0x1000;
                c.Rax = 0x10ff;
            });
            assert_eq!((skipped, after.Rcx), (Some(13), 0x1100), "{past_the_end}");
        }
        // mwaitx with one and two bytes on the next page.
        for past_the_end in 1..3 {
            let at = pages.end_with(&[0x0f, 0x01, 0xfb], past_the_end);
            assert_eq!(emulate_at(ILLEGAL, at, |_| {}).0, Some(3));
        }
    }

    #[test]
    fn clzero_of_memory_that_cannot_be_written_is_declined() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
        let clzero = [0x0f, 0x01, 0xfc];
        // Nothing mapped, and an address outside the address space.
        for rax in [0, 0x48, u64::MAX, 0x8000_0000_0000_0040] {
            let (skipped, after) = emulate_with(&clzero, |c| c.Rax = rax);
            assert_eq!((skipped, after.Rax), (None, rax), "rax {rax:#x}");
        }
        // The unreadable page, then the same page read-only.
        let pages = PageBeforeAGap::new();
        // SAFETY: inside our second page.
        let line = unsafe { pages.0.add(0x1040) };
        assert_eq!(emulate_with(&clzero, |c| c.Rax = line as u64).0, None);
        pages.close_the_gap();
        // SAFETY: our page, writable now; then made read-only.
        unsafe {
            line.write_bytes(0xff, 64);
            let mut old = 0;
            let second = pages.0.add(0x1000).cast();
            assert_ne!(VirtualProtect(second, 0x1000, PAGE_READONLY, &mut old), 0);
        }
        assert_eq!(emulate_with(&clzero, |c| c.Rax = line as u64).0, None);
        // SAFETY: the page is readable.
        let kept = unsafe { std::slice::from_raw_parts(line, 64) };
        assert!(kept.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn clwb_lengths() {
        let pad = |b: &[u8]| {
            let mut c = [0u8; 15];
            c[..b.len()].copy_from_slice(b);
            clwb_len(&c)
        };
        assert_eq!(pad(&[0x66, 0x41, 0x0f, 0xae, 0x31]), Some(5)); // clwb [r9]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0x30]), Some(4)); // clwb [rax]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0x70, 0x40]), Some(5)); // clwb [rax+0x40]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0, 0]), Some(8)); // [rax+disp32]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0x34, 0x24]), Some(5)); // clwb [rsp]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0x35, 0, 0, 0, 0]), Some(8)); // [rip+disp32]
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0x38]), None); // clflushopt, /7
        assert_eq!(pad(&[0x0f, 0xae, 0x30]), None); // xsaveopt, no 66
        assert_eq!(pad(&[0x66, 0x0f, 0xae, 0xf0]), None); // register form
        // Cut off before its end: not a clwb that can be skipped.
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x70]), None); // no disp8
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x34]), None); // no SIB
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0xb0, 0, 1, 0]), None); // 3 of disp32
        assert_eq!(clwb_len(&[0x66, 0x41, 0x0f, 0xae]), None);
        assert_eq!(clwb_len(&[0x66]), None);
        assert_eq!(clwb_len(&[]), None);
        assert_eq!(clwb_len(&[0x66, 0x0f, 0xae, 0x30]), Some(4)); // nothing after it
    }

    #[test]
    fn clzero_zeroes_its_line_on_any_cpu() {
        report_crashes();
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5); // inside the first line
        // SAFETY: clzero writes only the 64-byte line holding `at`, ours.
        unsafe { std::arch::asm!(".byte 0x0f, 0x01, 0xfc", in("rax") at) };
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
        // insertq xmm0, xmm1, len 16, at 16; then extrq xmm0, len 8, at 20.
        let out: u64;
        // SAFETY: touches only the named registers.
        unsafe {
            std::arch::asm!(
                "movq xmm0, {a}", "movq xmm1, {b}",
                ".byte 0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10",
                "movq {a}, xmm0",
                ".byte 0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14",
                "movq {b}, xmm0",
                a = inout(reg) 0x1111_2222_3333_4444u64 => _, b = inout(reg) 0xabcdu64 => out,
                out("xmm0") _, out("xmm1") _)
        };
        assert_eq!(out, 0xbc); // 0x1111_2222_abcd_4444 >> 20, 8 bits
        // SAFETY: monitorx/mwaitx are wait hints that write no memory.
        unsafe {
            std::arch::asm!(".byte 0x0f, 0x01, 0xfa", ".byte 0x0f, 0x01, 0xfb",
                in("rax") at, in("ecx") 0, in("edx") 0)
        };
    }

    fn reg(low: u64, high: u64) -> M128A {
        M128A {
            Low: low,
            High: high as i64,
        }
    }

    /// Gives the bytes at `rip` to the emulation as if they faulted with
    /// `exception`, the saved registers being what `set` makes of zeroed
    /// ones. The emulation does not execute the bytes, so this works on any
    /// CPU. Returns the number of bytes that the emulation skipped, or
    /// `None` if it did not emulate the instruction, and the registers
    /// after.
    fn emulate_at(
        exception: i32,
        rip: *const u8,
        set: impl FnOnce(&mut CONTEXT),
    ) -> (Option<u64>, CONTEXT) {
        // SAFETY: a zeroed record and context are valid; the emulation
        // reads only the page `rip` is in and writes only `context` (and
        // memory at `rax` for clzero, which the caller owns).
        unsafe {
            let mut record: EXCEPTION_RECORD = std::mem::zeroed();
            record.ExceptionCode = exception;
            let mut context: CONTEXT = std::mem::zeroed();
            set(&mut context);
            context.Rip = rip as u64;
            let mut pointers = EXCEPTION_POINTERS {
                ExceptionRecord: &mut record,
                ContextRecord: &mut context,
            };
            let emulated = emulate_amd_only(&mut pointers);
            (emulated.then(|| context.Rip - rip as u64), context)
        }
    }

    /// [`emulate_at`] for an illegal instruction at the start of `code`,
    /// which is followed by `int3`s, so that a decoder that reads too far
    /// sees no valid bytes.
    fn emulate_with(code: &[u8], set: impl FnOnce(&mut CONTEXT)) -> (Option<u64>, CONTEXT) {
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        emulate_at(EXCEPTION_ILLEGAL_INSTRUCTION, bytes.as_ptr(), set)
    }

    /// [`emulate_at`] with `rax` and `xmm` as the saved registers; `xmm`
    /// gets the saved registers after.
    fn emulate_as(exception: i32, code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        let mut bytes = [0xcc_u8; 16];
        bytes[..code.len()].copy_from_slice(code);
        let (skipped, context) = emulate_at(exception, bytes.as_ptr(), |context| {
            context.Rax = rax;
            context.Anonymous.FltSave.XmmRegisters = *xmm;
        });
        // SAFETY: the saved floating-point state is plain data.
        *xmm = unsafe { context.Anonymous.FltSave.XmmRegisters };
        skipped
    }

    fn emulate(code: &[u8], rax: u64, xmm: &mut [M128A; 16]) -> Option<u64> {
        emulate_as(EXCEPTION_ILLEGAL_INSTRUCTION, code, rax, xmm)
    }

    fn low(xmm: &[M128A; 16], i: usize) -> u64 {
        xmm[i].Low
    }

    #[test]
    fn emulated_clzero_zeroes_its_line() {
        #[repr(align(64))]
        struct Line([u8; 128]);
        let mut line = Line([0xff; 128]);
        let at = line.0.as_mut_ptr().wrapping_add(5) as u64; // inside the first line
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfc], at, &mut xmm), Some(3));
        assert!(line.0[..64].iter().all(|&b| b == 0));
        assert!(line.0[64..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn emulated_monitorx_and_mwaitx_are_skipped() {
        let mut xmm = [reg(1, 2); 16];
        assert_eq!(emulate(&[0x0f, 0x01, 0xfa], 0, &mut xmm), Some(3));
        assert_eq!(emulate(&[0x0f, 0x01, 0xfb], 0, &mut xmm), Some(3));
        assert!(xmm.iter().all(|r| r.Low == 1 && r.High == 2));
    }

    #[test]
    fn emulated_insertq_and_extrq_with_immediates() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1111_2222_3333_4444, 0x5555);
        xmm[1] = reg(0xabcd, 0x6666);
        // insertq xmm0, xmm1, length 16, index 16
        let insertq = [0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 1), xmm[1].High), (0xabcd, 0x6666));
        // extrq xmm0, length 8, index 20
        let extrq = [0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(6));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_insertq_and_extrq_with_fields_in_a_register() {
        let mut xmm = [reg(0, 0); 16];
        // insertq xmm0, xmm1: length and index in xmm1's high half.
        xmm[0] = reg(0x1111_2222_3333_4444, 0);
        xmm[1] = reg(0xabcd, 0x10_10);
        assert_eq!(emulate(&[0xf2, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0x1111_2222_abcd_4444);
        // extrq xmm0, xmm1: length and index in xmm1's low half.
        xmm[1] = reg(0x14_08, 0);
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0xc1], 0, &mut xmm), Some(4));
        assert_eq!(low(&xmm, 0), 0xbc);
    }

    #[test]
    fn emulated_sse4a_reaches_xmm8_to_xmm15() {
        let mut xmm = [reg(0, 0); 16];
        xmm[8] = reg(0x1111_2222_3333_4444, 0);
        xmm[9] = reg(0xabcd, 0);
        // insertq xmm8, xmm9, length 16, index 16 (REX.R and REX.B)
        let insertq = [0xf2, 0x45, 0x0f, 0x78, 0xc1, 0x10, 0x10];
        assert_eq!(emulate(&insertq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 8), 0x1111_2222_abcd_4444);
        assert_eq!((low(&xmm, 0), low(&xmm, 1)), (0, 0));
        // extrq xmm9, length 8, index 4 (REX.B)
        let extrq = [0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04];
        assert_eq!(emulate(&extrq, 0, &mut xmm), Some(7));
        assert_eq!(low(&xmm, 9), 0xbc);
        assert_eq!(low(&xmm, 1), 0);
    }

    #[test]
    fn emulated_sse4a_length_0_is_64_bits() {
        let mut xmm = [reg(0, 0); 16];
        xmm[0] = reg(0x1122_3344_5566_7788, 0);
        // extrq xmm0, length 0 (64), index 0. A non-zero index with
        // length 64 goes past bit 63, and AMD leaves that result undefined.
        assert_eq!(
            emulate(&[0x66, 0x0f, 0x78, 0xc0, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0x1122_3344_5566_7788);
        // insertq xmm0, xmm1, length 0 (64), index 0
        xmm[1] = reg(0xdead_beef_cafe_f00d, 0);
        assert_eq!(
            emulate(&[0xf2, 0x0f, 0x78, 0xc1, 0x00, 0x00], 0, &mut xmm),
            Some(6)
        );
        assert_eq!(low(&xmm, 0), 0xdead_beef_cafe_f00d);
    }

    #[test]
    fn other_instructions_and_exceptions_are_not_emulated() {
        let mut xmm = [reg(0, 0); 16];
        assert_eq!(emulate(&[0x0f, 0x0b], 0, &mut xmm), None); // ud2
        // extrq with a memory operand, which does not exist
        assert_eq!(emulate(&[0x66, 0x0f, 0x79, 0x00], 0, &mut xmm), None);
        // extrq with immediates needs ModRM.reg 0
        let bad_extrq = [0x66, 0x0f, 0x78, 0xc8, 0x08, 0x14];
        assert_eq!(emulate(&bad_extrq, 0, &mut xmm), None);
        // a clzero that did not cause an illegal instruction
        let clzero = [0x0f, 0x01, 0xfc];
        assert_eq!(
            emulate_as(EXCEPTION_ACCESS_VIOLATION, &clzero, 0, &mut xmm),
            None
        );
    }

    /// Where [`throw_image`] puts things.
    const THROW_INFO: usize = 0x10;
    const TYPE_ARRAY: usize = 0x40;
    const TYPES: usize = 0x400;
    const DESCRIPTORS: usize = 0x1000;

    /// The tables of a C++ throw as MSVC lays them out, with one catchable
    /// type for each of `names` and `count` as the array's count: a
    /// ThrowInfo, the catchable type array, the types (0x20 apart) and
    /// their descriptors (0x40 apart). Written here, like a compiler would.
    fn throw_image(names: &[&str], count: u32) -> Vec<u8> {
        let mut image = vec![0u8; DESCRIPTORS + 0x40 * names.len() + 0x40];
        let mut put = |at: usize, value: usize| {
            image[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
        };
        put(THROW_INFO + 12, TYPE_ARRAY);
        put(TYPE_ARRAY, count as usize);
        for i in 0..names.len() {
            put(TYPE_ARRAY + 4 + 4 * i, TYPES + 0x20 * i);
            put(TYPES + 0x20 * i + 4, DESCRIPTORS + 0x40 * i);
        }
        for (i, name) in names.iter().enumerate() {
            let at = DESCRIPTORS + 0x40 * i + 16;
            image[at..at + name.len()].copy_from_slice(name.as_bytes());
        }
        image
    }

    /// What the report says of a throw of `object` with the tables `image`.
    fn thrown_from(image: &[u8], object: usize) -> Option<String> {
        let base = image.as_ptr() as usize;
        let mut p = [0usize; 15];
        p[..4].copy_from_slice(&[CPP_MAGIC, object, base + THROW_INFO, base]);
        let size = image.len();
        thrown_in(&p, Image { base, size })
    }

    const RUNTIME_ERROR: &str = ".?AVruntime_error@std@@";
    const STD_EXCEPTION: &str = ".?AVexception@std@@";

    /// A `std::exception` object: the table pointer, then the message.
    fn exception_object(message: &std::ffi::CStr) -> [usize; 2] {
        [0, message.as_ptr() as usize]
    }

    #[test]
    fn thrown_reads_an_int_and_a_std_exception() {
        let hresult = 0x8007_0057_u32;
        let int = throw_image(&[".H"], 1);
        let object = &raw const hresult as usize;
        assert_eq!(
            thrown_from(&int, object).as_deref(),
            Some(".H = 0x80070057")
        );
        let long = throw_image(&[".J"], 1);
        assert_eq!(
            thrown_from(&long, object).as_deref(),
            Some(".J = 0x80070057")
        );

        let object = exception_object(c"boom");
        let error = throw_image(&[RUNTIME_ERROR, STD_EXCEPTION], 2);
        let said = thrown_from(&error, object.as_ptr() as usize);
        assert_eq!(said.as_deref(), Some(".?AVruntime_error@std@@: boom"));
        // An exception without a message, and a class that is not one.
        let silent = [0usize; 2];
        let said = thrown_from(&error, silent.as_ptr() as usize);
        assert_eq!(said.as_deref(), Some(RUNTIME_ERROR));
        let other = throw_image(&[".?AVTitleError@@"], 1);
        let said = thrown_from(&other, object.as_ptr() as usize);
        assert_eq!(said.as_deref(), Some(".?AVTitleError@@"));
    }

    #[test]
    fn thrown_with_a_huge_type_count_is_bounded() {
        let object = exception_object(c"boom");
        let object = object.as_ptr() as usize;
        let names = |exception_at: usize| {
            let mut names = [".?AVTitleError@@"; 80];
            names[exception_at] = STD_EXCEPTION;
            names
        };
        // The last type that is looked at, and one past the cap that is
        // not, whatever the count says.
        let inside = throw_image(&names(MAX_CATCHABLE_TYPES - 1), u32::MAX);
        let said = thrown_from(&inside, object);
        assert_eq!(said.as_deref(), Some(".?AVTitleError@@: boom"));
        let outside = throw_image(&names(MAX_CATCHABLE_TYPES + 6), u32::MAX);
        assert_eq!(
            thrown_from(&outside, object).as_deref(),
            Some(".?AVTitleError@@")
        );
        // A count larger than the array: the search ends with the image.
        let short = throw_image(&[".?AVTitleError@@"], u32::MAX);
        assert_eq!(
            thrown_from(&short, object).as_deref(),
            Some(".?AVTitleError@@")
        );
        // A message with no end in sight is cut.
        let long = vec![b'a'; 2 * MAX_TEXT];
        let endless = [0, long.as_ptr() as usize];
        let error = throw_image(&[STD_EXCEPTION], 1);
        let said = thrown_from(&error, endless.as_ptr() as usize).unwrap();
        assert_eq!(said.len(), STD_EXCEPTION.len() + 2 + MAX_TEXT + 3);
        assert!(said.ends_with("aaa..."), "{said}");
    }

    #[test]
    fn thrown_with_rvas_outside_the_module_is_declined() {
        let object = exception_object(c"boom");
        let object = object.as_ptr() as usize;
        let good = throw_image(&[RUNTIME_ERROR, STD_EXCEPTION], 2);
        let put = |at: usize, value: u32| {
            let mut image = good.clone();
            image[at..at + 4].copy_from_slice(&value.to_le_bytes());
            image
        };
        let size = good.len() as u32;
        // The type array, the first type and its descriptor, each outside
        // the image or with its last byte outside.
        for (at, what) in [
            (THROW_INFO + 12, "type array"),
            (TYPE_ARRAY + 4, "type"),
            (TYPES + 4, "descriptor"),
        ] {
            for rva in [
                size,
                size - 3,
                size - 16,
                0x7fff_fff0,
                0xffff_fff0,
                u32::MAX,
            ] {
                let said = thrown_from(&put(at, rva), object);
                assert_eq!(said, None, "{what} at {rva:#x}");
            }
        }
        // No catchable types at all.
        assert_eq!(thrown_from(&put(TYPE_ARRAY, 0), object), None);
        // A second type that leads outside ends the search for the
        // standard exception: the name is reported without a message.
        let said = thrown_from(&put(TYPE_ARRAY + 8, 0xffff_fff0), object);
        assert_eq!(said.as_deref(), Some(RUNTIME_ERROR));
        // A ThrowInfo that is not in the image.
        let base = good.as_ptr() as usize;
        let image = Image {
            base,
            size: good.len(),
        };
        for throw_info in [
            0,
            base - 1,
            base + good.len() - 15,
            base + good.len(),
            usize::MAX,
        ] {
            let mut p = [0usize; 15];
            p[..4].copy_from_slice(&[CPP_MAGIC, object, throw_info, base]);
            assert_eq!(thrown_in(&p, image), None, "ThrowInfo at {throw_info:#x}");
        }
        // An object, or a message, where nothing can be read: the type
        // is still named.
        for object in [0, 8, usize::MAX - 2, usize::MAX] {
            let said = thrown_from(&good, object);
            assert_eq!(said.as_deref(), Some(RUNTIME_ERROR), "object {object:#x}");
            let int = throw_image(&[".H"], 1);
            assert_eq!(thrown_from(&int, object).as_deref(), Some(".H"));
        }
        for message in [8, usize::MAX] {
            let wild = [0, message];
            let said = thrown_from(&good, wild.as_ptr() as usize);
            assert_eq!(said.as_deref(), Some(RUNTIME_ERROR), "message {message:#x}");
        }
        // A base that is no loaded image, as the report gets it.
        let mut p = [0usize; 15];
        p[..4].copy_from_slice(&[CPP_MAGIC, object, base + THROW_INFO, base]);
        assert_eq!(thrown(&p), None);
        p[2..4].copy_from_slice(&[0, 0]);
        assert_eq!(thrown(&p), None);
    }

    #[test]
    fn a_loaded_image_is_found_by_its_base_only() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        // SAFETY: asks for this program's own module.
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        let image = Image::loaded_at(base).expect("the test program");
        let here = a_loaded_image_is_found_by_its_base_only as *const () as usize;
        assert!(image.base == base && here - base < image.size, "{image:x?}");
        assert_eq!(image.at(image.size - 4, 4), Some(base + image.size - 4));
        assert_eq!(image.at(image.size - 3, 4), None);
        assert_eq!(image.at(usize::MAX, 4), None);
        // An address inside it is not its base; nor is unmapped memory.
        assert_eq!(Image::loaded_at(here), None);
        assert_eq!(Image::loaded_at(base + 0x1000), None);
        assert_eq!(Image::loaded_at(0), None);
        assert_eq!(Image::loaded_at(usize::MAX), None);
    }

    #[test]
    fn instruction_bytes_stop_at_the_end_of_their_page() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };
        // rdpru, then the next instruction (mov rcx, rax) and padding.
        let mut code = [0x90_u8; 16];
        code[..6].copy_from_slice(&[0x0f, 0x01, 0xfd, 0x48, 0x89, 0xc1]);
        // SAFETY: two fresh pages of ours, the second made unreadable, so a
        // read past the end of the first faults.
        unsafe {
            let page = VirtualAlloc(
                std::ptr::null(),
                0x2000,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!page.is_null());
            let mut old = 0;
            let guard = page.add(0x1000).cast();
            assert_ne!(VirtualProtect(guard, 0x1000, PAGE_NOACCESS, &mut old), 0);
            page.copy_from_nonoverlapping(code.as_ptr(), code.len());
            let whole = "0f 01 fd 48 89 c1 90 90 90 90 90 90 90 90 90";
            assert_eq!(instruction_bytes(page as usize), whole);
            // The page's last three bytes: rdpru, and nothing read after it.
            let end = page.add(0x1000 - 3);
            end.copy_from_nonoverlapping(code.as_ptr(), 3);
            assert_eq!(instruction_bytes(end as usize), "0f 01 fd");
            VirtualFree(page.cast(), 0, MEM_RELEASE);
        }
    }
}
