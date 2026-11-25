//! A stand-in title that draws (`title_draw.dll`), written by us: the
//! title of `../../draw_title.rs` with the console's graphics runtime in
//! its import table, as a title has it. `d3d12_xs.dll` exists on no PC, so
//! this image loads only through victory-loader, whose resolver binds the
//! three imports to VIctory's runtime: the device creation, the root
//! signature serializer, and the device's interface id, which is data.
//!
//! It holds no shader of its own (nothing binary is kept in this
//! repository): the test that maps it hands it the two containers it made,
//! as files a title reads its shaders from would.

#[path = "../../draw_title.rs"]
mod title;

use std::ffi::c_void;

use title::{Hresult, Iid, Object, Report, RootDesc, Runtime};

#[link(name = "d3d12_xs", kind = "raw-dylib")]
unsafe extern "system" {
    fn D3D12XboxCreateDevice(
        adapter: *const c_void,
        parameters: *const u32,
        iid: *const Iid,
        out: *mut Object,
    ) -> Hresult;
    fn D3D12SerializeRootSignature(
        desc: *const RootDesc,
        version: u32,
        blob: *mut Object,
        errors: *mut Object,
    ) -> Hresult;
    static IID_ID3D12Device: Iid;
}

/// Runs the title with the vertex and pixel shader containers at `vs` and
/// `ps` (`vs_len` and `ps_len` bytes) and the clear colour at `clear`, and
/// writes what it came to at `report`. Returns the step it failed at
/// ([`title::STEPS`]), 0 for none.
///
/// # Safety
/// The pointers are to what the lengths and types say, and `report` is
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn title_draw_run(
    vs: *const u8,
    vs_len: usize,
    ps: *const u8,
    ps_len: usize,
    clear: *const [f32; 4],
    report: *mut Report,
) -> u32 {
    let runtime = Runtime {
        create_device: D3D12XboxCreateDevice,
        serialize_root_signature: D3D12SerializeRootSignature,
        device_iid: &raw const IID_ID3D12Device,
    };
    // SAFETY: the caller's pointers and lengths; the imports are bound to
    // the console runtime by whoever mapped this image.
    unsafe {
        let (vs, ps) = (
            std::slice::from_raw_parts(vs, vs_len),
            std::slice::from_raw_parts(ps, ps_len),
        );
        let ran = title::run(&runtime, vs, ps, *clear);
        report.write(ran);
        ran.failed
    }
}
