//! Seeded random-input tests of the whole translator: whatever a title's
//! container holds, each stage answers with a value (a translation or a
//! [`ShaderError`](crate::ShaderError)) and never panics, in a build with
//! overflow checks and in one without.
//!
//! Inputs are made here from a seed with [`testkit::Rng`]: instruction
//! streams from the test assembler ([`asm`]) and from raw random words,
//! stage data and signatures with the numbers a title could write where
//! small ones are expected, and containers of the testkit's builders with
//! bits flipped, lengths cut and size fields edited. Nothing comes from a
//! compiled shader.
//!
//! What these tests prove is the absence of panics and of hangs for the
//! cases they run, and that every module they emit holds together (the
//! sizes, counts and registers it declares are the ones it has; see
//! `invariants`), not that an emitted module computes the right thing.
//! The second half is what a build without overflow checks is judged by:
//! there a wrapped size is no panic. A panic or a broken module they find
//! is fixed with an error value and its input kept as a test of its own,
//! named for what went wrong.
//!
//! * `VICTORY_FUZZ_CASES`: cases per test (the default is 500).
//! * `VICTORY_FUZZ_SEED`: the first case's seed (decimal or `0x` hex); case
//!   n has the seed plus n. A failure names its seed, and
//!   `VICTORY_FUZZ_SEED=<seed> VICTORY_FUZZ_CASES=1` with the test's name
//!   runs that case alone.
//! * `VICTORY_FUZZ_TRACE=1`: prints each seed before its case runs, past
//!   the harness's capture. A stack overflow ends the process without a
//!   panic to catch; the last line printed is then the seed that did it.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;

use crate::dxbc::{Container, FourCC};
use crate::emit::HostShader;
use crate::gfx10::Encoding;
use crate::gfx10::asm::{self, Src};
use crate::native::{
    ComputeIo, ExportPatch, HwStage, KIND_NATIVE, MeshIo, NativeProgram, PsInput, ResourceType,
    RootParam, RootType, StageIo, Usage, UsedResource, VsInput, VsOutput, native_programs,
};
use crate::testkit::{self, Element, Rng};
use crate::{ShaderError, emit, io, ir, resource, structure};

/// Cases per test without `VICTORY_FUZZ_CASES`.
const DEFAULT_CASES: u64 = 500;
/// How long a case may take before a test calls it a hang. Cases take
/// milliseconds; this is for a loaded machine and an emulated one.
const HANG: Duration = Duration::from_secs(120);
/// A worker's stack: the translator recurses over values and blocks, and a
/// test thread's default 2 MiB is less than the 8 MiB a main thread gets.
const STACK: usize = 8 << 20;

/// A case: makes its input from the generator and runs it. The answer is
/// whether the input translated.
type Case = fn(&mut Gen) -> bool;

/// What a run did, for the checks that it was not all shallow.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    cases: u64,
    translated: u64,
    /// The defaults were in force (no variable changed cases or seed).
    defaults: bool,
}

fn number(var: &str) -> Option<u64> {
    let text = std::env::var(var).ok()?;
    let text = text.trim();
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    Some(parsed.unwrap_or_else(|_| panic!("{var}={text:?} is not a number")))
}

/// A case's source of choices: the seeded generator, and whether the case
/// is a tame one. A tame case keeps to what a compiled program does
/// (registers the program has, the descriptor its prologue loaded, forward
/// branches, signatures of four channels), so that it gets past the decoder
/// and into the stages behind it; a wild one does not care.
#[derive(Debug, Clone)]
struct Gen {
    rng: Rng,
    tame: bool,
}

impl Gen {
    fn next_u64(&mut self) -> u64 {
        self.rng.next_u64()
    }

    fn next_u32(&mut self) -> u32 {
        self.rng.next_u32()
    }

    fn below(&mut self, bound: u32) -> u32 {
        self.rng.below(bound)
    }
}

/// The generator of the case with `seed`. Seeds of one run are consecutive
/// numbers, which xorshift would turn into similar first outputs: the seed
/// is spread first (the multiplier of splitmix64).
fn generator(seed: u64) -> Gen {
    let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
    for _ in 0..4 {
        rng.next_u64();
    }
    Gen { rng, tame: false }
}

/// Prints `line` past the harness's capture.
fn trace(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Runs `case` for each seed of the run on one worker thread with a
/// [`STACK`]-sized stack, each case inside `catch_unwind`. Fails, naming
/// the seed, on the first case that panics (a translation that breaks an
/// invariant is such a case) or takes longer than `budget`.
fn drive(default_seed: u64, budget: Duration, case: Case) -> Tally {
    let cases = number("VICTORY_FUZZ_CASES");
    let seed = number("VICTORY_FUZZ_SEED");
    let defaults = cases.is_none() && seed.is_none();
    let (cases, first) = (cases.unwrap_or(DEFAULT_CASES), seed.unwrap_or(default_seed));
    let tracing = std::env::var("VICTORY_FUZZ_TRACE").is_ok_and(|v| v.trim() == "1");
    let test = std::thread::current().name().unwrap_or("?").to_string();
    let rerun = |seed: u64| format!("VICTORY_FUZZ_SEED={seed:#x} VICTORY_FUZZ_CASES=1 ({test})");

    enum Message {
        Started(u64),
        Finished(Result<bool, String>),
    }
    let (send, receive) = mpsc::channel();
    let worker_test = test.clone();
    let worker = std::thread::Builder::new()
        .name(test.clone())
        .stack_size(STACK)
        .spawn(move || {
            for n in 0..cases {
                let seed = first.wrapping_add(n);
                if tracing {
                    trace(&format!("fuzz: {worker_test} seed {seed:#x}"));
                }
                if send.send(Message::Started(seed)).is_err() {
                    return;
                }
                let outcome = catch_unwind(AssertUnwindSafe(|| case(&mut generator(seed))));
                let outcome = outcome.map_err(|payload| {
                    let text = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied());
                    text.unwrap_or("a panic without a message").to_string()
                });
                if send.send(Message::Finished(outcome)).is_err() {
                    return;
                }
            }
        })
        .expect("a worker thread");

    let mut tally = Tally {
        defaults,
        ..Tally::default()
    };
    // The worker hangs up when it is done.
    while let Ok(message) = receive.recv() {
        let Message::Started(seed) = message else {
            unreachable!("a case finishes after it starts");
        };
        match receive.recv_timeout(budget) {
            Ok(Message::Finished(Ok(translated))) => {
                tally.cases += 1;
                tally.translated += u64::from(translated);
            }
            Ok(Message::Finished(Err(panic))) => {
                panic!("seed {seed:#x} panicked: {panic}\nrerun: {}", rerun(seed))
            }
            Ok(Message::Started(_)) => unreachable!("a case starts after the last finished"),
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "seed {seed:#x} did not finish within {budget:?}\nrerun: {}",
                rerun(seed)
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "seed {seed:#x}: the worker ended without an answer\nrerun: {}",
                    rerun(seed)
                )
            }
        }
    }
    worker.join().expect("the worker catches its cases' panics");
    assert_eq!(tally.cases, cases, "every case ran");
    if tracing {
        trace(&format!(
            "fuzz: {test}: {} cases, {} translated",
            tally.cases, tally.translated
        ));
    }
    tally
}

// Small choices.

fn chance(r: &mut Gen, one_in: u32) -> bool {
    r.below(one_in) == 0
}

fn pick<T: Copy>(r: &mut Gen, from: &[T]) -> T {
    from[r.below(from.len() as u32) as usize]
}

fn byte(r: &mut Gen) -> u8 {
    r.next_u32() as u8
}

/// A number where a title writes a small one: below `small` most of the
/// time, else a value at the edge of some width or anything at all.
fn wild(r: &mut Gen, small: u32) -> u32 {
    const EDGES: [u32; 16] = [
        0,
        1,
        31,
        32,
        33,
        255,
        256,
        1024,
        1025,
        0xFFFF,
        0x1_0000,
        0x1000_0000,
        0x7FFF_FFFF,
        0x8000_0000,
        u32::MAX - 1,
        u32::MAX,
    ];
    match r.below(16) {
        0 => pick(r, &EDGES),
        1 => r.next_u32(),
        _ => r.below(small),
    }
}

/// [`wild`], as the byte a stage-data entry holds.
fn wild8(r: &mut Gen, small: u32) -> u8 {
    wild(r, small) as u8
}

// Instructions.

/// A VGPR number: a low one most of the time, so that instructions read
/// what others wrote.
fn vreg(r: &mut Gen) -> u8 {
    if chance(r, 8) && !r.tame {
        byte(r)
    } else {
        r.below(8) as u8
    }
}

/// A scalar destination: an SGPR, now and then a special register.
fn sreg(r: &mut Gen) -> u8 {
    if r.tame {
        // Past the root parameters and the descriptor of the prologue.
        24 + r.below(16) as u8
    } else if chance(r, 8) {
        r.below(128) as u8
    } else {
        r.below(24) as u8
    }
}

/// The first SGPR of a descriptor: [`DESCRIPTOR`] often, else any multiple
/// of 4.
fn descriptor(r: &mut Gen) -> u8 {
    if r.tame || chance(r, 2) {
        DESCRIPTOR
    } else {
        4 * r.below(32) as u8
    }
}

/// A scalar source: an SGPR, a special register, an inline constant or the
/// instruction's literal.
fn scalar(r: &mut Gen, literal: u32) -> Src {
    match r.below(8) {
        0 => asm::lit32(literal),
        1 if r.tame => asm::imm(r.below(65) as i32),
        1 => Src::code(128 + r.below(127) as u16),
        // Tame: a root constant, or a register `sreg` writes.
        2 if r.tame => asm::sgpr(r.below(4) as u8),
        2 => Src::code(r.below(255) as u16),
        _ if r.tame => asm::sgpr(24 + r.below(16) as u8),
        _ => asm::sgpr(r.below(24) as u8),
    }
}

/// Any source: a VGPR most of the time.
fn source(r: &mut Gen, literal: u32) -> Src {
    if chance(r, 4) {
        scalar(r, literal)
    } else {
        asm::vgpr(vreg(r))
    }
}

/// Now and then the SDWA or DPP form of a one-dword VOP1, VOP2 or VOPC
/// instruction (one whose source is no literal).
fn extended(r: &mut Gen, inst: Vec<u32>) -> Vec<u32> {
    if inst.len() != 1 || !chance(r, 4) {
        return inst;
    }
    let vector_source = inst[0] & 0x1FF >= 256;
    let compare = inst[0] >> 25 == 0x3E;
    match r.below(3) {
        0 if vector_source => asm::dpp16(
            &inst,
            asm::Dpp16Mods {
                ctrl: r.below(0x200) as u16,
                row_mask: byte(r),
                bank_mask: byte(r),
                bound_ctrl: chance(r, 2),
                fi: chance(r, 2),
                neg: byte(r),
                abs: byte(r),
            },
        ),
        1 if vector_source => {
            let lanes = r.next_u64().to_le_bytes();
            asm::dpp8(&inst, lanes, chance(r, 2))
        }
        _ => asm::sdwa(
            &inst,
            asm::SdwaMods {
                dst_sel: byte(r),
                dst_unused: byte(r),
                src_sel: [byte(r), byte(r)],
                sext: byte(r),
                neg: byte(r),
                abs: byte(r),
                clamp: chance(r, 2),
                omod: byte(r),
                scalar_src1: chance(r, 4),
                sdst: (compare && chance(r, 2)).then(|| sreg(r)),
            },
        ),
    }
}

fn buffer(r: &mut Gen) -> asm::Buffer {
    asm::Buffer {
        vdata: vreg(r),
        vaddr: vreg(r),
        srsrc: descriptor(r),
        soffset: if chance(r, 2) {
            asm::imm(0)
        } else {
            asm::sgpr(r.below(24) as u8)
        },
        offset: if chance(r, 4) {
            r.below(0x1000) as u16
        } else {
            4 * r.below(16) as u16
        },
        offen: chance(r, 4),
        idxen: chance(r, 4),
        glc: chance(r, 8),
        slc: chance(r, 8),
        dlc: chance(r, 8),
        lds: chance(r, 16),
        tfe: chance(r, 16),
    }
}

const ENCODINGS: [Encoding; 20] = [
    Encoding::Sop2,
    Encoding::Sopk,
    Encoding::Sop1,
    Encoding::Sopc,
    Encoding::Sopp,
    Encoding::Smem,
    Encoding::Vop2,
    Encoding::Vop1,
    Encoding::Vopc,
    Encoding::Vop3,
    Encoding::Vop3p,
    Encoding::Vintrp,
    Encoding::Ds,
    Encoding::Mubuf,
    Encoding::Mtbuf,
    Encoding::Mimg,
    Encoding::Exp,
    Encoding::Flat,
    Encoding::Global,
    Encoding::Scratch,
];

/// The encodings of a tame case: arithmetic, compares, LDS and buffer
/// accesses. Its branches are made apart ([`tame_noise`]).
const TAME_ENCODINGS: [Encoding; 12] = [
    Encoding::Sop2,
    Encoding::Sopk,
    Encoding::Sop1,
    Encoding::Sopc,
    Encoding::Vop2,
    Encoding::Vop2,
    Encoding::Vop1,
    Encoding::Vopc,
    Encoding::Vop3,
    Encoding::Vop3p,
    Encoding::Ds,
    Encoding::Mubuf,
];

/// One instruction of a random encoding and opcode with random operands,
/// in the layout the assembler gives it.
fn instruction(r: &mut Gen) -> Vec<u32> {
    let literal = if chance(r, 2) {
        r.next_u32()
    } else {
        wild(r, 64)
    };
    let encoding = if r.tame {
        pick(r, &TAME_ENCODINGS)
    } else {
        pick(r, &ENCODINGS)
    };
    let name = pick(r, &asm::names(encoding));
    match encoding {
        Encoding::Sop2 => asm::sop2(name, sreg(r), scalar(r, literal), scalar(r, literal)),
        // The two-dword form the assembler leaves out: its first word.
        Encoding::Sopk if name == "s_setreg_imm32_b32" => {
            vec![0xB000_0000 | u32::from(asm::opcode(encoding, name)) << 23 | r.below(0x80_0000)]
        }
        Encoding::Sopk => asm::sopk(name, sreg(r), r.next_u32() as u16),
        Encoding::Sop1 => asm::sop1(name, sreg(r), scalar(r, literal)),
        Encoding::Sopc => asm::sopc(name, scalar(r, literal), scalar(r, literal)),
        Encoding::Sopp => {
            // Branch offsets that land near by, most of the time.
            let imm = if chance(r, 4) {
                r.next_u32() as u16
            } else {
                (r.below(13) as i16 - 4) as u16
            };
            asm::sopp(name, imm)
        }
        Encoding::Smem => {
            let offset = if chance(r, 4) {
                r.below(1 << 21) as i32 - (1 << 20)
            } else {
                4 * r.below(16) as i32
            };
            let soffset = if chance(r, 4) { sreg(r) } else { asm::NULL };
            asm::smem(name, sreg(r), 2 * r.below(16) as u8, offset, soffset)
        }
        Encoding::Vop1 => {
            let inst = asm::vop1(name, vreg(r), source(r, literal));
            extended(r, inst)
        }
        Encoding::Vop2 => {
            let inst = asm::vop2(name, vreg(r), source(r, literal), vreg(r));
            extended(r, inst)
        }
        Encoding::Vopc => {
            let inst = asm::vopc(name, source(r, literal), vreg(r));
            extended(r, inst)
        }
        Encoding::Vop3 | Encoding::Vop3p => {
            let srcs: Vec<Src> = (0..1 + r.below(3)).map(|_| source(r, literal)).collect();
            if encoding == Encoding::Vop3p {
                let mods = asm::Vop3pMods {
                    neg: byte(r),
                    neg_hi: byte(r),
                    opsel: byte(r),
                    opsel_hi: byte(r),
                    clamp: chance(r, 4),
                };
                asm::vop3p(name, vreg(r), &srcs, mods)
            } else if chance(r, 4) {
                asm::vop3b(name, vreg(r), sreg(r), &srcs)
            } else {
                let plain = chance(r, 2);
                let mods = asm::Vop3Mods {
                    abs: if plain { 0 } else { byte(r) },
                    neg: if plain { 0 } else { byte(r) },
                    opsel: if plain { 0 } else { byte(r) },
                    omod: if plain { 0 } else { byte(r) },
                    clamp: !plain && chance(r, 2),
                };
                asm::vop3(name, vreg(r), &srcs, mods)
            }
        }
        Encoding::Vintrp => asm::vintrp(name, vreg(r), vreg(r), wild8(r, 4), byte(r)),
        Encoding::Ds => asm::ds(
            name,
            asm::Ds {
                vdst: vreg(r),
                addr: vreg(r),
                data0: vreg(r),
                data1: vreg(r),
                offset0: wild8(r, 16),
                offset1: wild8(r, 16),
                gds: chance(r, 16),
            },
        ),
        Encoding::Mubuf => asm::mubuf(name, buffer(r)),
        Encoding::Mtbuf => asm::mtbuf(
            name,
            byte(r),
            asm::Buffer {
                lds: false,
                ..buffer(r)
            },
        ),
        Encoding::Mimg => {
            let count = if chance(r, 4) {
                1 + r.below(13)
            } else {
                1 + r.below(4)
            };
            let addr: Vec<u8> = (0..count).map(|_| vreg(r)).collect();
            asm::mimg(
                name,
                asm::Image {
                    vdata: vreg(r),
                    addr: &addr,
                    srsrc: descriptor(r),
                    ssamp: descriptor(r),
                    dmask: byte(r),
                    dim: byte(r),
                    unorm: chance(r, 4),
                    glc: chance(r, 8),
                    slc: chance(r, 8),
                    dlc: chance(r, 8),
                    r128: chance(r, 4),
                    tfe: chance(r, 16),
                    lwe: chance(r, 16),
                    a16: chance(r, 4),
                    d16: chance(r, 4),
                },
            )
        }
        Encoding::Exp => {
            let target = match r.below(8) {
                0 => r.below(64) as u8,
                1 => asm::target::MRTZ,
                2 => asm::target::POS0 + r.below(4) as u8,
                3 => asm::target::PRIM,
                4 | 5 => asm::target::PARAM0 + r.below(4) as u8,
                _ => asm::target::MRT0 + r.below(8) as u8,
            };
            let flags = asm::ExpFlags {
                compr: chance(r, 4),
                done: chance(r, 2),
                vm: chance(r, 2),
            };
            asm::exp(target, byte(r), [vreg(r), vreg(r), vreg(r), vreg(r)], flags)
        }
        Encoding::Flat | Encoding::Global | Encoding::Scratch => {
            let offset = if encoding == Encoding::Flat {
                r.below(0x800) as i16
            } else {
                r.below(0x1000) as i16 - 0x800
            };
            asm::flat(
                encoding,
                name,
                asm::Flat {
                    vdst: vreg(r),
                    addr: vreg(r),
                    data: vreg(r),
                    saddr: if chance(r, 2) { asm::NULL } else { sreg(r) },
                    offset,
                    lds: chance(r, 16),
                    glc: chance(r, 8),
                    slc: chance(r, 8),
                    dlc: chance(r, 8),
                },
            )
        }
    }
}

/// A tame instruction: one the first stage takes when it stands alone
/// (an opcode it knows, operands that exist), found by asking it. `v_mov_b32
/// v1, v0` where sixteen tries found none.
fn tame_instruction(r: &mut Gen) -> Vec<u32> {
    for _ in 0..16 {
        let words = instruction(r);
        let ended: Vec<u32> = words.iter().copied().chain([asm::S_ENDPGM]).collect();
        let alone = ir::build(&asm::bytes(&ended), false, 32);
        if alone.is_ok_and(|p| p.blocks.len() == 1 && p.blocks[0].insts.len() == 2) {
            return words;
        }
    }
    asm::vop1("v_mov_b32", 1, asm::vgpr(0))
}

/// Up to `most` tame instructions, some of them behind a forward branch
/// over the next few.
fn tame_noise(r: &mut Gen, most: u32) -> Vec<u32> {
    const BRANCHES: [&str; 7] = [
        "s_branch",
        "s_cbranch_scc0",
        "s_cbranch_scc1",
        "s_cbranch_vccz",
        "s_cbranch_vccnz",
        "s_cbranch_execz",
        "s_cbranch_execnz",
    ];
    let mut words = Vec::new();
    let mut left = r.below(most + 1);
    while left > 0 {
        let skipped = if chance(r, 6) {
            1 + r.below(3).min(left - 1)
        } else {
            0
        };
        let body: Vec<u32> = (0..skipped.max(1))
            .flat_map(|_| tame_instruction(r))
            .collect();
        if skipped > 0 {
            words.extend(asm::sopp(pick(r, &BRANCHES), body.len() as u16));
        }
        words.extend(body);
        left -= skipped.max(1);
    }
    words
}

/// Up to `most` instructions, one in `raw_one_in` of them a random word
/// instead of an assembled instruction.
fn noise(r: &mut Gen, most: u32, raw_one_in: u32) -> Vec<u32> {
    let mut words = Vec::new();
    for _ in 0..r.below(most + 1) {
        if chance(r, raw_one_in) {
            words.push(r.next_u32());
        } else {
            words.extend(instruction(r));
        }
    }
    words
}

// Stage data, root layouts and signatures.

/// Where the prologue of [`with_prologue`] puts its descriptor.
const DESCRIPTOR: u8 = 16;

const STAGES: [HwStage; 8] = [
    HwStage::Cs,
    HwStage::Ps,
    HwStage::Vs,
    HwStage::Ms,
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
];

fn usage(r: &mut Gen) -> Usage {
    match r.below(8) {
        0 => Usage::ClipDistance,
        1 => Usage::Other(byte(r)),
        _ => Usage::Generic,
    }
}

fn vs_outputs(r: &mut Gen) -> Vec<VsOutput> {
    (0..r.below(5))
        .map(|_| VsOutput {
            usage: usage(r),
            row: wild8(r, 4),
            // Bit 7: a mesh shader's per-primitive output.
            param: wild8(r, 4) | if chance(r, 4) { 0x80 } else { 0 },
            fp16: chance(r, 4),
            pack2: chance(r, 4),
        })
        .collect()
}

/// Stage data for `stage` with the numbers a container could hold, within
/// what the testkit's container builder can write (16-bit mesh limits, one
/// size for all export patches, at most four semantics).
fn stage_io(r: &mut Gen, stage: HwStage, code_words: usize) -> StageIo {
    match stage {
        HwStage::Cs => {
            let rsrc2 = if chance(r, 8) {
                r.next_u32()
            } else {
                // User SGPRs, the system SGPRs, thread id components (3 is
                // malformed), LDS granules.
                r.below(17) << 1
                    | r.below(16) << 7
                    | (if chance(r, 16) { 3 } else { r.below(3) }) << 11
                    | (if chance(r, 2) { 0 } else { wild(r, 4) & 0x1FF }) << 15
                    | r.below(2)
            };
            let threads = match r.below(8) {
                0 => [wild(r, 64), wild(r, 64), wild(r, 64)],
                1 => [r.next_u32(), r.next_u32(), r.next_u32()],
                _ => pick(
                    r,
                    &[
                        [1, 1, 1],
                        [8, 8, 1],
                        [64, 1, 1],
                        [4, 4, 4],
                        [32, 32, 1],
                        [16, 16, 1],
                    ],
                ),
            };
            StageIo::Cs(ComputeIo { rsrc2, threads })
        }
        HwStage::Ps => {
            let inputs = (0..r.below(5))
                .map(|_| PsInput {
                    usage: usage(r),
                    row: wild8(r, 4),
                    mask: r.below(16) as u8,
                    attr: wild8(r, 4),
                    flat: chance(r, 4),
                    fp16: chance(r, 4),
                    pack2: chance(r, 4),
                    row2: wild8(r, 4),
                })
                .collect();
            let mut patches = Vec::new();
            if chance(r, 4) {
                let size = r.below(11) as usize;
                let mut mrts: Vec<u8> = (0..8).filter(|_| chance(r, 4)).collect();
                mrts.truncate(3);
                for mrt in mrts {
                    patches.push(ExportPatch {
                        mrt,
                        offset: if chance(r, 8) {
                            wild(r, 64)
                        } else {
                            r.below(code_words.max(1) as u32)
                        },
                        code: (0..size).map(|_| instruction(r)[0]).chain(None).collect(),
                    });
                }
            }
            StageIo::Ps {
                inputs,
                input_addr: if chance(r, 4) {
                    r.next_u32()
                } else {
                    r.below(1 << 16)
                },
                db_shader_control: if chance(r, 2) { 0 } else { r.next_u32() },
                patches,
            }
        }
        HwStage::Vs => StageIo::Vs {
            inputs: (0..r.below(5))
                .map(|_| VsInput {
                    usage: usage(r),
                    row: wild8(r, 4),
                    mask: r.below(16) as u8,
                    vgpr: wild8(r, 16),
                })
                .collect(),
            outputs: vs_outputs(r),
        },
        HwStage::Ms => StageIo::Ms(MeshIo {
            outputs: vs_outputs(r),
            clip_cntl: if chance(r, 2) { 0 } else { r.next_u32() },
            threads: if chance(r, 4) {
                wild(r, 256) & 0xFFFF
            } else {
                pick(r, &[32, 64, 128])
            },
            max_vertices: wild(r, 257) & 0xFFFF,
            max_primitives: wild(r, 257) & 0xFFFF,
            topology: if chance(r, 4) { byte(r) } else { 2 },
            lds_bytes: if chance(r, 2) { 0 } else { wild(r, 4096) },
        }),
        _ => StageIo::None,
    }
}

fn root_type(r: &mut Gen) -> RootType {
    match r.below(8) {
        0 => RootType::Descriptor,
        1 | 2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        // A kind the parser does not know (a byte, as a used resource has it).
        6 => RootType::Other(7 + r.below(249)),
        _ => RootType::Constants,
    }
}

fn layout(r: &mut Gen) -> Vec<RootParam> {
    (0..r.below(5))
        .map(|_| RootParam {
            root: root_type(r),
            table_offset_dwords: wild(r, 16),
            sgprs: wild(r, 5),
            base_register: wild(r, 8),
            user_data: wild8(r, 16),
            slot: wild8(r, 8),
        })
        .collect()
}

fn used(r: &mut Gen) -> Vec<UsedResource> {
    (0..r.below(5))
        .map(|_| UsedResource {
            kind: match r.below(8) {
                0 => ResourceType::Other(byte(r)),
                1 | 2 => ResourceType::Sampler,
                3 | 4 => ResourceType::Cbv,
                5 => ResourceType::Uav,
                _ => ResourceType::Srv,
            },
            root: root_type(r),
            table_offset_dwords: wild(r, 16),
            lower_bound: wild(r, 8),
            stride: wild(r, 8),
            dwords: if chance(r, 8) {
                byte(r)
            } else {
                pick(r, &[4, 8])
            },
            user_data: wild8(r, 16),
            slot: wild8(r, 8),
        })
        .collect()
}

/// Up to four signature rows: user semantics and system values, in rows a
/// signature has and now and then in ones it has not.
fn signature(r: &mut Gen) -> Vec<Element> {
    const NAMES: [(&str, u32); 14] = [
        ("TEXCOORD", 0),
        ("COLOR", 0),
        ("SV_Position", 1),
        ("SV_ClipDistance", 2),
        ("SV_CullDistance", 3),
        ("SV_PrimitiveID", 7),
        ("SV_IsFrontFace", 9),
        ("SV_SampleIndex", 10),
        ("SV_Target", 64),
        ("SV_Depth", 65),
        ("SV_Coverage", 66),
        ("SV_DepthGreaterEqual", 67),
        ("SV_StencilRef", 69),
        ("SV_Unknown", 99),
    ];
    (0..r.below(5))
        .map(|_| {
            let (name, system_value) = pick(r, &NAMES);
            Element {
                name,
                index: wild(r, 4),
                system_value,
                component: if chance(r, 8) { wild(r, 8) } else { 3 },
                register: if chance(r, 8) {
                    pick(r, &[31, 32, 0x1000_0000, u32::MAX - 1, u32::MAX])
                } else {
                    r.below(4)
                },
                mask: if chance(r, 8) {
                    byte(r)
                } else {
                    pick(r, &[0xF, 0x7, 0x3, 0x1, 0x8, 0xC])
                },
            }
        })
        .collect()
}

/// A program of a random stage around `words`, with random stage data,
/// root layout and used resources.
fn program(r: &mut Gen, stage: HwStage, words: &[u32]) -> NativeProgram {
    NativeProgram {
        stage,
        wave64: chance(r, 2),
        vgprs: if chance(r, 8) {
            r.below(257) as u16
        } else {
            pick(r, &[8, 16, 32, 64, 256])
        },
        layout: layout(r),
        used: used(r),
        io: stage_io(r, stage, words.len()),
        code: asm::bytes(words),
    }
}

// The pipeline.

/// Every stage of the translator over `native`, as the `victory` tool's
/// shader survey and [`emit::translate`] run them: each stage runs even
/// where an earlier one refused, as long as it has its inputs. Whether the
/// program translated.
fn stages(c: &Container, native: &NativeProgram) -> bool {
    let Ok(code) = native.patched_code() else {
        return false;
    };
    let Ok(p) = ir::build(&code, native.wave64, native.vgprs) else {
        return false;
    };
    let st = structure::structure(&p);
    let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok());
    let io = io::raise(&p, native);
    let translates = matches!(
        native.stage,
        HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms
    );
    match (st, res, io) {
        (Ok(st), Ok(res), Ok(io)) if translates => {
            match emit::emit(c, native, &p, &st, &io, &res) {
                Ok(shader) => {
                    hold(&shader, Some(native));
                    true
                }
                Err(_) => false,
            }
        }
        _ => false,
    }
}

/// Fails the case (the driver names its seed) if `shader` breaks one of
/// the [`invariants`].
fn hold(shader: &HostShader, native: Option<&NativeProgram>) {
    if let Err(broken) = invariants(shader, native) {
        panic!("a translation that does not hold together: {broken}");
    }
}

/// A container's bytes through the parser and the translator, then each
/// program the parser finds through [`stages`]. Whether anything translated.
fn whole(container: &[u8]) -> bool {
    let mut translated = false;
    if let Ok(shader) = emit::translate(container) {
        hold(&shader, None);
        translated = true;
    }
    let Ok(c) = Container::parse(container) else {
        return translated;
    };
    let programs = c
        .part(FourCC::DXIL)
        .map(|part| native_programs(part.data))
        .and_then(Result::ok)
        .unwrap_or_default();
    for native in &programs {
        translated |= stages(&c, native);
    }
    translated
}

/// A container with the signature parts of `signatures` (empty ones left
/// out) and, for a mesh program, now and then a `PSV0` part.
fn parts_for(r: &mut Gen, stage: HwStage, signatures: &[Vec<Element>; 3]) -> Vec<u8> {
    let sigs: Vec<Vec<u8>> = signatures
        .iter()
        .map(|s| testkit::signature_part(s))
        .collect();
    let mut psv = Vec::new();
    if stage == HwStage::Ms && chance(r, 2) {
        // The runtime info's size, then the info: the thread count of a
        // mesh group is read from it.
        psv.extend(pick(r, &[48u32, 52, 24, 0, u32::MAX]).to_le_bytes());
        psv.resize(4 + 4 * r.below(16) as usize, 0);
        if psv.len() >= 48 {
            psv[40..44].copy_from_slice(&wild(r, 129).to_le_bytes());
        }
    }
    let mut parts: Vec<(FourCC, &[u8])> = Vec::new();
    for (fourcc, sig) in [FourCC::ISG1, FourCC::OSG1, FourCC::PSG1]
        .into_iter()
        .zip(&sigs)
    {
        if sig.len() > 8 {
            parts.push((fourcc, sig));
        }
    }
    if !psv.is_empty() {
        parts.push((FourCC::PSV0, &psv));
    }
    crate::dxbc::build(&parts)
}

/// Runs `native` with random signatures, as a program built by hand.
fn run_program(r: &mut Gen, native: &NativeProgram) -> bool {
    let signatures = [signature(r), signature(r), signature(r)];
    run_with(r, native, &signatures)
}

/// Runs `native` with the input, output and primitive `signatures`.
fn run_with(r: &mut Gen, native: &NativeProgram, signatures: &[Vec<Element>; 3]) -> bool {
    let container = parts_for(r, native.stage, signatures);
    let c = Container::parse(&container).expect("the builder's own container");
    stages(&c, native)
}

// What every translation must satisfy.
//
// "Never panics" proves little about arithmetic in a build without
// overflow checks, which is the build that ships: a size, count or offset
// of the title's that wraps is silent there. So every case that translates
// is also checked by value. The checks below read the emitted container
// the way a consumer does, with their own arithmetic in 64 bits, and hold
// each declared size, count and register against what is really there; a
// wrapped or truncated number in the translator shows up as a size that
// does not match, in either build.

/// The 32-bit word at `at` of `data`, which `what` names.
fn word(data: &[u8], at: u64, what: &str) -> Result<u32, String> {
    usize::try_from(at)
        .ok()
        .and_then(|at| data.get(at..at.checked_add(4)?))
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        .ok_or_else(|| format!("{what}: no word at {at:#x} of {} bytes", data.len()))
}

fn byte_at(data: &[u8], at: u64, what: &str) -> Result<u8, String> {
    usize::try_from(at)
        .ok()
        .and_then(|at| data.get(at).copied())
        .ok_or_else(|| format!("{what}: no byte at {at:#x} of {} bytes", data.len()))
}

/// The parts of an emitted container. The size the header declares is the
/// file's, the parts lie one after another from the end of the offset
/// table to the end of the file, and the digest is that of the bytes.
fn container_parts(file: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
    if file.get(..4) != Some(b"DXBC") {
        return Err("the container does not start with DXBC".into());
    }
    let (total, len) = (word(file, 0x18, "container")?, file.len() as u64);
    if u64::from(total) != len {
        return Err(format!("the container says {total} bytes and has {len}"));
    }
    let count = word(file, 0x1C, "container")?;
    if count > 16 {
        return Err(format!("{count} parts"));
    }
    let mut parts = Vec::new();
    let mut end = 0x20 + 4 * u64::from(count);
    for i in 0..u64::from(count) {
        let offset = u64::from(word(file, 0x20 + 4 * i, "part offsets")?);
        if offset != end {
            return Err(format!(
                "part {i} starts at {offset:#x}, not where the one before it ends ({end:#x})"
            ));
        }
        let fourcc = word(file, offset, "part header")?.to_le_bytes();
        let size = u64::from(word(file, offset + 4, "part header")?);
        end = offset + 8 + size;
        if end > len {
            return Err(format!(
                "part {i} of {size} bytes at {offset:#x} runs past the file's {len}"
            ));
        }
        parts.push((fourcc, &file[offset as usize + 8..end as usize]));
    }
    if end != len {
        return Err(format!("the parts end at {end:#x}, the file at {len:#x}"));
    }
    if file[4..20] != emit::container_hash(&file[20..]) {
        return Err("the container's digest is not that of its bytes".into());
    }
    Ok(parts)
}

/// The rows of an emitted signature part as (register, mask): the count
/// fits the part, every name lies in the part behind the rows and ends
/// there, and a register is one of D3D12's 32 or none.
fn signature_rows(part: &[u8], name: &str) -> Result<Vec<(u32, u8)>, String> {
    let (count, len) = (u64::from(word(part, 0, name)?), part.len() as u64);
    if word(part, 4, name)? != 8 {
        return Err(format!("{name}: rows not at 8"));
    }
    let names = 8 + 32 * count;
    if names > len {
        return Err(format!("{name}: {count} rows in {len} bytes"));
    }
    let mut rows = Vec::new();
    for at in (0..count).map(|i| 8 + 32 * i) {
        let text = u64::from(word(part, at + 4, name)?);
        if text < names || text >= len || !part[text as usize..].contains(&0) {
            return Err(format!("{name}: a name at {text:#x} of {len:#x} bytes"));
        }
        let register = word(part, at + 20, name)?;
        if register != u32::MAX && register >= 32 {
            return Err(format!("{name}: register {register}"));
        }
        let (mask, used) = (part[at as usize + 24], part[at as usize + 25]);
        if mask > 0xF || used > 0xF {
            return Err(format!("{name}: masks {mask:#x} and {used:#x}"));
        }
        rows.push((register, mask));
    }
    Ok(rows)
}

/// Registers the rows of a signature take: one past the highest.
fn vectors(rows: &[(u32, u8)]) -> u64 {
    let allocated = rows.iter().filter(|r| r.0 != u32::MAX);
    allocated.map(|r| u64::from(r.0) + 1).max().unwrap_or(0)
}

/// An emitted `PSV0` part against the signatures beside it (input,
/// output, per-primitive rows): its counts are theirs, its tables have
/// the sizes its counts say, every reference lies inside its table, and
/// the tables fill the part exactly. Its resource bindings are returned
/// as (type, space, register).
fn psv_holds(
    psv: &[u8],
    kind: u32,
    rows: [&[(u32, u8)]; 3],
    mesh: bool,
) -> Result<Vec<(u32, u32, u32)>, String> {
    let w = |at: u64| word(psv, at, "PSV0");
    let b = |at: u64| byte_at(psv, at, "PSV0").map(u64::from);
    if w(0)? != 48 {
        return Err("PSV0: runtime info not of 48 bytes".into());
    }
    let info = 4;
    if b(info + 24)? != u64::from(kind) {
        return Err(format!("PSV0: stage {}, not {kind}", b(info + 24)?));
    }
    let counted = [b(info + 28)?, b(info + 29)?, b(info + 30)?];
    let declared = [
        rows[0].len() as u64,
        rows[1].len() as u64,
        if mesh { rows[2].len() as u64 } else { 0 },
    ];
    if counted != declared {
        return Err(format!(
            "PSV0 counts {counted:?} signature elements, the signatures have {declared:?}"
        ));
    }
    let (ins, outs) = (vectors(rows[0]), vectors(rows[1]));
    let prim_vectors = if mesh { vectors(rows[2]) } else { 0 };
    let said = [b(info + 31)?, b(info + 32)?, b(info + 26)?];
    // In a mesh program byte 26 is the per-primitive vectors; else unused.
    if said[..2] != [ins, outs] || (mesh && said[2] != prim_vectors) {
        return Err(format!(
            "PSV0 says {said:?} vectors, the signatures take {ins}, {outs} and {prim_vectors}"
        ));
    }
    if kind == 5 {
        let t = [w(info + 36)?, w(info + 40)?, w(info + 44)?];
        let each = t[0] <= 1024 && t[1] <= 1024 && t[2] <= 64 && !t.contains(&0);
        if !each || t.iter().map(|&n| u64::from(n)).product::<u64>() > 1024 {
            return Err(format!("PSV0: a thread group of {t:?}"));
        }
    }
    let mut at = info + 48;
    let bound = u64::from(w(at)?);
    at += 4;
    let mut bindings = Vec::new();
    if bound > 0 {
        if w(at)? != 24 {
            return Err("PSV0: bindings not of 24 bytes".into());
        }
        at += 4;
        for _ in 0..bound.min(1 << 16) {
            if w(at + 8)? != w(at + 12)? {
                return Err("PSV0: a binding of several registers".into());
            }
            bindings.push((w(at)?, w(at + 4)?, w(at + 8)?));
            at += 24;
        }
    }
    let strings = u64::from(w(at)?);
    if !strings.is_multiple_of(4) {
        return Err(format!("PSV0: {strings} bytes of names"));
    }
    at += 4 + strings;
    let indices = u64::from(w(at)?);
    at += 4 + 4 * indices;
    let elements = declared.iter().sum::<u64>();
    if elements > 0 {
        if w(at)? != 16 {
            return Err("PSV0: elements not of 16 bytes".into());
        }
        at += 4;
        for _ in 0..elements {
            let (name, index) = (u64::from(w(at)?), u64::from(w(at + 4)?));
            if name >= strings.max(1) || index >= indices {
                return Err(format!(
                    "PSV0: an element's name {name} of {strings} bytes, index {index} of {indices}"
                ));
            }
            if b(at + 9)? >= 32 {
                return Err(format!("PSV0: an element at register {}", b(at + 9)?));
            }
            at += 16;
        }
    }
    if ins > 0 && outs > 0 {
        at += 4 * (ins * 4) * (outs * 4).div_ceil(32);
    }
    if at != psv.len() as u64 {
        return Err(format!(
            "PSV0: its tables end at {at:#x}, the part at {:#x}",
            psv.len()
        ));
    }
    Ok(bindings)
}

/// An emitted `DXIL` part: the program header's size is the part's, the
/// bitcode it points at is the rest of the part, and the kind is `kind`.
fn program_holds(part: &[u8], kind: u32) -> Result<(), String> {
    let w = |at: u64| word(part, at, "DXIL");
    let len = part.len() as u64;
    if w(0)? != kind << 16 | 0x66 {
        return Err(format!("DXIL: program version {:#x}", w(0)?));
    }
    if u64::from(w(4)?) * 4 != len {
        return Err(format!(
            "the DXIL part says {} dwords and has {len} bytes",
            w(4)?
        ));
    }
    if part.get(8..12) != Some(b"DXIL") || w(16)? != 16 {
        return Err("DXIL: no bitcode header".into());
    }
    if u64::from(w(20)?) != len - 24 {
        return Err(format!(
            "DXIL: {} bytes of bitcode in a part with room for {}",
            w(20)?,
            len - 24
        ));
    }
    if part.get(24..28) != Some(&[b'B', b'C', 0xC0, 0xDE]) {
        return Err("DXIL: the bitcode does not start as bitcode".into());
    }
    Ok(())
}

/// What a translation must satisfy whatever its input was:
///
/// * the container is laid out as its header says ([`container_parts`])
///   and has the parts of its stage, each once and in the emitter's order;
/// * the signatures, `PSV0` and the program agree with each other and
///   with their own sizes ([`signature_rows`], [`psv_holds`],
///   [`program_holds`]);
/// * what the host is told to bind is what the container declares: a
///   static sampler per immediate sampler, at most the 64 the comparison
///   mask has bits for, each bound as its own register; every root CBV
///   and SRV once and bound at its slot's register; the user-data buffer
///   bound exactly when user data is read;
/// * each user-data dword is listed once, at the index of an SGPR (so
///   the buffer's size cannot wrap) which the buffer's size covers, and
///   (with the program at hand) comes from a root parameter of the
///   program's layout, at that parameter's own dword.
fn invariants(shader: &HostShader, native: Option<&NativeProgram>) -> Result<(), String> {
    use crate::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
    let (kind, mesh) = match shader.stage {
        HwStage::Ps => (0, false),
        HwStage::Vs => (1, false),
        HwStage::Cs => (5, false),
        HwStage::Ms => (13, true),
        other => return Err(format!("a translation of stage {other:?}")),
    };
    if native.is_some_and(|n| n.stage != shader.stage) {
        return Err(format!("stage {:?} of another program", shader.stage));
    }
    let parts = container_parts(&shader.dxil)?;
    let names: Vec<&[u8; 4]> = parts.iter().map(|p| &p.0).collect();
    let mut expected: Vec<&[u8; 4]> = vec![b"SFI0", b"ISG1", b"OSG1"];
    if mesh {
        expected.push(b"PSG1");
    }
    expected.extend([b"PSV0", b"HASH", b"DXIL"]);
    if names != expected {
        let names: Vec<String> = names.iter().map(|n| n.escape_ascii().to_string()).collect();
        return Err(format!("parts {names:?}"));
    }
    let part = |name: &[u8; 4]| {
        let found = parts.iter().find(|p| &p.0 == name);
        found.map(|p| p.1).unwrap_or_default()
    };
    if part(b"SFI0").len() != 8 || part(b"HASH").len() != 20 {
        return Err("SFI0 or HASH of another size".into());
    }
    let inputs = signature_rows(part(b"ISG1"), "ISG1")?;
    let outputs = signature_rows(part(b"OSG1"), "OSG1")?;
    let prims = if mesh {
        signature_rows(part(b"PSG1"), "PSG1")?
    } else {
        Vec::new()
    };
    let bindings = psv_holds(part(b"PSV0"), kind, [&inputs, &outputs, &prims], mesh)?;
    program_holds(part(b"DXIL"), kind)?;

    // What the host binds.
    let samplers = shader.samplers.len();
    if shader.static_samplers.len() != samplers || samplers > 64 {
        return Err(format!(
            "{samplers} immediate samplers, {} static ones",
            shader.static_samplers.len()
        ));
    }
    for (what, slots) in [("CBV", &shader.root_cbvs), ("SRV", &shader.root_srvs)] {
        let once: std::collections::HashSet<u8> = slots.iter().copied().collect();
        if once.len() != slots.len() {
            return Err(format!("root {what}s {slots:?} name a slot twice"));
        }
    }
    let mut bound: Vec<(u32, u32, u32)> = Vec::new();
    if !shader.user_data.is_empty() {
        bound.push((2, USER_DATA_SPACE, 0));
    }
    let slots = |kind, slots: &[u8]| {
        let registers = slots.iter().map(|&s| (kind, ROOT_CBV_SPACE, u32::from(s)));
        registers.collect::<Vec<_>>()
    };
    bound.extend(slots(2, &shader.root_cbvs));
    bound.extend(slots(4, &shader.root_srvs));
    bound.extend((0..samplers as u32).map(|n| (1, SAMPLER_SPACE, n)));
    if bindings != bound {
        return Err(format!(
            "PSV0 binds {bindings:?} (type, space, register), the host is told {bound:?}"
        ));
    }

    // User data.
    let mut seen = std::collections::HashSet::new();
    let mut dwords = 0u64;
    for u in &shader.user_data {
        if !seen.insert(u.index) {
            return Err(format!("user-data dword {} is listed twice", u.index));
        }
        // A user-data dword arrives in an SGPR, and there are s0 to s105.
        if u.index >= u32::from(ir::VCC) {
            return Err(format!("user-data dword {} is no SGPR", u.index));
        }
        dwords = dwords.max(u64::from(u.index) + 1);
        let Some(native) = native else { continue };
        let from_layout = native.layout.iter().any(|rp| {
            rp.slot == u.slot
                && rp.root == u.root
                && u64::from(rp.user_data) + u64::from(u.dword) == u64::from(u.index)
        });
        if !from_layout || !matches!(u.root, RootType::Constants | RootType::Table) {
            return Err(format!("{u:?} is of no root parameter of the layout"));
        }
    }
    let dwords = dwords.next_multiple_of(4);
    if u64::from(shader.user_data_dwords()) != dwords {
        return Err(format!(
            "a user-data buffer of {} dwords for {dwords} dwords read",
            shader.user_data_dwords()
        ));
    }
    Ok(())
}

// The cases.

/// Random instructions and random words as a program of any stage.
fn random_words(r: &mut Gen) -> bool {
    let mut words = if chance(r, 4) {
        (0..1 + r.below(32)).map(|_| r.next_u32()).collect()
    } else {
        noise(r, 24, 4)
    };
    if !chance(r, 8) {
        words.push(asm::S_ENDPGM);
    }
    let stage = pick(r, &STAGES);
    let native = program(r, stage, &words);
    run_program(r, &native)
}

/// A program of a stage the translator takes that starts as a compiled one
/// does: the root signature of the testkit's compute programs (four dwords
/// of root constants, then a descriptor table), the table's first
/// descriptor loaded into s[16..] and a root constant moved into v0. Then
/// noise and `s_endpgm`. Half the cases are tame ([`Gen`]): their noise is
/// instructions the first stage takes, their stage data and signatures
/// those of a plain program, and a pixel or vertex program ends with its
/// export. The other half is noise of any kind over random stage data.
fn with_prologue(r: &mut Gen) -> bool {
    r.tame = chance(r, 2);
    let stage = if r.tame {
        pick(r, &[HwStage::Cs, HwStage::Cs, HwStage::Ps, HwStage::Vs])
    } else {
        pick(r, &[HwStage::Cs, HwStage::Ps, HwStage::Vs, HwStage::Ms])
    };
    let dwords = pick(r, &[4u8, 8]);
    let table = stage.user_sgpr(4).expect("an SGPR");
    let mut words = asm::sop1("s_mov_b32", table + 1, asm::imm(0));
    let load = if dwords == 4 {
        "s_load_dwordx4"
    } else {
        "s_load_dwordx8"
    };
    words.extend(asm::smem(load, DESCRIPTOR, table, 0, asm::NULL));
    let constant = asm::sgpr(stage.user_sgpr(0).expect("an SGPR"));
    words.extend(asm::vop1("v_mov_b32", 0, constant));
    if r.tame {
        words.extend(tame_noise(r, 16));
        let done = asm::ExpFlags {
            done: true,
            vm: stage == HwStage::Ps,
            ..asm::ExpFlags::default()
        };
        match stage {
            HwStage::Ps => words.extend(asm::exp(asm::target::MRT0, 0xF, [0, 1, 2, 3], done)),
            HwStage::Vs => words.extend(asm::exp(asm::target::POS0, 0xF, [0, 1, 2, 3], done)),
            _ => {}
        }
    } else {
        words.extend(noise(r, 16, 16));
    }
    words.push(asm::S_ENDPGM);
    let param = |root, sgprs, user_data, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let kind = pick(
        r,
        &[
            ResourceType::Uav,
            ResourceType::Srv,
            ResourceType::Cbv,
            ResourceType::Sampler,
        ],
    );
    let mut io = stage_io(r, stage, words.len());
    let mut signatures = [signature(r), signature(r), signature(r)];
    if let StageIo::Cs(cs) = &mut io {
        // Five user SGPRs, and most of the time a group D3D12 takes.
        cs.rsrc2 = cs.rsrc2 & !0x3E | 5 << 1;
        if r.tame || !chance(r, 8) {
            cs.rsrc2 &= !(3 << 11);
            cs.threads = pick(r, &[[1, 1, 1], [8, 8, 1], [64, 1, 1], [32, 1, 1]]);
        }
    }
    if r.tame {
        let texcoord = Element::float("TEXCOORD", 0, 0, 0xF);
        (io, signatures) = match stage {
            // The thread id in v0 to v2, a granule of LDS.
            HwStage::Cs => (io, [vec![], vec![], vec![]]),
            HwStage::Ps => (
                StageIo::Ps {
                    inputs: vec![PsInput {
                        usage: Usage::Generic,
                        row: 0,
                        mask: 0xF,
                        attr: 0,
                        flat: false,
                        fp16: false,
                        pack2: false,
                        row2: 0,
                    }],
                    // Perspective barycentrics at the pixel centre.
                    input_addr: 1 << 1,
                    db_shader_control: 0,
                    patches: vec![],
                },
                [vec![texcoord], vec![testkit::SV_TARGET], vec![]],
            ),
            _ => (
                StageIo::Vs {
                    inputs: vec![],
                    outputs: vec![],
                },
                [vec![], vec![testkit::SV_POSITION], vec![]],
            ),
        };
        if let StageIo::Cs(cs) = &mut io {
            cs.rsrc2 = 5 << 1 | 2 << 11 | 1 << 15;
        }
    }
    let native = NativeProgram {
        stage,
        wave64: chance(r, 2),
        vgprs: 32,
        layout: vec![
            param(RootType::Constants, 4, 0, 0),
            param(RootType::Table, 1, 4, 1),
        ],
        used: vec![UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords,
            user_data: 4,
            slot: 1,
        }],
        io,
        code: asm::bytes(&words),
    };
    run_with(r, &native, &signatures)
}

/// Edits `bytes` as a damaged or hostile file would be: flipped bits,
/// replaced bytes, a cut, and 32- and 64-bit fields set to values at the
/// edges of what a size or an offset can be.
fn mutate(r: &mut Gen, bytes: &mut Vec<u8>) {
    for _ in 0..1 + r.below(3) {
        if bytes.is_empty() {
            return;
        }
        let len = bytes.len();
        let at = r.below(len as u32) as usize;
        match r.below(8) {
            0 => bytes[at] ^= 1 << r.below(8),
            1 => bytes[at] = byte(r),
            2 => bytes.truncate(at),
            3..=5 if len >= 4 => {
                let at = (at & !3).min(len - 4);
                let old = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
                let new = match r.below(8) {
                    0 => old.wrapping_add(1),
                    1 => old.wrapping_sub(1),
                    2 => old ^ 1 << r.below(32),
                    3 => len as u32,
                    4 => r.below(64),
                    5 => old.wrapping_mul(2),
                    _ => wild(r, 2),
                };
                bytes[at..at + 4].copy_from_slice(&new.to_le_bytes());
            }
            _ if len >= 8 => {
                let at = (at & !7).min(len - 8);
                let old = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
                let new = match r.below(8) {
                    0 => old.wrapping_add(8),
                    1 => old.wrapping_sub(8),
                    2 => len as u64,
                    3 => u64::MAX,
                    4 => u64::MAX - 7,
                    5 => 1 << pick(r, &[31u32, 32, 63]),
                    6 => u64::from(r.below(len as u32)),
                    _ => 0,
                };
                bytes[at..at + 8].copy_from_slice(&new.to_le_bytes());
            }
            _ => bytes[at] = !bytes[at],
        }
    }
}

/// A container of the testkit's builders: one of its named programs, or a
/// random program of any stage with random signatures.
fn base_container(r: &mut Gen) -> Vec<u8> {
    match r.below(12) {
        0 => testkit::vs_fullscreen_triangle(),
        1 => testkit::vs_position_buffer(),
        2 => testkit::ps_constant([1.0, 0.5, 0.25, 1.0]),
        3 => testkit::ps_root_constants(),
        4 => testkit::cs_store_root_constant(),
        5 => testkit::cs_store_words(&[r.next_u32(), r.next_u32()]),
        _ => {
            let mut words = noise(r, 12, 8);
            words.push(asm::S_ENDPGM);
            let stage = pick(r, &STAGES);
            let native = program(r, stage, &words);
            testkit::container(&native, &signature(r), &signature(r))
        }
    }
}

/// A container of [`base_container`], taken apart and put together again
/// with one or more of its layers edited by [`mutate`]: the unpacked
/// native blob (records, stage data, code and tables), the packed blob,
/// the `DXIL` part around it, the other parts, the whole file. The packed
/// stream is checksummed, so edits to the file alone rarely get past it:
/// the layers under it are edited before they are packed.
fn mutated_container(r: &mut Gen) -> bool {
    let base = base_container(r);
    let c = Container::parse(&base).expect("a testkit container");
    let mut parts: Vec<(FourCC, Vec<u8>)> = Vec::new();
    for part in &c.parts {
        let mut data = part.data.to_vec();
        if part.fourcc == FourCC::DXIL {
            // A stage-data number the parser refuses leaves the part as it is.
            if let Ok(programs) = native_programs(part.data)
                && !programs.is_empty()
            {
                let mut blob = testkit::native_blob(&programs);
                if !chance(r, 4) {
                    mutate(r, &mut blob);
                }
                let mut packed = testkit::xbch(&blob);
                if chance(r, 8) {
                    mutate(r, &mut packed);
                }
                data = testkit::part(&[(KIND_NATIVE, &packed)]);
            }
            if chance(r, 8) {
                mutate(r, &mut data);
            }
        } else if chance(r, 3) {
            mutate(r, &mut data);
        }
        parts.push((part.fourcc, data));
    }
    let parts: Vec<(FourCC, &[u8])> = parts.iter().map(|(f, d)| (*f, &d[..])).collect();
    let mut container = crate::dxbc::build(&parts);
    if chance(r, 6) {
        mutate(r, &mut container);
    }
    whole(&container)
}

/// One case of any of the three kinds.
fn any_case(r: &mut Gen) -> bool {
    match r.below(3) {
        0 => random_words(r),
        1 => with_prologue(r),
        _ => mutated_container(r),
    }
}

// The tests. Each has its own first seed, so that they do not run the same
// cases.

#[test]
fn random_words_never_panic_in_any_stage() {
    drive(0x5348_0501_0000_0000, HANG, random_words);
}

/// With the default cases, some of these programs translate and some are
/// refused: the noise after the prologue is not all stopped by the decoder.
#[test]
fn random_words_after_a_valid_prologue_never_panic() {
    let tally = drive(0x5348_0502_0000_0000, HANG, with_prologue);
    if tally.defaults {
        assert!(tally.translated > 0, "nothing translated: {tally:?}");
        assert!(tally.translated < tally.cases, "nothing refused: {tally:?}");
    }
}

/// With the default cases, some mutated containers still translate (an
/// edit that changes nothing the translator reads) and some are refused.
#[test]
fn mutated_containers_never_panic() {
    let tally = drive(0x5348_0503_0000_0000, HANG, mutated_container);
    if tally.defaults {
        assert!(tally.translated > 0, "nothing translated: {tally:?}");
        assert!(tally.translated < tally.cases, "nothing refused: {tally:?}");
    }
}

/// No input makes the translator loop without end: every case of every
/// kind finishes, and with the default cases some translate and some are
/// refused.
///
/// The limit per case is [`HANG`], as for the other tests. It was two
/// seconds of wall-clock time, measured across a channel while the rest
/// of the test binary (and, under Wine, device tests) runs in parallel: a
/// case takes milliseconds, but a worker thread the system did not
/// schedule for two seconds on a loaded or emulated machine failed the
/// test with nothing wrong in the translator. A short limit measures the
/// machine. The translator has no counter of work done that a test could
/// bound instead, so what this proves is the weaker thing that can be
/// proven reliably: no case runs away. An endless loop still fails, by
/// its seed, after [`HANG`]; a translation that is merely slow does not,
/// and no test here claims otherwise.
#[test]
fn every_case_finishes_within_its_time_budget() {
    let tally = drive(0x5348_0504_0000_0000, HANG, any_case);
    if tally.defaults {
        assert!(tally.translated > 0, "nothing translated: {tally:?}");
        assert!(tally.translated < tally.cases, "nothing refused: {tally:?}");
    }
}

/// The testkit's own programs translate to modules that hold together,
/// so the [`invariants`] refuse nothing a sound translation has.
#[test]
fn the_testkit_programs_hold_together() {
    for (name, container) in sound() {
        let shader = emit::translate(&container).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(invariants(&shader, None), Ok(()), "{name}");
        let c = Container::parse(&container).expect("a testkit container");
        let part = c.part(FourCC::DXIL).expect("a program");
        let programs = native_programs(part.data).expect("its programs");
        let native = programs.iter().find(|p| p.stage == shader.stage);
        assert!(native.is_some(), "{name}");
        assert_eq!(invariants(&shader, native), Ok(()), "{name}");
    }
}

/// The testkit's named programs.
fn sound() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("vs_fullscreen_triangle", testkit::vs_fullscreen_triangle()),
        ("vs_position_buffer", testkit::vs_position_buffer()),
        ("ps_constant", testkit::ps_constant([1.0, 0.5, 0.25, 1.0])),
        ("ps_root_constants", testkit::ps_root_constants()),
        ("cs_store_root_constant", testkit::cs_store_root_constant()),
        ("cs_store_words", testkit::cs_store_words(&[7, 9])),
    ]
}

/// Where the body of part `name` starts in an emitted container.
fn part_at(file: &[u8], name: &[u8; 4]) -> usize {
    let count = u32::from_le_bytes(file[0x1C..0x20].try_into().expect("4 bytes")) as usize;
    (0..count)
        .map(|i| {
            let at = 0x20 + 4 * i;
            u32::from_le_bytes(file[at..at + 4].try_into().expect("4 bytes")) as usize
        })
        .find(|&offset| &file[offset..offset + 4] == name)
        .expect("the part")
        + 8
}

/// The [`invariants`] catch what a wrapped or truncated number in the
/// translator would leave behind, by value and so in a build without
/// overflow checks too: each edit below is one such number in an
/// otherwise sound translation, and each is refused for its own reason.
/// (Edits inside the container are hashed again, as the emitter hashes
/// last: a wrong number is not given away by the digest.)
#[test]
fn the_invariants_refuse_numbers_that_do_not_match() {
    let pixel = emit::translate(&testkit::ps_root_constants()).expect("a pixel program");
    assert_eq!(invariants(&pixel, None), Ok(()));
    assert!(
        !pixel.user_data.is_empty(),
        "a program that reads user data"
    );
    let add = |file: &mut Vec<u8>, at: usize, n: u32| {
        let old = u32::from_le_bytes(file[at..at + 4].try_into().expect("4 bytes"));
        file[at..at + 4].copy_from_slice(&old.wrapping_add(n).to_le_bytes());
    };
    let refused = |what: &str, edit: &dyn Fn(&mut HostShader)| {
        let mut shader = pixel.clone();
        edit(&mut shader);
        if shader.dxil.len() >= 20 {
            let digest = emit::container_hash(&shader.dxil[20..]);
            shader.dxil[4..20].copy_from_slice(&digest);
        }
        let broken = invariants(&shader, None).expect_err(what);
        assert!(broken.contains(what), "{what:?} expected, got {broken:?}");
    };
    let at = |name: &[u8; 4]| part_at(&pixel.dxil, name);
    let (psv, program, isg, osg) = (at(b"PSV0"), at(b"DXIL"), at(b"ISG1"), at(b"OSG1"));
    // The container: a total, a part size and a part offset 4 off, and a
    // file cut short.
    refused("the container says", &|s| add(&mut s.dxil, 0x18, 4));
    refused("the container says", &|s| s.dxil.truncate(s.dxil.len() - 4));
    refused("not where the one before it ends", &|s| {
        add(&mut s.dxil, psv - 4, 4)
    });
    refused("runs past the file", &|s| add(&mut s.dxil, program - 4, 4));
    refused("not where the one before it ends", &|s| {
        add(&mut s.dxil, 0x20 + 4, 4)
    });
    // A size that wrapped to a small number, as 2^32 + 8 bytes would.
    refused("the parts end at", &|s| {
        let at = program - 4;
        s.dxil[at..at + 4].copy_from_slice(&8u32.to_le_bytes());
    });
    // The program: its size in dwords and its bitcode size.
    refused("the DXIL part says", &|s| add(&mut s.dxil, program + 4, 1));
    refused("bytes of bitcode", &|s| {
        add(&mut s.dxil, program + 20, u32::MAX - 3)
    });
    // The signatures: a row count, a register, a name outside the part.
    refused("ISG1: ", &|s| add(&mut s.dxil, isg, 1 << 27));
    refused("OSG1: register 32", &|s| add(&mut s.dxil, osg + 8 + 20, 32));
    refused("OSG1: a name at", &|s| {
        add(&mut s.dxil, osg + 8 + 4, 0x1000)
    });
    // PSV0: an element count cut to a byte, a binding count, a register.
    refused("PSV0 counts", &|s| s.dxil[psv + 4 + 28] ^= 1);
    refused("PSV0", &|s| add(&mut s.dxil, psv + 52, 1));
    refused("PSV0: a binding of several registers", &|s| {
        add(&mut s.dxil, psv + 60 + 8, 1)
    });
    refused("PSV0 binds", &|s| {
        add(&mut s.dxil, psv + 60 + 8, 1);
        add(&mut s.dxil, psv + 60 + 12, 1);
    });
    // What the host binds.
    refused("user-data dword", &|s| s.user_data.push(s.user_data[0]));
    refused("user-data dword 4294967295 is no SGPR", &|s| {
        s.user_data[0].index = u32::MAX
    });
    refused("PSV0 binds", &|s| s.user_data.clear());
    refused("PSV0 binds", &|s| s.root_cbvs.push(3));
    refused("name a slot twice", &|s| s.root_srvs.extend([2, 2]));
    refused("immediate samplers", &|s| s.samplers.push([0; 4]));
    refused("a translation of stage", &|s| s.stage = HwStage::Hs);

    // With the program at hand: user data of no root parameter.
    let container = testkit::ps_root_constants();
    let c = Container::parse(&container).expect("a testkit container");
    let programs = native_programs(c.part(FourCC::DXIL).expect("a program").data);
    let native = &programs.expect("its programs")[0];
    assert_eq!(invariants(&pixel, Some(native)), Ok(()));
    let mut other = pixel.clone();
    other.user_data[0].slot ^= 0x40;
    let broken = invariants(&other, Some(native)).expect_err("a slot of no parameter");
    assert!(broken.contains("of no root parameter"), "{broken}");
}

/// The driver reports a panicking case by its seed and how to run it
/// again, and a case that outlasts its budget likewise, instead of passing
/// or taking the process down.
#[test]
fn the_driver_names_the_seed_of_a_failing_case() {
    // The variables would change which seed fails; the messages are checked
    // for the default run only.
    if number("VICTORY_FUZZ_CASES").is_some() || number("VICTORY_FUZZ_SEED").is_some() {
        return;
    }
    let message = |default_seed: u64, budget: Duration, case: Case| {
        let failure = std::thread::Builder::new()
            .name("fuzz_tests::driver_check".into())
            .spawn(move || drive(default_seed, budget, case))
            .expect("a thread")
            .join()
            .expect_err("the driver fails");
        failure
            .downcast_ref::<String>()
            .expect("a formatted message")
            .clone()
    };
    // The third case (seed 0x102) panics.
    let panics: Case = |r| {
        let third = generator(0x102).next_u64();
        assert!(r.clone().next_u64() != third, "the case's own message");
        false
    };
    let text = message(0x100, HANG, panics);
    assert!(text.starts_with("seed 0x102 panicked: "), "{text}");
    assert!(text.contains("the case's own message"), "{text}");
    assert!(
        text.contains("VICTORY_FUZZ_SEED=0x102 VICTORY_FUZZ_CASES=1"),
        "{text}"
    );
    // The first case sleeps past its budget.
    let sleeps: Case = |_| {
        std::thread::sleep(Duration::from_millis(1500));
        true
    };
    let text = message(0x200, Duration::from_millis(50), sleeps);
    assert!(
        text.starts_with("seed 0x200 did not finish within 50ms"),
        "{text}"
    );
}

/// The generator of a seed is a fixed sequence, different from its
/// neighbours' from the first output on.
#[test]
fn neighbouring_seeds_give_unrelated_cases() {
    let first = |seed: u64| generator(seed).next_u64();
    assert_eq!(first(7), first(7));
    let firsts: std::collections::HashSet<u64> = (0..1000).map(first).collect();
    assert_eq!(firsts.len(), 1000);
    // Not the small differences consecutive xorshift states start with.
    assert!((first(0) ^ first(1)).count_ones() > 8);
}

/// An unused import of the error type would hide that the stages answer
/// with it: the pipeline's first stage does, for code that is no code.
#[test]
fn the_pipeline_answers_with_an_error_value() {
    let native = testkit::bare_program(HwStage::Cs, &[0xFF; 4]);
    let refused = ir::build(&native.code, native.wave64, native.vgprs).map(drop);
    assert!(
        matches!(refused, Err(ShaderError::Isa { .. })),
        "{refused:?}"
    );
}
