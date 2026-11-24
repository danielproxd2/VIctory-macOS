//! A GFX10 assembler for tests: one encoder per instruction encoding, taking
//! the opcode by AMD's name (looked up in the decoder's tables; an unknown
//! name panics) and the operand fields as [`Instruction::fields`] names
//! them. Synthetic test programs are written with it, so nothing in a test
//! needs words taken from a compiled shader.
//!
//! Bit layouts follow AMD's RDNA ISA reference, as the decoder's do. An
//! encoder checked only against our own decoder would prove nothing, so the
//! tests here also reproduce words that came from llvm-mc (a whole program
//! in `resource.rs`) and words older tests hand-encoded and ran on
//! hardware. Encoding classes without such an anchor are marked
//! *decoder-consistent only* at their encoder.
//!
//! Compiled for tests and with the `test-support` feature only.
//!
//! [`Instruction::fields`]: super::Instruction::fields

use super::{Encoding, opcodes as t};

/// `s_endpgm`.
pub const S_ENDPGM: u32 = 0xBF81_0000;
/// Scalar operand codes of the special registers.
pub const VCC_LO: u8 = 106;
pub const VCC_HI: u8 = 107;
pub const M0: u8 = 124;
pub const NULL: u8 = super::NULL;
pub const EXEC_LO: u8 = 126;
pub const EXEC_HI: u8 = 127;

/// A source operand: its 9-bit code, and the literal dword when the code is
/// 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Src {
    code: u16,
    literal: Option<u32>,
}

impl Src {
    /// A raw 9-bit operand code (0-127 scalar registers, 128-254 constants,
    /// 256-511 VGPRs). 255 is [`lit32`].
    pub const fn code(code: u16) -> Src {
        assert!(code < 512 && code != 255, "not an operand code");
        Src {
            code,
            literal: None,
        }
    }
}

impl From<u16> for Src {
    fn from(code: u16) -> Src {
        Src::code(code)
    }
}

/// `s<n>`, or a special scalar register by its code ([`VCC_LO`], [`EXEC_LO`]...).
pub const fn sgpr(n: u8) -> Src {
    assert!(n < 128, "not a scalar register");
    Src::code(n as u16)
}

/// `v<n>`.
pub const fn vgpr(n: u8) -> Src {
    Src::code(256 + n as u16)
}

/// An inline integer constant: 0 to 64, or -1 to -16.
pub const fn imm(value: i32) -> Src {
    match value {
        0..=64 => Src::code(128 + value as u16),
        -16..=-1 => Src::code((192 - value) as u16),
        _ => panic!("no inline constant for this value; use lit32"),
    }
}

/// An inline float constant: ±0.5, ±1.0, ±2.0 or ±4.0.
pub fn fconst(value: f32) -> Src {
    let code = match value {
        0.5 => 240,
        -0.5 => 241,
        1.0 => 242,
        -1.0 => 243,
        2.0 => 244,
        -2.0 => 245,
        4.0 => 246,
        -4.0 => 247,
        _ => panic!("no inline constant for {value}; use lit32"),
    };
    Src::code(code)
}

/// A 32-bit literal: the dword that follows the instruction.
pub const fn lit32(value: u32) -> Src {
    Src {
        code: 255,
        literal: Some(value),
    }
}

/// The code bytes of `words`.
pub fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The opcode of `name` in `encoding`. VOP3 also takes the names of VOPC,
/// VOP2 and VOP1 opcodes, at their bases in VOP3. Panics on an unknown name.
pub fn opcode(encoding: Encoding, name: &str) -> u16 {
    let find = |table: &[(u16, &str)]| table.iter().find(|e| e.1 == name).map(|e| e.0);
    let found = match encoding {
        Encoding::Sop2 => find(t::SOP2),
        Encoding::Sopk => find(t::SOPK),
        Encoding::Sop1 => find(t::SOP1),
        Encoding::Sopc => find(t::SOPC),
        Encoding::Sopp => find(t::SOPP),
        Encoding::Smem => find(t::SMEM),
        Encoding::Vop2 => find(t::VOP2),
        Encoding::Vop1 => find(t::VOP1),
        Encoding::Vopc => find(t::VOPC),
        Encoding::Vop3 => find(t::VOP3)
            .or_else(|| find(t::VOPC))
            .or_else(|| find(t::VOP2).map(|op| op + 0x100))
            .or_else(|| find(t::VOP1).map(|op| op + 0x180)),
        Encoding::Vop3p => find(t::VOP3P),
        Encoding::Vintrp => find(t::VINTRP),
        Encoding::Ds => find(t::DS),
        Encoding::Mubuf => find(t::MUBUF),
        Encoding::Mtbuf => find(t::MTBUF),
        Encoding::Mimg => find(t::MIMG),
        Encoding::Exp => (name == "exp").then_some(0),
        Encoding::Flat => find(t::FLAT),
        Encoding::Global => find(t::GLOBAL),
        Encoding::Scratch => find(t::SCRATCH),
    };
    found.unwrap_or_else(|| panic!("no {encoding:?} opcode named {name}"))
}

/// The name of `opcode` in `encoding`, for callers that hold a number.
/// Panics on an opcode the tables lack.
pub fn mnemonic(encoding: Encoding, opcode: u16) -> &'static str {
    super::name(encoding, opcode)
        .unwrap_or_else(|| panic!("no {encoding:?} opcode {opcode:#x} in the tables"))
}

/// An 8-bit scalar source field (no VGPRs).
fn scalar_field(src: Src) -> u32 {
    assert!(src.code < 256, "a VGPR in a scalar source field");
    src.code.into()
}

/// `words` plus the literal of `srcs`, if one of them is a literal.
fn with_literal(mut words: Vec<u32>, srcs: &[Src]) -> Vec<u32> {
    let mut literals = srcs.iter().filter_map(|s| s.literal);
    if let Some(first) = literals.next() {
        assert!(
            literals.all(|l| l == first),
            "an instruction has one literal"
        );
        words.push(first);
    }
    words
}

fn bit(flag: bool, at: u32) -> u32 {
    u32::from(flag) << at
}

/// SOP2 `name sdst, src0, src1`.
pub fn sop2(name: &str, sdst: u8, src0: Src, src1: Src) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Sop2, name));
    let w = 0x8000_0000
        | op << 23
        | u32::from(sdst & 0x7F) << 16
        | scalar_field(src1) << 8
        | scalar_field(src0);
    with_literal(vec![w], &[src0, src1])
}

/// SOP1 `name sdst, src0`.
pub fn sop1(name: &str, sdst: u8, src0: Src) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Sop1, name));
    let w = 0xBE80_0000 | u32::from(sdst & 0x7F) << 16 | op << 8 | scalar_field(src0);
    with_literal(vec![w], &[src0])
}

/// SOPC `name src0, src1` (the result is SCC).
/// Decoder-consistent only.
pub fn sopc(name: &str, src0: Src, src1: Src) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Sopc, name));
    let w = 0xBF00_0000 | op << 16 | scalar_field(src1) << 8 | scalar_field(src0);
    with_literal(vec![w], &[src0, src1])
}

/// SOPK `name sdst, imm16`. `s_setreg_imm32_b32` (two dwords) is not covered.
/// Decoder-consistent only.
pub fn sopk(name: &str, sdst: u8, imm16: u16) -> Vec<u32> {
    assert!(
        name != "s_setreg_imm32_b32",
        "the two-dword SOPK form is not covered"
    );
    let op = u32::from(opcode(Encoding::Sopk, name));
    vec![0xB000_0000 | op << 23 | u32::from(sdst & 0x7F) << 16 | u32::from(imm16)]
}

/// SOPP `name imm16`: a branch offset in dwords from the next instruction,
/// wait counters, a message...
pub fn sopp(name: &str, imm16: u16) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Sopp, name));
    vec![0xBF80_0000 | op << 16 | u32::from(imm16)]
}

/// SMEM `name s[sdata..], s[sbase..], offset` plus `soffset` (an SGPR, or
/// [`NULL`] for none). `sbase` is even; `offset` is a signed 21-bit byte
/// offset.
pub fn smem(name: &str, sdata: u8, sbase: u8, offset: i32, soffset: u8) -> Vec<u32> {
    assert!(sbase.is_multiple_of(2) && sbase < 128, "SMEM base register");
    assert!((-(1 << 20)..1 << 20).contains(&offset), "SMEM offset range");
    let op = u32::from(opcode(Encoding::Smem, name));
    vec![
        0x3D << 26 | op << 18 | u32::from(sdata & 0x7F) << 6 | u32::from(sbase >> 1),
        u32::from(soffset & 0x7F) << 25 | (offset as u32 & 0x1F_FFFF),
    ]
}

/// VOP1 `name vdst, src0` (`vdst` is an SGPR for `v_readfirstlane_b32`).
pub fn vop1(name: &str, vdst: u8, src0: Src) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vop1, name));
    let w = 0x7E00_0000 | u32::from(vdst) << 17 | op << 9 | u32::from(src0.code);
    with_literal(vec![w], &[src0])
}

/// VOP2 `name vdst, src0, v<vsrc1>`. For the forms with a constant operand
/// (`v_madmk_f32`, `v_madak_f32`...) use [`vop2k`].
pub fn vop2(name: &str, vdst: u8, src0: Src, vsrc1: u8) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vop2, name));
    let w = op << 25 | u32::from(vdst) << 17 | u32::from(vsrc1) << 9 | u32::from(src0.code);
    with_literal(vec![w], &[src0])
}

/// VOP2 with the constant `k` in the second dword (`v_madmk_f32 vdst, src0,
/// k, vsrc1`; `v_madak_f32 vdst, src0, vsrc1, k`). `src0` is no literal.
/// Decoder-consistent only.
pub fn vop2k(name: &str, vdst: u8, src0: Src, vsrc1: u8, k: u32) -> Vec<u32> {
    assert!(src0.literal.is_none(), "the second dword is the constant");
    let mut words = vop2(name, vdst, src0, vsrc1);
    words.push(k);
    words
}

/// VOPC `name src0, v<vsrc1>`: the result is VCC (EXEC for `v_cmpx_*`).
pub fn vopc(name: &str, src0: Src, vsrc1: u8) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vopc, name));
    let w = 0x7C00_0000 | op << 17 | u32::from(vsrc1) << 9 | u32::from(src0.code);
    with_literal(vec![w], &[src0])
}

/// VOP3 modifiers. Bit i of `abs`, `neg` applies to source i.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Vop3Mods {
    pub abs: u8,
    pub neg: u8,
    /// Bit 3 is the destination.
    pub opsel: u8,
    /// Output modifier: 1 ×2, 2 ×4, 3 ÷2.
    pub omod: u8,
    pub clamp: bool,
}

fn vop3_sources(srcs: &[Src]) -> u32 {
    assert!((1..=3).contains(&srcs.len()), "VOP3 takes 1 to 3 sources");
    srcs.iter()
        .enumerate()
        .fold(0, |w, (i, s)| w | u32::from(s.code) << (9 * i))
}

/// VOP3 `name dst, srcs...`: also the VOP3 form of a VOPC, VOP2 or VOP1
/// opcode (a compare's `dst` is the SGPR that takes the mask).
pub fn vop3(name: &str, dst: u8, srcs: &[Src], mods: Vop3Mods) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vop3, name));
    let w0 = 0x35 << 26
        | op << 16
        | bit(mods.clamp, 15)
        | u32::from(mods.opsel & 0xF) << 11
        | u32::from(mods.abs & 7) << 8
        | u32::from(dst);
    let w1 = vop3_sources(srcs) | u32::from(mods.omod & 3) << 27 | u32::from(mods.neg & 7) << 29;
    with_literal(vec![w0, w1], srcs)
}

/// VOP3 with a second, scalar result in bits 14:8 (`v_add_co_u32 vdst,
/// sdst, src0, src1`, `v_mad_u64_u32`...).
/// Decoder-consistent only.
pub fn vop3b(name: &str, vdst: u8, sdst: u8, srcs: &[Src]) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vop3, name));
    let w0 = 0x35 << 26 | op << 16 | u32::from(sdst & 0x7F) << 8 | u32::from(vdst);
    with_literal(vec![w0, vop3_sources(srcs)], srcs)
}

/// VOP3P modifiers. Bit i applies to source i.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Vop3pMods {
    /// Negate the low half.
    pub neg: u8,
    /// Negate the high half (`v_fma_mix*`: absolute value).
    pub neg_hi: u8,
    /// The half the low result reads.
    pub opsel: u8,
    /// The half the high result reads.
    pub opsel_hi: u8,
    pub clamp: bool,
}

/// VOP3P `name vdst, srcs...` (packed 16-bit pairs and `v_fma_mix*`).
pub fn vop3p(name: &str, vdst: u8, srcs: &[Src], mods: Vop3pMods) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vop3p, name));
    let w0 = 0x33 << 26
        | op << 16
        | bit(mods.clamp, 15)
        | u32::from(mods.opsel_hi >> 2 & 1) << 14
        | u32::from(mods.opsel & 7) << 11
        | u32::from(mods.neg_hi & 7) << 8
        | u32::from(vdst);
    let w1 =
        vop3_sources(srcs) | u32::from(mods.opsel_hi & 3) << 27 | u32::from(mods.neg & 7) << 29;
    with_literal(vec![w0, w1], srcs)
}

/// SDWA modifiers. Selects: 0-3 byte, 4-5 word, 6 dword (the default).
/// Bit i of `sext`, `neg`, `abs` applies to source i.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdwaMods {
    pub dst_sel: u8,
    /// 0 pad with zeros, 1 sign-extend, 2 preserve.
    pub dst_unused: u8,
    pub src_sel: [u8; 2],
    pub sext: u8,
    pub neg: u8,
    pub abs: u8,
    pub clamp: bool,
    pub omod: u8,
    /// The second source field names an SGPR or constant, not a VGPR.
    pub scalar_src1: bool,
    /// Compares: the SGPR that takes the mask instead of VCC.
    pub sdst: Option<u8>,
}

impl Default for SdwaMods {
    fn default() -> Self {
        SdwaMods {
            dst_sel: 6,
            dst_unused: 0,
            src_sel: [6, 6],
            sext: 0,
            neg: 0,
            abs: 0,
            clamp: false,
            omod: 0,
            scalar_src1: false,
            sdst: None,
        }
    }
}

/// Takes the first source out of a one-dword VOP1, VOP2 or VOPC
/// instruction and puts `marker` in its field.
fn extended(inst: &[u32], marker: u32) -> (u32, u32) {
    let &[w0] = inst else {
        panic!("SDWA and DPP extend a one-dword VOP1, VOP2 or VOPC instruction");
    };
    assert!(w0 >> 31 == 0, "SDWA and DPP extend VOP1, VOP2 or VOPC");
    (w0 & !0x1FF | marker, w0 & 0x1FF)
}

/// The SDWA form of `inst`, a one-dword [`vop1`], [`vop2`] or [`vopc`].
pub fn sdwa(inst: &[u32], mods: SdwaMods) -> Vec<u32> {
    let (w0, src0) = extended(inst, 0xF9);
    let compare = w0 >> 25 == 0x3E;
    let dst = match (compare, mods.sdst) {
        (true, Some(sdst)) => u32::from(sdst & 0x7F) << 8 | 1 << 15,
        (true, None) => 0,
        (false, None) => {
            u32::from(mods.dst_sel & 7) << 8
                | u32::from(mods.dst_unused & 3) << 11
                | bit(mods.clamp, 13)
                | u32::from(mods.omod & 3) << 14
        }
        (false, Some(_)) => panic!("only compares have a scalar SDWA destination"),
    };
    let ext = (src0 & 0xFF)
        | dst
        | u32::from(mods.src_sel[0] & 7) << 16
        | bit(mods.sext & 1 != 0, 19)
        | bit(mods.neg & 1 != 0, 20)
        | bit(mods.abs & 1 != 0, 21)
        | bit(src0 < 256, 23)
        | u32::from(mods.src_sel[1] & 7) << 24
        | bit(mods.sext & 2 != 0, 27)
        | bit(mods.neg & 2 != 0, 28)
        | bit(mods.abs & 2 != 0, 29)
        | bit(mods.scalar_src1, 31);
    vec![w0, ext]
}

/// DPP16 modifiers: `ctrl` is the 9-bit control (quad permutation, row
/// shifts...). Bit i of `neg`, `abs` applies to source i.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dpp16Mods {
    pub ctrl: u16,
    pub row_mask: u8,
    pub bank_mask: u8,
    pub bound_ctrl: bool,
    pub fi: bool,
    pub neg: u8,
    pub abs: u8,
}

impl Default for Dpp16Mods {
    fn default() -> Self {
        Dpp16Mods {
            ctrl: 0,
            row_mask: 0xF,
            bank_mask: 0xF,
            bound_ctrl: false,
            fi: false,
            neg: 0,
            abs: 0,
        }
    }
}

/// The DPP16 form of `inst`, a one-dword [`vop1`], [`vop2`] or [`vopc`]
/// whose first source is a VGPR.
pub fn dpp16(inst: &[u32], mods: Dpp16Mods) -> Vec<u32> {
    let (w0, src0) = extended(inst, 0xFA);
    assert!(src0 >= 256, "the DPP source is a VGPR");
    let ext = (src0 & 0xFF)
        | u32::from(mods.ctrl & 0x1FF) << 8
        | bit(mods.fi, 18)
        | bit(mods.bound_ctrl, 19)
        | bit(mods.neg & 1 != 0, 20)
        | bit(mods.abs & 1 != 0, 21)
        | bit(mods.neg & 2 != 0, 22)
        | bit(mods.abs & 2 != 0, 23)
        | u32::from(mods.bank_mask & 0xF) << 24
        | u32::from(mods.row_mask & 0xF) << 28;
    vec![w0, ext]
}

/// The DPP8 form of `inst`: lane i of each group of eight reads lane
/// `lanes[i]`. `fi` fetches from inactive lanes.
pub fn dpp8(inst: &[u32], lanes: [u8; 8], fi: bool) -> Vec<u32> {
    let (w0, src0) = extended(inst, if fi { 0xEA } else { 0xE9 });
    assert!(src0 >= 256, "the DPP source is a VGPR");
    let sel = lanes
        .iter()
        .enumerate()
        .fold(0, |s, (i, &l)| s | u32::from(l & 7) << (3 * i));
    vec![w0, (src0 & 0xFF) | sel << 8]
}

/// VINTRP `name vdst, vsrc, attr<attr>.<chan>`: `vsrc` is the i or j VGPR,
/// or for `v_interp_mov_f32` the vertex (0 p10, 1 p20, 2 p0).
pub fn vintrp(name: &str, vdst: u8, vsrc: u8, attr: u8, chan: u8) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Vintrp, name));
    vec![
        0x32 << 26
            | u32::from(vdst) << 18
            | op << 16
            | u32::from(attr & 0x3F) << 10
            | u32::from(chan & 3) << 8
            | u32::from(vsrc),
    ]
}

/// The fields of an LDS instruction; which of them an opcode reads is the
/// opcode's business.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ds {
    pub vdst: u8,
    pub addr: u8,
    pub data0: u8,
    pub data1: u8,
    pub offset0: u8,
    pub offset1: u8,
    pub gds: bool,
}

/// DS `name` with the fields of `f`.
pub fn ds(name: &str, f: Ds) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Ds, name));
    vec![
        0x36 << 26 | op << 18 | bit(f.gds, 17) | u32::from(f.offset1) << 8 | u32::from(f.offset0),
        u32::from(f.vdst) << 24
            | u32::from(f.data1) << 16
            | u32::from(f.data0) << 8
            | u32::from(f.addr),
    ]
}

/// The fields of a buffer instruction (MUBUF and MTBUF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buffer {
    pub vdata: u8,
    pub vaddr: u8,
    /// First SGPR of the descriptor (a multiple of 4).
    pub srsrc: u8,
    /// An SGPR or inline constant (the default is 0).
    pub soffset: Src,
    /// 12-bit byte offset.
    pub offset: u16,
    pub offen: bool,
    pub idxen: bool,
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
    /// MUBUF only.
    pub lds: bool,
    pub tfe: bool,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer {
            vdata: 0,
            vaddr: 0,
            srsrc: 0,
            soffset: imm(0),
            offset: 0,
            offen: false,
            idxen: false,
            glc: false,
            slc: false,
            dlc: false,
            lds: false,
            tfe: false,
        }
    }
}

fn descriptor_field(sgpr: u8) -> u32 {
    assert!(
        sgpr.is_multiple_of(4) && sgpr < 128,
        "descriptor SGPRs start at a multiple of 4"
    );
    u32::from(sgpr >> 2)
}

fn buffer_words(f: &Buffer) -> (u32, u32) {
    assert!(f.offset < 0x1000, "buffer offsets have 12 bits");
    (
        bit(f.dlc, 15) | bit(f.glc, 14) | bit(f.idxen, 13) | bit(f.offen, 12) | u32::from(f.offset),
        scalar_field(f.soffset) << 24
            | bit(f.tfe, 23)
            | bit(f.slc, 22)
            | descriptor_field(f.srsrc) << 16
            | u32::from(f.vdata) << 8
            | u32::from(f.vaddr),
    )
}

/// MUBUF `name` with the fields of `f`.
pub fn mubuf(name: &str, f: Buffer) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Mubuf, name));
    let (w0, w1) = buffer_words(&f);
    vec![
        0x38 << 26 | (op >> 7) << 25 | (op & 0x7F) << 18 | bit(f.lds, 16) | w0,
        w1,
    ]
}

/// MTBUF `name` with data format `format` and the fields of `f`.
/// Decoder-consistent only.
pub fn mtbuf(name: &str, format: u8, f: Buffer) -> Vec<u32> {
    assert!(!f.lds, "MTBUF has no LDS form");
    let op = u32::from(opcode(Encoding::Mtbuf, name));
    let (w0, w1) = buffer_words(&f);
    vec![
        0x3A << 26 | u32::from(format & 0x7F) << 19 | (op & 7) << 16 | w0,
        (op >> 3) << 21 | w1,
    ]
}

/// The fields of an image instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image<'a> {
    pub vdata: u8,
    /// One VGPR: the first of the address range. Several: one VGPR per
    /// address (the NSA form).
    pub addr: &'a [u8],
    /// First SGPR of the image descriptor (a multiple of 4).
    pub srsrc: u8,
    /// First SGPR of the sampler (a multiple of 4).
    pub ssamp: u8,
    pub dmask: u8,
    /// 0-7: 1D 2D 3D CUBE 1D_ARRAY 2D_ARRAY 2D_MSAA 2D_MSAA_ARRAY.
    pub dim: u8,
    pub unorm: bool,
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
    pub r128: bool,
    pub tfe: bool,
    pub lwe: bool,
    pub a16: bool,
    pub d16: bool,
}

impl Default for Image<'_> {
    fn default() -> Self {
        Image {
            vdata: 0,
            addr: &[0],
            srsrc: 0,
            ssamp: 0,
            dmask: 0xF,
            dim: 1,
            unorm: false,
            glc: false,
            slc: false,
            dlc: false,
            r128: false,
            tfe: false,
            lwe: false,
            a16: false,
            d16: false,
        }
    }
}

/// MIMG `name` with the fields of `f`.
pub fn mimg(name: &str, f: Image) -> Vec<u32> {
    let op = u32::from(opcode(Encoding::Mimg, name));
    let (&first, rest) = f.addr.split_first().expect("an image address");
    let extra = rest.len().div_ceil(4);
    assert!(extra <= 3, "at most 13 address registers");
    let mut words = vec![
        0x3C << 26
            | bit(f.slc, 25)
            | (op & 0x7F) << 18
            | bit(f.lwe, 17)
            | bit(f.tfe, 16)
            | bit(f.r128, 15)
            | bit(f.glc, 13)
            | bit(f.unorm, 12)
            | u32::from(f.dmask & 0xF) << 8
            | bit(f.dlc, 7)
            | u32::from(f.dim & 7) << 3
            | (extra as u32) << 1
            | op >> 7,
        bit(f.d16, 31)
            | bit(f.a16, 30)
            | descriptor_field(f.ssamp) << 21
            | descriptor_field(f.srsrc) << 16
            | u32::from(f.vdata) << 8
            | u32::from(first),
    ];
    for group in rest.chunks(4) {
        words.push(
            group
                .iter()
                .enumerate()
                .fold(0, |w, (i, &v)| w | u32::from(v) << (8 * i)),
        );
    }
    words
}

/// The fields of a FLAT, GLOBAL or SCRATCH instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flat {
    pub vdst: u8,
    pub addr: u8,
    pub data: u8,
    /// The scalar base, or [`NULL`] for none (the default).
    pub saddr: u8,
    /// FLAT: 11 bits, unsigned. GLOBAL and SCRATCH: 12 bits, signed.
    pub offset: i16,
    pub lds: bool,
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
}

impl Default for Flat {
    fn default() -> Self {
        Flat {
            vdst: 0,
            addr: 0,
            data: 0,
            saddr: NULL,
            offset: 0,
            lds: false,
            glc: false,
            slc: false,
            dlc: false,
        }
    }
}

/// `name` in segment `seg` ([`Encoding::Flat`], [`Encoding::Global`] or
/// [`Encoding::Scratch`]) with the fields of `f`. Only GLOBAL loads have an
/// llvm-mc anchor; the rest is decoder-consistent only.
pub fn flat(seg: Encoding, name: &str, f: Flat) -> Vec<u32> {
    let (segment, range) = match seg {
        Encoding::Flat => (0, 0..0x800),
        Encoding::Scratch => (1, -0x800..0x800),
        Encoding::Global => (2, -0x800..0x800),
        _ => panic!("{seg:?} is no FLAT segment"),
    };
    assert!(range.contains(&f.offset), "{seg:?} offset range");
    let op = u32::from(opcode(seg, name));
    vec![
        0x37 << 26
            | op << 18
            | bit(f.slc, 17)
            | bit(f.glc, 16)
            | segment << 14
            | bit(f.lds, 13)
            | bit(f.dlc, 12)
            | (f.offset as u32 & 0xFFF),
        u32::from(f.vdst) << 24
            | u32::from(f.saddr & 0x7F) << 16
            | u32::from(f.data) << 8
            | u32::from(f.addr),
    ]
}

/// Export targets.
pub mod target {
    /// `mrt0`; `mrt<n>` is `MRT0 + n`.
    pub const MRT0: u8 = 0;
    pub const MRTZ: u8 = 8;
    pub const NULL: u8 = 9;
    /// `pos0`; `pos<n>` is `POS0 + n`.
    pub const POS0: u8 = 12;
    pub const PRIM: u8 = 20;
    /// `param0`; `param<n>` is `PARAM0 + n`.
    pub const PARAM0: u8 = 32;
}

/// Export flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExpFlags {
    /// Two 16-bit values per source (sources 0 and 1 only).
    pub compr: bool,
    /// The program's last export.
    pub done: bool,
    /// Valid mask: EXEC decides which pixels are written.
    pub vm: bool,
}

/// EXP `exp target, v<srcs[0]>...`: channel i is exported when bit i of `en`
/// is set.
pub fn exp(target: u8, en: u8, srcs: [u8; 4], flags: ExpFlags) -> Vec<u32> {
    vec![
        0x3E << 26
            | bit(flags.vm, 12)
            | bit(flags.done, 11)
            | bit(flags.compr, 10)
            | u32::from(target & 0x3F) << 4
            | u32::from(en & 0xF),
        u32::from_le_bytes(srcs),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::{Cache, Dpp, Fields, Instruction, Operand, Sdwa, Valu, decode};

    /// The one instruction `words` encode.
    fn one(words: &[u32]) -> Instruction {
        let mut code = words.to_vec();
        code.push(S_ENDPGM);
        let mut insts = decode(&bytes(&code)).unwrap();
        assert_eq!(insts.len(), 2, "{words:08x?} is not one instruction");
        assert_eq!(insts[0].words, words, "instruction length");
        insts.swap_remove(0)
    }

    fn s(code: u8, dwords: u8) -> Operand {
        Operand::Scalar { code, dwords }
    }

    fn v(reg: u8, dwords: u8) -> Operand {
        Operand::Vector { reg, dwords }
    }

    fn valu(dst: Operand, src: &[Operand]) -> Valu {
        Valu {
            dst: Some(dst),
            src: src.to_vec(),
            ..Valu::default()
        }
    }

    /// Each encoder's words decode to the opcode named and to the operands
    /// and modifiers given, for both wave sizes (lane masks are one dword in
    /// wave32, two in wave64).
    #[test]
    fn every_encoder_round_trips_through_fields() {
        // (encoding, name, words, fields with lane masks of `lanes` dwords)
        type Case = (Encoding, &'static str, Vec<u32>, Box<dyn Fn(u8) -> Fields>);
        let sop = |dst: Option<Operand>,
                   src: Vec<Operand>,
                   imm: Option<u16>|
         -> Box<dyn Fn(u8) -> Fields> {
            Box::new(move |_| Fields::Sop {
                dst,
                src: src.clone(),
                imm,
            })
        };
        let fixed = |f: Fields| -> Box<dyn Fn(u8) -> Fields> { Box::new(move |_| f.clone()) };
        let buffer = Buffer {
            vdata: 4,
            vaddr: 2,
            srsrc: 8,
            soffset: sgpr(1),
            offset: 16,
            offen: true,
            idxen: true,
            glc: true,
            ..Buffer::default()
        };
        let buffer_fields = |data: Operand, format: Option<u8>, lds: bool| Fields::Buffer {
            data: Some(data),
            addr: v(2, 2),
            rsrc: s(8, 4),
            soffset: s(1, 1),
            offset: 16,
            offen: true,
            idxen: true,
            format,
            lds,
            tfe: false,
            cache: Cache {
                glc: true,
                slc: false,
                dlc: false,
            },
        };
        let image = |addr: Vec<Operand>, data: Operand, d16: bool, slc: bool| Fields::Image {
            data,
            addr,
            rsrc: s(16, 8),
            sampler: Some(s(4, 4)),
            dmask: 7,
            dim: 1,
            unorm: false,
            r128: false,
            a16: false,
            d16,
            tfe: false,
            lwe: false,
            cache: Cache {
                glc: false,
                slc,
                dlc: false,
            },
        };
        let flat_fields =
            |addr: Option<Operand>, saddr: Option<Operand>, offset: i16| Fields::Flat {
                dst: Some(v(4, 4)),
                addr,
                data: None,
                saddr,
                offset,
                lds: false,
                cache: Cache::default(),
            };
        let cases: Vec<Case> = vec![
            (
                Encoding::Sop2,
                "s_add_u32",
                sop2("s_add_u32", 3, sgpr(1), lit32(0x1234)),
                sop(Some(s(3, 1)), vec![s(1, 1), Operand::Literal(0x1234)], None),
            ),
            (
                Encoding::Sop2,
                "s_and_b64",
                sop2("s_and_b64", 2, sgpr(EXEC_LO), sgpr(2)),
                sop(Some(s(2, 2)), vec![s(EXEC_LO, 2), s(2, 2)], None),
            ),
            (
                Encoding::Sop1,
                "s_mov_b64",
                sop1("s_mov_b64", 4, sgpr(EXEC_LO)),
                sop(Some(s(4, 2)), vec![s(EXEC_LO, 2)], None),
            ),
            (
                Encoding::Sop1,
                "s_mov_b32",
                sop1("s_mov_b32", 9, imm(-16)),
                sop(Some(s(9, 1)), vec![Operand::Const(208)], None),
            ),
            (
                Encoding::Sopc,
                "s_cmp_eq_u32",
                sopc("s_cmp_eq_u32", sgpr(2), imm(5)),
                sop(None, vec![s(2, 1), Operand::Const(133)], None),
            ),
            (
                Encoding::Sopk,
                "s_movk_i32",
                sopk("s_movk_i32", 7, 0xFFFE),
                sop(Some(s(7, 1)), vec![], Some(0xFFFE)),
            ),
            (
                Encoding::Sopp,
                "s_waitcnt",
                sopp("s_waitcnt", 0x3F70),
                fixed(Fields::Sopp { imm: 0x3F70 }),
            ),
            (
                Encoding::Smem,
                "s_load_dwordx4",
                smem("s_load_dwordx4", 20, 2, 0x40, NULL),
                fixed(Fields::Smem {
                    data: Some(s(20, 4)),
                    base: s(2, 2),
                    offset: 0x40,
                    soffset: s(NULL, 1),
                    cache: Cache::default(),
                }),
            ),
            (
                Encoding::Smem,
                "s_buffer_load_dword",
                smem("s_buffer_load_dword", 5, 8, -8, 2),
                fixed(Fields::Smem {
                    data: Some(s(5, 1)),
                    base: s(8, 4),
                    offset: -8,
                    soffset: s(2, 1),
                    cache: Cache::default(),
                }),
            ),
            (
                Encoding::Vop1,
                "v_mov_b32",
                vop1("v_mov_b32", 3, fconst(1.0)),
                fixed(Fields::Valu(valu(v(3, 1), &[Operand::Const(242)]))),
            ),
            (
                Encoding::Vop1,
                "v_readfirstlane_b32",
                vop1("v_readfirstlane_b32", 12, vgpr(4)),
                fixed(Fields::Valu(valu(s(12, 1), &[v(4, 1)]))),
            ),
            (
                Encoding::Vop2,
                "v_mul_f32",
                vop2("v_mul_f32", 2, lit32(0x3F00_0001), 5),
                fixed(Fields::Valu(valu(
                    v(2, 1),
                    &[Operand::Literal(0x3F00_0001), v(5, 1)],
                ))),
            ),
            (
                Encoding::Vop2,
                "v_madak_f32",
                vop2k("v_madak_f32", 1, vgpr(2), 3, 0x4000_0000),
                fixed(Fields::Valu(valu(
                    v(1, 1),
                    &[v(2, 1), v(3, 1), Operand::Literal(0x4000_0000)],
                ))),
            ),
            (
                Encoding::Vop2,
                "v_madmk_f32",
                vop2k("v_madmk_f32", 1, sgpr(2), 3, 7),
                fixed(Fields::Valu(valu(
                    v(1, 1),
                    &[s(2, 1), Operand::Literal(7), v(3, 1)],
                ))),
            ),
            (
                Encoding::Vopc,
                "v_cmp_gt_f32",
                vopc("v_cmp_gt_f32", fconst(0.5), 3),
                Box::new(|lanes| {
                    Fields::Valu(valu(s(VCC_LO, lanes), &[Operand::Const(240), v(3, 1)]))
                }),
            ),
            (
                Encoding::Vopc,
                "v_cmpx_eq_u64",
                vopc("v_cmpx_eq_u64", sgpr(12), 4),
                Box::new(|lanes| Fields::Valu(valu(s(EXEC_LO, lanes), &[s(12, 2), v(4, 2)]))),
            ),
            (
                Encoding::Vop3,
                "v_fma_f32",
                vop3(
                    "v_fma_f32",
                    4,
                    &[vgpr(1), sgpr(2), lit32(9)],
                    Vop3Mods {
                        abs: 0b010,
                        neg: 0b101,
                        opsel: 0,
                        omod: 3,
                        clamp: true,
                    },
                ),
                fixed(Fields::Valu(Valu {
                    abs: 0b010,
                    neg: 0b101,
                    omod: 3,
                    clamp: true,
                    ..valu(v(4, 1), &[v(1, 1), s(2, 1), Operand::Literal(9)])
                })),
            ),
            (
                Encoding::Vop3,
                "v_cmp_eq_u64",
                vop3(
                    "v_cmp_eq_u64",
                    21,
                    &[sgpr(12), vgpr(4)],
                    Vop3Mods::default(),
                ),
                Box::new(|lanes| Fields::Valu(valu(s(21, lanes), &[s(12, 2), v(4, 2)]))),
            ),
            (
                Encoding::Vop3,
                "v_cndmask_b32",
                vop3(
                    "v_cndmask_b32",
                    0,
                    &[imm(0), imm(1), sgpr(20)],
                    Vop3Mods::default(),
                ),
                Box::new(|lanes| {
                    Fields::Valu(valu(
                        v(0, 1),
                        &[Operand::Const(128), Operand::Const(129), s(20, lanes)],
                    ))
                }),
            ),
            (
                Encoding::Vop3,
                "v_add_co_u32",
                vop3b("v_add_co_u32", 1, 6, &[vgpr(2), vgpr(3)]),
                Box::new(|lanes| {
                    Fields::Valu(Valu {
                        carry: Some(s(6, lanes)),
                        ..valu(v(1, 1), &[v(2, 1), v(3, 1)])
                    })
                }),
            ),
            (
                Encoding::Vop3p,
                "v_pk_add_f16",
                vop3p(
                    "v_pk_add_f16",
                    0,
                    &[vgpr(1), vgpr(2)],
                    Vop3pMods {
                        neg: 0b01,
                        neg_hi: 0b10,
                        opsel: 0b10,
                        opsel_hi: 0b01,
                        clamp: true,
                    },
                ),
                fixed(Fields::Valu(Valu {
                    neg: 0b01,
                    neg_hi: 0b10,
                    opsel: 0b10,
                    opsel_hi: 0b01,
                    clamp: true,
                    ..valu(v(0, 1), &[v(1, 1), v(2, 1)])
                })),
            ),
            (
                Encoding::Vop3p,
                "v_fma_mix_f32",
                vop3p(
                    "v_fma_mix_f32",
                    1,
                    &[vgpr(2), sgpr(3), vgpr(4)],
                    Vop3pMods {
                        opsel: 0b100,
                        opsel_hi: 0b110,
                        ..Vop3pMods::default()
                    },
                ),
                fixed(Fields::Valu(Valu {
                    opsel: 0b100,
                    opsel_hi: 0b110,
                    ..valu(v(1, 1), &[v(2, 1), s(3, 1), v(4, 1)])
                })),
            ),
            (
                Encoding::Vop2,
                "v_add_f32",
                sdwa(
                    &vop2("v_add_f32", 1, sgpr(2), 3),
                    SdwaMods {
                        dst_sel: 4,
                        dst_unused: 2,
                        src_sel: [1, 5],
                        sext: 0b10,
                        neg: 0b01,
                        abs: 0b10,
                        clamp: true,
                        omod: 2,
                        scalar_src1: true,
                        sdst: None,
                    },
                ),
                fixed(Fields::Valu(Valu {
                    neg: 0b01,
                    abs: 0b10,
                    clamp: true,
                    omod: 2,
                    sdwa: Some(Sdwa {
                        dst_sel: 4,
                        dst_unused: 2,
                        src_sel: [1, 5],
                        sext: 0b10,
                    }),
                    ..valu(v(1, 1), &[s(2, 1), s(3, 1)])
                })),
            ),
            (
                Encoding::Vopc,
                "v_cmp_lt_u32",
                sdwa(
                    &vopc("v_cmp_lt_u32", vgpr(1), 2),
                    SdwaMods {
                        src_sel: [0, 6],
                        sdst: Some(9),
                        ..SdwaMods::default()
                    },
                ),
                Box::new(|lanes| {
                    Fields::Valu(Valu {
                        sdwa: Some(Sdwa {
                            dst_sel: 6,
                            dst_unused: 0,
                            src_sel: [0, 6],
                            sext: 0,
                        }),
                        ..valu(s(9, lanes), &[v(1, 1), v(2, 1)])
                    })
                }),
            ),
            (
                Encoding::Vop2,
                "v_add_f32",
                dpp16(
                    &vop2("v_add_f32", 5, vgpr(6), 7),
                    Dpp16Mods {
                        ctrl: 0x1B,
                        row_mask: 0xA,
                        bank_mask: 0x5,
                        bound_ctrl: true,
                        fi: true,
                        neg: 0b10,
                        abs: 0b01,
                    },
                ),
                fixed(Fields::Valu(Valu {
                    neg: 0b10,
                    abs: 0b01,
                    dpp: Some(Dpp::Row {
                        ctrl: 0x1B,
                        row_mask: 0xA,
                        bank_mask: 0x5,
                        bound_ctrl: true,
                        fi: true,
                    }),
                    ..valu(v(5, 1), &[v(6, 1), v(7, 1)])
                })),
            ),
            (
                Encoding::Vop1,
                "v_mov_b32",
                dpp8(
                    &vop1("v_mov_b32", 3, vgpr(1)),
                    [1, 0, 3, 2, 5, 4, 7, 6],
                    true,
                ),
                fixed(Fields::Valu(Valu {
                    dpp: Some(Dpp::Lanes {
                        sel: 1 | 3 << 6 | 2 << 9 | 5 << 12 | 4 << 15 | 7 << 18 | 6 << 21,
                        fi: true,
                    }),
                    ..valu(v(3, 1), &[v(1, 1)])
                })),
            ),
            (
                Encoding::Vintrp,
                "v_interp_p2_f32",
                vintrp("v_interp_p2_f32", 2, 1, 3, 2),
                fixed(Fields::Vintrp {
                    dst: v(2, 1),
                    src: v(1, 1),
                    attr: Operand::Attr {
                        index: 3,
                        chan: 2,
                        high: false,
                    },
                }),
            ),
            (
                Encoding::Ds,
                "ds_read2_b32",
                ds(
                    "ds_read2_b32",
                    Ds {
                        vdst: 6,
                        addr: 1,
                        offset0: 2,
                        offset1: 3,
                        ..Ds::default()
                    },
                ),
                fixed(Fields::Ds {
                    dst: Some(v(6, 2)),
                    addr: Some(v(1, 1)),
                    data: vec![],
                    offset0: 2,
                    offset1: 3,
                    gds: false,
                }),
            ),
            (
                Encoding::Ds,
                "ds_write2_b32",
                ds(
                    "ds_write2_b32",
                    Ds {
                        addr: 1,
                        data0: 8,
                        data1: 9,
                        offset1: 1,
                        gds: true,
                        ..Ds::default()
                    },
                ),
                fixed(Fields::Ds {
                    dst: None,
                    addr: Some(v(1, 1)),
                    data: vec![v(8, 1), v(9, 1)],
                    offset0: 0,
                    offset1: 1,
                    gds: true,
                }),
            ),
            (
                Encoding::Mubuf,
                "buffer_load_dwordx2",
                mubuf("buffer_load_dwordx2", buffer),
                fixed(buffer_fields(v(4, 2), None, false)),
            ),
            (
                // An opcode above 0x7F: its high bit is bit 25.
                Encoding::Mubuf,
                "buffer_load_format_d16_x",
                mubuf(
                    "buffer_load_format_d16_x",
                    Buffer {
                        lds: true,
                        ..buffer
                    },
                ),
                fixed(buffer_fields(v(4, 1), None, true)),
            ),
            (
                Encoding::Mtbuf,
                "tbuffer_load_format_xyzw",
                mtbuf("tbuffer_load_format_xyzw", 0x4A, buffer),
                fixed(buffer_fields(v(4, 4), Some(0x4A), false)),
            ),
            (
                // An opcode above 7: its high bit is bit 21 of the second dword.
                Encoding::Mtbuf,
                "tbuffer_load_format_d16_xy",
                mtbuf("tbuffer_load_format_d16_xy", 1, buffer),
                fixed(buffer_fields(v(4, 1), Some(1), false)),
            ),
            (
                Encoding::Mimg,
                "image_sample_lz",
                mimg(
                    "image_sample_lz",
                    Image {
                        vdata: 8,
                        addr: &[1],
                        srsrc: 16,
                        ssamp: 4,
                        dmask: 7,
                        slc: true,
                        ..Image::default()
                    },
                ),
                fixed(image(vec![v(1, 2)], v(8, 3), false, true)),
            ),
            (
                Encoding::Mimg,
                "image_sample_lz",
                mimg(
                    "image_sample_lz",
                    Image {
                        vdata: 8,
                        addr: &[1, 5],
                        srsrc: 16,
                        ssamp: 4,
                        dmask: 7,
                        d16: true,
                        ..Image::default()
                    },
                ),
                fixed(image(vec![v(1, 1), v(5, 1)], v(8, 2), true, false)),
            ),
            (
                Encoding::Global,
                "global_load_dwordx4",
                flat(
                    Encoding::Global,
                    "global_load_dwordx4",
                    Flat {
                        vdst: 4,
                        addr: 1,
                        saddr: 4,
                        offset: -16,
                        ..Flat::default()
                    },
                ),
                fixed(flat_fields(Some(v(1, 1)), Some(s(4, 2)), -16)),
            ),
            (
                Encoding::Flat,
                "flat_load_dwordx4",
                flat(
                    Encoding::Flat,
                    "flat_load_dwordx4",
                    Flat {
                        vdst: 4,
                        addr: 2,
                        offset: 0x7FF,
                        ..Flat::default()
                    },
                ),
                fixed(flat_fields(Some(v(2, 2)), None, 0x7FF)),
            ),
            (
                Encoding::Scratch,
                "scratch_load_dwordx4",
                flat(
                    Encoding::Scratch,
                    "scratch_load_dwordx4",
                    Flat {
                        vdst: 4,
                        saddr: 3,
                        offset: 8,
                        ..Flat::default()
                    },
                ),
                fixed(flat_fields(None, Some(s(3, 1)), 8)),
            ),
            (
                Encoding::Exp,
                "exp",
                exp(
                    target::PARAM0 + 5,
                    0b1011,
                    [1, 2, 3, 4],
                    ExpFlags {
                        done: true,
                        ..ExpFlags::default()
                    },
                ),
                fixed(Fields::Exp {
                    target: 37,
                    src: [Some(v(1, 1)), Some(v(2, 1)), None, Some(v(4, 1))],
                    compr: false,
                    done: true,
                    vm: false,
                }),
            ),
            (
                Encoding::Exp,
                "exp",
                exp(
                    target::MRT0,
                    0xF,
                    [2, 3, 0, 0],
                    ExpFlags {
                        compr: true,
                        done: true,
                        vm: true,
                    },
                ),
                fixed(Fields::Exp {
                    target: 0,
                    src: [Some(v(2, 1)), Some(v(2, 1)), Some(v(3, 1)), Some(v(3, 1))],
                    compr: true,
                    done: true,
                    vm: true,
                }),
            ),
        ];
        for (encoding, name, words, fields) in &cases {
            let inst = one(words);
            assert_eq!(inst.encoding, *encoding, "{name}");
            assert_eq!(inst.name(), Some(*name));
            for (wave64, lanes) in [(false, 1), (true, 2)] {
                assert_eq!(
                    inst.fields(wave64),
                    fields(lanes),
                    "{name} {words:08x?} wave64 {wave64}"
                );
            }
        }
        // Every encoding the decoder knows has a case.
        for encoding in [
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
        ] {
            assert!(cases.iter().any(|c| c.0 == encoding), "{encoding:?}");
        }
    }

    /// One line of SP3 text per encoding class with modifiers, as the
    /// printer in `sp3.rs` writes what the encoder was given.
    #[test]
    fn encoders_print_the_expected_sp3_text() {
        let text = |words: Vec<u32>| one(&words).text(true);
        assert_eq!(
            text(sdwa(
                &vop1("v_cvt_f32_f16", 1, vgpr(2)),
                SdwaMods {
                    src_sel: [5, 6],
                    ..SdwaMods::default()
                }
            )),
            "v_cvt_f32_f16 v1, v2 src0_sel:WORD_1"
        );
        assert_eq!(
            text(dpp16(
                &vop1("v_mov_b32", 5, vgpr(6)),
                Dpp16Mods {
                    ctrl: 0xB1,
                    bound_ctrl: true,
                    ..Dpp16Mods::default()
                }
            )),
            "v_mov_b32 v5, v6 quad_perm:[1,0,3,2] bound_ctrl:1"
        );
        assert_eq!(
            text(dpp8(
                &vop1("v_mov_b32", 3, vgpr(1)),
                [7, 6, 5, 4, 3, 2, 1, 0],
                false
            )),
            "v_mov_b32 v3, v1 dpp8:[7,6,5,4,3,2,1,0]"
        );
        assert_eq!(
            text(vop3p(
                "v_fma_mix_f32",
                1,
                &[vgpr(2), sgpr(3), vgpr(4)],
                Vop3pMods {
                    neg: 0b001,
                    neg_hi: 0b100,
                    opsel: 0b001,
                    opsel_hi: 0b001,
                    clamp: false,
                }
            )),
            "v_fma_mix_f32 v1, -sel_hi(v2), s3, abs(v4)"
        );
        assert_eq!(
            text(mimg(
                "image_sample_lz",
                Image {
                    vdata: 8,
                    addr: &[1, 5],
                    srsrc: 16,
                    ssamp: 4,
                    dmask: 7,
                    d16: true,
                    ..Image::default()
                }
            )),
            "image_sample_lz v[8:9], [v1,v5], s[16:23], s[4:7] dmask:0x7 dim:SQ_RSRC_IMG_2D d16"
        );
        assert_eq!(
            text(mtbuf(
                "tbuffer_load_format_xyzw",
                10,
                Buffer {
                    vdata: 4,
                    vaddr: 1,
                    srsrc: 8,
                    idxen: true,
                    offset: 12,
                    ..Buffer::default()
                }
            )),
            MTBUF_TEXT
        );
        assert_eq!(
            text(flat(
                Encoding::Scratch,
                "scratch_store_dword",
                Flat {
                    data: 2,
                    saddr: 32,
                    offset: 8,
                    ..Flat::default()
                }
            )),
            SCRATCH_TEXT
        );
        assert_eq!(
            text(exp(
                target::MRT0,
                0xF,
                [2, 3, 0, 0],
                ExpFlags {
                    compr: true,
                    done: true,
                    vm: true
                }
            )),
            "exp mrt0, v2, v2, v3, v3 done compr vm"
        );
    }

    const MTBUF_TEXT: &str = "tbuffer_load_format_xyzw v[4:7], v1, s[8:11], 0 idxen offset:12 format:[BUF_FMT_16_SSCALED]";
    const SCRATCH_TEXT: &str = "scratch_store_dword off, v2, s32 inst_offset:8";

    /// The anchor against circularity: words that did not come from this
    /// assembler. A whole program assembled with llvm-mc for gfx1030, and
    /// the instructions older tests hand-encoded (their comments give the
    /// text; several of them run on hardware in the device tests).
    #[test]
    fn encoders_reproduce_the_words_older_tests_hand_encoded() {
        let none = Vop3Mods::default();
        // `resource.rs`: WATERFALL_FETCHED_WAVE32, from llvm-mc.
        let mut w: Vec<Vec<u32>> = vec![
            sop1("s_mov_b32", 4, sgpr(0)),
            sop1("s_mov_b32", 5, imm(0)),
            sop1("s_mov_b32", 6, sgpr(1)),
            sop1("s_mov_b32", 7, imm(0)),
            vop2("v_lshlrev_b32", 1, imm(5), 0),
        ];
        for (vdst, offset) in [(4, 0), (8, 16)] {
            w.push(flat(
                Encoding::Global,
                "global_load_dwordx4",
                Flat {
                    vdst,
                    addr: 1,
                    saddr: 4,
                    offset,
                    ..Flat::default()
                },
            ));
        }
        w.extend([
            smem("s_load_dwordx4", 8, 6, 0, NULL),
            vop1("v_cvt_f32_u32", 2, vgpr(0)),
            vop1("v_mov_b32", 3, vgpr(2)),
            sopp("s_waitcnt", 0x0070),
            sop1("s_mov_b32", 20, sgpr(EXEC_LO)),
        ]);
        for i in 0..8 {
            w.push(vop1("v_readfirstlane_b32", 12 + i, vgpr(4 + i)));
        }
        w.push(vop3("v_cmp_eq_u64", 21, &[sgpr(12), vgpr(4)], none));
        for i in 1..4 {
            w.extend([
                vop3(
                    "v_cmp_eq_u64",
                    22,
                    &[sgpr(12 + 2 * i), vgpr(4 + 2 * i)],
                    none,
                ),
                sop2("s_and_b32", 21, sgpr(21), sgpr(22)),
            ]);
        }
        w.extend([
            sop1("s_and_saveexec_b32", 21, sgpr(21)),
            sopp("s_nop", 0),
            mimg(
                "image_sample_lz",
                Image {
                    vdata: 12,
                    addr: &[2],
                    srsrc: 12,
                    ssamp: 8,
                    ..Image::default()
                },
            ),
            sopp("s_waitcnt", 0x3F70),
            sop2("s_xor_b32", EXEC_LO, sgpr(EXEC_LO), sgpr(21)),
            sopp("s_cbranch_execnz", -26i16 as u16),
            sop1("s_mov_b32", EXEC_LO, sgpr(20)),
            ds(
                "ds_write_b32",
                Ds {
                    data0: 12,
                    ..Ds::default()
                },
            ),
            vec![S_ENDPGM],
        ]);
        assert_eq!(
            w.concat(),
            crate::resource::tests::WATERFALL_FETCHED_WAVE32,
            "the llvm-mc program"
        );

        // Hand-encoded words of older tests, with the text their comments give.
        let mrt = ExpFlags {
            done: true,
            vm: true,
            ..ExpFlags::default()
        };
        let hand: Vec<(Vec<u32>, &[u32], &str)> = vec![
            // emit/mod.rs
            (
                vop1("v_mov_b32", 0, fconst(1.0)),
                &[0x7E00_02F2],
                "v_mov_b32 v0, 1.0",
            ),
            (
                vop1("v_mov_b32", 1, imm(5)),
                &[0x7E02_0285],
                "v_mov_b32 v1, 5",
            ),
            (
                exp(target::MRTZ, 0b0011, [0, 1, 0, 0], mrt),
                &[0xF800_1883, 0x0000_0100],
                "exp mrtz v0, v1, off, off done vm",
            ),
            (
                sop1("s_mov_b32", EXEC_HI, imm(0)),
                &[0xBEFF_0380],
                "s_mov_b32 exec_hi, 0",
            ),
            (
                vintrp("v_interp_p1_f32", 2, 0, 0, 0),
                &[0xC808_0000],
                "v_interp_p1_f32 v2, v0, attr0.x",
            ),
            (
                vintrp("v_interp_p2_f32", 2, 1, 0, 0),
                &[0xC809_0001],
                "v_interp_p2_f32 v2, v1, attr0.x",
            ),
            (
                vintrp("v_interp_mov_f32", 3, 2, 0, 2),
                &[0xC80E_0202],
                "v_interp_mov_f32 v3, p0, attr0.z",
            ),
            (
                exp(target::MRT0, 0xF, [2, 2, 2, 2], mrt),
                &[0xF800_180F, 0x0202_0202],
                "exp mrt0, v2, v2, v2, v2 done vm",
            ),
            // emit/valu_tests.rs (run on hardware and WARP)
            (
                smem("s_load_dwordx4", 8, 4, 0, NULL),
                &[0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, 125 << 25],
                "s_load_dwordx4 s[8:11], s[4:5], 0",
            ),
            (
                mubuf(
                    "buffer_store_dword",
                    Buffer {
                        srsrc: 8,
                        ..Buffer::default()
                    },
                ),
                &[0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16],
                "buffer_store_dword v0, off, s[8:11], 0",
            ),
            (
                vop3p(
                    "v_pk_add_f16",
                    0,
                    &[Src::code(248), vgpr(1), imm(0)],
                    Vop3pMods {
                        opsel_hi: 0b111,
                        ..Vop3pMods::default()
                    },
                ),
                &[0xCC0F_4000, 0x1A02_02F8],
                "v_pk_add_f16 v0, 0x3118, v1",
            ),
            (
                vop3p(
                    "v_pk_add_u16",
                    0,
                    &[Src::code(248), vgpr(1), imm(0)],
                    Vop3pMods {
                        opsel_hi: 0b111,
                        ..Vop3pMods::default()
                    },
                ),
                &[0xCC0A_4000, 0x1A02_02F8],
                "v_pk_add_u16 v0, 0x3e22f983, v1",
            ),
            // resource.rs
            (
                sop2("s_or_b32", 1, sgpr(11), lit32(0x10_0000)),
                &[0x8801_FF0B, 0x0010_0000],
                "s_or_b32 s1, s11, 0x100000",
            ),
            (
                sop1("s_mov_b32", 2, imm(-1)),
                &[0xBE82_03C1],
                "s_mov_b32 s2, -1",
            ),
            (
                smem("s_buffer_load_dwordx16", 0, 0, 0, NULL),
                &[0xF430_0000, 0xFA00_0000],
                "s_buffer_load_dwordx16 s[0:15], s[0:3], null",
            ),
            (
                sop2("s_lshl_b32", 8, sgpr(14), imm(5)),
                &[0x8F08_850E],
                "s_lshl_b32 s8, s14, 5",
            ),
            (
                smem("s_load_dwordx4", 8, 18, 0, 8),
                &[0xF408_0209, 0x1000_0000],
                "s_load_dwordx4 s[8:11], s[18:19], s8",
            ),
            (
                mubuf(
                    "buffer_load_format_x",
                    Buffer {
                        vdata: 4,
                        srsrc: 8,
                        idxen: true,
                        ..Buffer::default()
                    },
                ),
                &[0xE000_2000, 0x8002_0400],
                "buffer_load_format_x v4, v0, s[8:11], 0 idxen",
            ),
            (
                mimg(
                    "image_sample",
                    Image {
                        addr: &[2],
                        ssamp: 8,
                        ..Image::default()
                    },
                ),
                &[0xF080_0F08, 0x0040_0002],
                "image_sample v[0:3], v[2:3], s[0:7], s[8:11]",
            ),
            (
                mubuf(
                    "buffer_store_dword",
                    Buffer {
                        vdata: 12,
                        vaddr: 16,
                        srsrc: 40,
                        offen: true,
                        ..Buffer::default()
                    },
                ),
                &[0xE070_1000, 0x800A_0C10],
                "buffer_store_dword v12, v16, s[40:43], 0 offen",
            ),
            (
                vopc("v_cmp_gt_u32", imm(16), 0),
                &[0x7D88_0090],
                "v_cmp_gt_u32 vcc_lo, 16, v0",
            ),
            (
                vop2("v_cndmask_b32", 4, sgpr(32), 4),
                &[0x0208_0820],
                "v_cndmask_b32 v4, s32, v4, vcc_lo",
            ),
            (
                sop1("s_andn2_wrexec_b32", 21, sgpr(21)),
                &[0xBE95_4715],
                "s_andn2_wrexec_b32 s21, s21",
            ),
            // gfx10/mod.rs
            (
                sop1("s_getpc_b64", 8, Src::code(0)),
                &[0xBE88_1F00],
                "s_getpc_b64 s[8:9]",
            ),
            (
                sop2("s_addc_u32", 9, sgpr(9), imm(-1)),
                &[0x8209_C109],
                "s_addc_u32 s9, s9, -1",
            ),
            (
                sop1("s_setpc_b64", 0, sgpr(8)),
                &[0xBE80_2008],
                "s_setpc_b64 s[8:9]",
            ),
            (
                vop3(
                    "v_fma_f32",
                    0,
                    &[sgpr(0), vgpr(0), lit32(0x4000_0000)],
                    none,
                ),
                &[0xD54B_0000, 0x03FE_0000, 0x4000_0000],
                "v_fma_f32 with a literal src2",
            ),
            // gfx10/sp3.rs
            (
                vop3(
                    "v_fma_f32",
                    4,
                    &[vgpr(1), sgpr(2), fconst(0.5)],
                    Vop3Mods {
                        neg: 0b001,
                        omod: 1,
                        clamp: true,
                        ..none
                    },
                ),
                &[0xD54B_8004, 0x2BC0_0501],
                "v_fma_f32 v4, -v1, s2, 0.5 mul:2 clamp",
            ),
            (
                vop3(
                    "v_cmp_lt_f32",
                    6,
                    &[vgpr(1), fconst(1.0)],
                    Vop3Mods { abs: 0b001, ..none },
                ),
                &[0xD401_0106, 0x0001_E501],
                "v_cmp_lt_f32 s6, abs(v1), 1.0",
            ),
            (
                mubuf(
                    "buffer_load_dwordx2",
                    Buffer {
                        vdata: 4,
                        vaddr: 2,
                        srsrc: 8,
                        soffset: sgpr(1),
                        offset: 16,
                        offen: true,
                        idxen: true,
                        glc: true,
                        ..Buffer::default()
                    },
                ),
                &[0xE034_7010, 0x0102_0402],
                "buffer_load_dwordx2 v[4:5], v[2:3], s[8:11], s1 idxen offen offset:16 glc",
            ),
            (
                ds(
                    "ds_read2_b32",
                    Ds {
                        vdst: 6,
                        addr: 1,
                        offset0: 2,
                        offset1: 3,
                        ..Ds::default()
                    },
                ),
                &[0xD8DC_0302, 0x0600_0001],
                "ds_read2_b32 v[6:7], v1 offset0:2 offset1:3",
            ),
            (
                exp(target::POS0 + 4, 1, [0; 4], ExpFlags::default()),
                &[0xF800_0101, 0],
                "exp pos4, v0, off, off, off",
            ),
            (
                smem("s_buffer_load_dword", 5, 8, 0, 2),
                &[0xF420_0144, 0x0400_0000],
                "s_buffer_load_dword s5, s[8:11], s2",
            ),
        ];
        for (ours, theirs, text) in hand {
            assert_eq!(ours, theirs, "{text}");
        }
    }

    #[test]
    #[should_panic(expected = "no Sop2 opcode named s_frobnicate")]
    fn an_unknown_name_panics() {
        sop2("s_frobnicate", 0, sgpr(0), sgpr(0));
    }
}
