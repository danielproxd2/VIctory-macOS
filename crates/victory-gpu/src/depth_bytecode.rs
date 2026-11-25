//! The depth conversion shader (`depth.hlsl`) compiled by Microsoft's
//! compilers on a Windows runner. Written by `cargo xtask embed-depth`.
//! `SOURCE_HASH` names the source it was built from, and other bytes are
//! never used. No bytes and a hash of zero: not built yet.

pub(crate) const SOURCE_HASH: u64 = 0x0000000000000000;
pub(crate) const DXIL: &[u8] = &[];
pub(crate) const DXBC: &[u8] = &[];
