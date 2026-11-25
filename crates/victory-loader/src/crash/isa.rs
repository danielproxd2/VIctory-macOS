//! Which instructions of the console's CPU (AMD Zen 2) the host runs.
//!
//! One instruction per extension a Zen 2 has and an older or another
//! maker's CPU, or a translator such as Rosetta 2, may lack. The test runs
//! each in a child process and prints what happened, as a table. What it
//! asserts is the same on every host: an instruction runs, or is emulated,
//! or ends the process with a report that says what it was. Which of the
//! three is the host's business and is only printed.
//!
//! Every encoding here is as the vendors' public manuals give it (AMD64
//! Architecture Programmer's Manual, volume 3; Intel SDM, volume 2).
//! Privileged instructions (`wbnoinvd`, `invlpgb`, the SVM set) are left
//! out: no title runs them.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::EXCEPTION_ILLEGAL_INSTRUCTION;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, FlushInstructionCache,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// What a register holds when the instruction runs.
#[derive(Clone, Copy)]
enum Arg {
    Value(u64),
    /// The address of 4 KiB of zeroed memory of ours, aligned to 64 bytes.
    Scratch,
}
use Arg::{Scratch, Value};

/// One instruction, as it is run.
struct Probe {
    name: &'static str,
    /// The CPUID feature it belongs to.
    extension: &'static str,
    /// Whether only AMD CPUs have it.
    amd_only: bool,
    code: &'static [u8],
    rax: Arg,
    rcx: Arg,
    rdx: Arg,
    /// What `eax` holds after it, where the registers above decide that.
    eax: Option<u32>,
}

const fn probe(
    name: &'static str,
    extension: &'static str,
    amd_only: bool,
    code: &'static [u8],
) -> Probe {
    Probe {
        name,
        extension,
        amd_only,
        code,
        rax: Value(0),
        rcx: Value(0),
        rdx: Value(0),
        eax: None,
    }
}

impl Probe {
    const fn rax(mut self, rax: Arg) -> Probe {
        self.rax = rax;
        self
    }
    const fn rcx(mut self, rcx: Arg) -> Probe {
        self.rcx = rcx;
        self
    }
    const fn rdx(mut self, rdx: Arg) -> Probe {
        self.rdx = rdx;
        self
    }
    const fn eax(mut self, eax: u32) -> Probe {
        self.eax = Some(eax);
        self
    }
}

/// The instructions, with the operands their bytes name in the comments.
/// Vector registers hold whatever they held: the arithmetic on them is not
/// looked at, and floating-point exceptions are masked.
const PROBES: &[Probe] = &[
    // andn eax, ecx, edx: eax = !ecx & edx
    probe("andn", "BMI1", false, &[0xc4, 0xe2, 0x70, 0xf2, 0xc2])
        .rcx(Value(0xf0))
        .rdx(Value(0xff))
        .eax(0x0f),
    // pdep eax, ecx, edx: the low bits of ecx go where edx has bits
    probe("pdep", "BMI2", false, &[0xc4, 0xe2, 0x73, 0xf5, 0xc2])
        .rcx(Value(0b11))
        .rdx(Value(0b1010))
        .eax(0b1010),
    // adcx eax, ecx
    probe("adcx", "ADX", false, &[0x66, 0x0f, 0x38, 0xf6, 0xc1]),
    // vcvtph2ps xmm0, xmm1
    probe("vcvtph2ps", "F16C", false, &[0xc4, 0xe2, 0x79, 0x13, 0xc1]),
    // vfmadd231ps xmm0, xmm1, xmm2
    probe("vfmadd231ps", "FMA", false, &[0xc4, 0xe2, 0x71, 0xb8, 0xc2]),
    // vpaddd ymm0, ymm1, ymm2; vzeroupper
    probe(
        "vpaddd ymm",
        "AVX2",
        false,
        &[0xc5, 0xf5, 0xfe, 0xc2, 0xc5, 0xf8, 0x77],
    ),
    // movbe ecx, [rax]
    probe("movbe", "MOVBE", false, &[0x0f, 0x38, 0xf0, 0x08]).rax(Scratch),
    // lzcnt eax, ecx: 31 leading zeros in 1. A CPU without it runs the
    // same bytes as bsr, which gives 0.
    probe("lzcnt", "ABM", false, &[0xf3, 0x0f, 0xbd, 0xc1])
        .rcx(Value(1))
        .eax(31),
    // sha256rnds2 xmm1, xmm2 (and xmm0)
    probe("sha256rnds2", "SHA", false, &[0x0f, 0x38, 0xcb, 0xca]),
    // rdrand eax
    probe("rdrand", "RDRAND", false, &[0x0f, 0xc7, 0xf0]),
    // rdseed eax
    probe("rdseed", "RDSEED", false, &[0x0f, 0xc7, 0xf8]),
    // rdpid rax
    probe("rdpid", "RDPID", false, &[0xf3, 0x0f, 0xc7, 0xf8]),
    // clflushopt [rax]
    probe("clflushopt", "CLFLUSHOPT", false, &[0x66, 0x0f, 0xae, 0x38]).rax(Scratch),
    // clwb [rax]
    probe("clwb", "CLWB", false, &[0x66, 0x0f, 0xae, 0x30]).rax(Scratch),
    // xsavec [rcx], the x87 and SSE state (edx:eax = 3)
    probe("xsavec", "XSAVEC", false, &[0x0f, 0xc7, 0x21])
        .rax(Value(3))
        .rcx(Scratch),
    // clzero: the line at rax
    probe("clzero", "CLZERO", true, &[0x0f, 0x01, 0xfc]).rax(Scratch),
    // extrq xmm0, 8, 20
    probe(
        "extrq",
        "SSE4A",
        true,
        &[0x66, 0x0f, 0x78, 0xc0, 0x08, 0x14],
    ),
    // insertq xmm0, xmm1, 16, 16
    probe(
        "insertq",
        "SSE4A",
        true,
        &[0xf2, 0x0f, 0x78, 0xc1, 0x10, 0x10],
    ),
    // movntss [rax], xmm0
    probe("movntss", "SSE4A", true, &[0xf3, 0x0f, 0x2b, 0x00]).rax(Scratch),
    // movntsd [rax], xmm0
    probe("movntsd", "SSE4A", true, &[0xf2, 0x0f, 0x2b, 0x00]).rax(Scratch),
    // monitorx: the line at rax, no extensions, no hints
    probe("monitorx", "MONITORX", true, &[0x0f, 0x01, 0xfa]).rax(Scratch),
    // mwaitx: no hints, no extensions (returns at the next interrupt)
    probe("mwaitx", "MONITORX", true, &[0x0f, 0x01, 0xfb]),
    // rdpru: ecx 0 is MPERF
    probe("rdpru", "RDPRU", true, &[0x0f, 0x01, 0xfd]),
];

/// The scratch memory of [`Arg::Scratch`].
#[repr(align(64))]
struct ScratchMemory([u8; 4096]);

/// Where the instruction being run is, how long it is, and the exception
/// it raised (0: none). For the child's own handler.
static AT: AtomicUsize = AtomicUsize::new(0);
static LEN: AtomicUsize = AtomicUsize::new(0);
static RAISED: AtomicU32 = AtomicU32::new(0);

/// Notes an exception raised by the instruction at [`AT`] and goes on
/// after it (at the `ret` that follows it).
unsafe extern "system" fn note_and_skip(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes valid exception pointers.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
    let at = AT.load(Ordering::SeqCst);
    if at == 0 || record.ExceptionAddress as usize != at {
        return 0; // EXCEPTION_CONTINUE_SEARCH
    }
    RAISED.store(record.ExceptionCode as u32, Ordering::SeqCst);
    context.Rip = (at + LEN.load(Ordering::SeqCst)) as u64;
    -1 // EXCEPTION_CONTINUE_EXECUTION
}

/// Runs the probe's instruction once and returns `eax` after it.
fn run(probe: &Probe) -> u32 {
    let scratch = Box::leak(Box::new(ScratchMemory([0; 4096])));
    let arg = |arg| match arg {
        Value(value) => value,
        Scratch => scratch.0.as_ptr() as u64,
    };
    let (rax, rcx, rdx) = (arg(probe.rax), arg(probe.rcx), arg(probe.rdx));
    // SAFETY: a fresh page of ours gets the instruction and a `ret`, and is
    // called with the registers the probe names. The instructions write
    // only those registers, xmm0 to xmm2 and the scratch memory, all of
    // which are declared or ours. The page is never freed: one per run of
    // a short-lived process.
    unsafe {
        let page = VirtualAlloc(
            std::ptr::null(),
            0x1000,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_EXECUTE_READWRITE,
        )
        .cast::<u8>();
        assert!(!page.is_null());
        page.copy_from_nonoverlapping(probe.code.as_ptr(), probe.code.len());
        page.add(probe.code.len()).write(0xc3);
        FlushInstructionCache(GetCurrentProcess(), page.cast(), 0x1000);
        LEN.store(probe.code.len(), Ordering::SeqCst);
        RAISED.store(0, Ordering::SeqCst);
        AT.store(page as usize, Ordering::SeqCst);
        let eax: u64;
        std::arch::asm!(
            "call {code}",
            code = in(reg) page,
            inout("rax") rax => eax,
            inout("rcx") rcx => _,
            inout("rdx") rdx => _,
            clobber_abi("system"),
        );
        AT.store(0, Ordering::SeqCst);
        eax as u32
    }
}

/// In a child process of [`zen2_instruction_inventory`].
///
/// `VICTORY_ISA=all`: runs every probe with a handler of its own that
/// notes the exception and goes on, and prints for each whether the CPU
/// ran it. VIctory's handlers are not installed, so nothing is emulated.
///
/// `VICTORY_ISA=<name>`: runs that probe the way a title's code runs, with
/// VIctory's handlers and the crash log at `VICTORY_ISA_LOG`. It returns
/// if the instruction ran or was emulated; else the process ends with the
/// report.
#[test]
fn isa_child() {
    let Ok(which) = std::env::var("VICTORY_ISA") else {
        return;
    };
    if which == "all" {
        // SAFETY: the handler only reads the exception it is given.
        unsafe { AddVectoredExceptionHandler(1, Some(note_and_skip)) };
        for probe in PROBES {
            let eax = run(probe);
            match RAISED.load(Ordering::SeqCst) {
                0 => println!("ISA {} ran {eax:#x}", probe.name),
                code => println!("ISA {} raised {code:#x}", probe.name),
            }
        }
        return;
    }
    let probe = PROBES.iter().find(|p| p.name == which).expect("a probe");
    let log = std::env::var_os("VICTORY_ISA_LOG").expect("VICTORY_ISA_LOG");
    crate::log_crashes_to(log.into());
    crate::report_crashes();
    let eax = run(probe);
    println!("ISA {} ran {eax:#x}", probe.name);
}

/// What became of a probe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The CPU ran it; `eax` after it.
    Runs(u32),
    /// The CPU refused it and VIctory did its job.
    Emulated,
    /// The CPU refused it and the crash report names it.
    Named,
    /// The CPU refused it and the crash report shows its bytes: an
    /// instruction every Zen 2 has that this host's CPU lacks.
    Missing,
}

fn child(envs: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "crash::isa::isa_child", "--nocapture"]);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

/// The word after `ISA <name> ` in a child's output, and the number after it.
fn said(output: &std::process::Output, name: &str) -> Option<(String, u32)> {
    let text = String::from_utf8_lossy(&output.stdout);
    // The test harness may print on the same line, ahead of ours.
    let mark = format!("ISA {name} ");
    let line = text
        .lines()
        .find_map(|l| l.find(&mark).map(|at| &l[at + mark.len()..]))?;
    let (what, number) = line.split_once(' ')?;
    let number = u32::from_str_radix(number.trim().trim_start_matches("0x"), 16).ok()?;
    Some((what.to_owned(), number))
}

fn hex(code: &[u8]) -> String {
    let hex: Vec<_> = code.iter().map(|b| format!("{b:02x}")).collect();
    hex.join(" ")
}

/// Runs every probe (see the module's text) and prints the table. The
/// assertions hold on any CPU:
///
/// * the CPU runs the instruction, or refuses it as an illegal one;
/// * a refused instruction that VIctory emulates comes back;
/// * a refused instruction only AMD has, and VIctory does not emulate,
///   ends the process with "unsupported AMD instruction <name>" in the
///   crash log;
/// * any other refused instruction ends it with its bytes in the log.
#[test]
fn zen2_instruction_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let all = child(&[("VICTORY_ISA", "all".as_ref())]);
    assert!(
        all.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&all.stdout),
        String::from_utf8_lossy(&all.stderr)
    );
    let mut rows = Vec::new();
    for probe in PROBES {
        let name = probe.name;
        let outcome = match said(&all, name) {
            Some((what, eax)) if what == "ran" => Outcome::Runs(eax),
            Some((what, code)) if what == "raised" => {
                assert_eq!(
                    code, EXCEPTION_ILLEGAL_INSTRUCTION as u32,
                    "{name} raised something other than an illegal instruction"
                );
                let log = dir.path().join(format!("{}.log", name.replace(' ', "-")));
                let alone = child(&[
                    ("VICTORY_ISA", name.as_ref()),
                    ("VICTORY_ISA_LOG", log.as_os_str()),
                ]);
                let report = std::fs::read_to_string(&log).unwrap_or_default();
                let named = format!("(unsupported AMD instruction {name}: code ");
                let bytes = format!("(code {}", hex(probe.code));
                if alone.status.success() && said(&alone, name).is_some() {
                    assert_eq!(report, "", "{name} came back, and was reported");
                    Outcome::Emulated
                } else if report.contains(&named) {
                    assert!(probe.amd_only, "{name} is not an AMD instruction: {report}");
                    assert!(report.contains(&hex(probe.code)), "{report}");
                    Outcome::Named
                } else {
                    assert!(
                        report.contains(&bytes),
                        "{name} ended the process without a report of it: {report:?}\n{}",
                        String::from_utf8_lossy(&alone.stderr)
                    );
                    assert!(
                        !probe.amd_only,
                        "{name} is AMD's, is not emulated and is not named: {report}"
                    );
                    Outcome::Missing
                }
            }
            other => panic!(
                "no line for {name} ({other:?}):\n{}",
                String::from_utf8_lossy(&all.stdout)
            ),
        };
        rows.push((probe, outcome));
    }

    println!("instruction   extension   AMD only  encoding              on this host");
    for (probe, outcome) in &rows {
        let host = match outcome {
            Outcome::Runs(eax) => match probe.eax {
                Some(expected) if expected != *eax => {
                    format!("runs as another instruction (eax {eax:#x}, not {expected:#x})")
                }
                _ => "runs".to_owned(),
            },
            Outcome::Emulated => "illegal instruction; emulated by VIctory".to_owned(),
            Outcome::Named => "illegal instruction; reported by name".to_owned(),
            Outcome::Missing => "illegal instruction; reported by its bytes".to_owned(),
        };
        println!(
            "{:<13} {:<11} {:<9} {:<21} {host}",
            probe.name,
            probe.extension,
            if probe.amd_only { "yes" } else { "no" },
            hex(probe.code)
        );
    }
    println!(
        "ROSETTA_ADVERTISE_AVX: {}",
        std::env::var("ROSETTA_ADVERTISE_AVX").unwrap_or_else(|_| "not set".into())
    );

    // tools/macos/victory.sh sets ROSETTA_ADVERTISE_AVX=1 so that a title
    // sees AVX, AVX2, FMA and BMI in CPUID and uses them. Where it is set,
    // they have to run: a title that believes CPUID would otherwise end.
    if std::env::var("ROSETTA_ADVERTISE_AVX").is_ok_and(|v| v == "1") {
        for (probe, outcome) in &rows {
            if matches!(probe.extension, "AVX2" | "FMA" | "BMI1" | "BMI2") {
                let ran =
                    matches!(outcome, Outcome::Runs(eax) if probe.eax.is_none_or(|e| e == *eax));
                assert!(
                    ran,
                    "{} with ROSETTA_ADVERTISE_AVX=1: {outcome:?}",
                    probe.name
                );
            }
        }
    }

    // Every probe is in the table once, and the names tell them apart.
    assert_eq!(rows.len(), PROBES.len());
    for (i, probe) in PROBES.iter().enumerate() {
        assert!(PROBES[..i].iter().all(|p| p.name != probe.name));
        // What the report would name is what the probe is called.
        let named = super::unsupported_amd_instruction(probe.code);
        assert!(named.is_none() || named == Some(probe.name), "{named:?}");
    }
}
