//! The stand-in title that draws (tests/fixtures/draw_title.rs) on the
//! device this crate hands a title: nothing but the creation entry points
//! is called by name, and from the device on everything goes through the
//! objects' method tables, as a title's compiled code calls them. The two
//! shaders are console containers (`victory_shader::testkit`), translated
//! by this crate when the pipeline is first drawn with.
//!
//! Needs the host's D3D12 device: without one the test passes after
//! `skipped: no host D3D12 device`, and fails with `VICTORY_REQUIRE_GPU=1`.

#[path = "../../../tests/fixtures/draw_title_expect.rs"]
mod expect;
#[path = "../../../tests/fixtures/draw_title.rs"]
mod title;

/// The title's frame holds its triangle: the shader's colour below the
/// target's diagonal and the clear colour above it.
#[test]
fn a_title_draws_a_triangle_through_the_device_and_list_tables() {
    let Some(_gpu) = victory_gpu::test_support::gpu() else {
        return;
    };
    // SAFETY: the two entry points have these signatures but for the
    // names of their pointer types.
    let runtime = unsafe {
        title::Runtime {
            create_device: std::mem::transmute::<usize, title::CreateDevice>(
                victory_gpu::device::create as *const () as usize,
            ),
            serialize_root_signature: std::mem::transmute::<usize, title::SerializeRootSignature>(
                victory_gpu::root::serialize as *const () as usize,
            ),
            device_iid: &raw const victory_gpu::device::IID,
        }
    };
    let (vs, ps) = (expect::vertex_shader(), expect::pixel_shader());
    // SAFETY: the runtime's own entry points and interface id.
    let report = unsafe { title::run(&runtime, &vs, &ps, expect::CLEARED) };
    expect::check(&report);
}

/// What the test above would let through: a frame that is all one colour,
/// a triangle of another colour, the triangle on the other side of the
/// diagonal, and a title that stopped at a step are each told apart from
/// the frame that is right. No device is needed for it.
#[test]
fn a_wrong_frame_is_told_from_the_right_one() {
    use title::{Report, SIDE};
    let (drawn, cleared) = (expect::texel(expect::DRAWN), expect::texel(expect::CLEARED));
    assert_eq!((drawn, cleared), ([51, 102, 204, 255], [153, 0, 51, 255]));
    let frame = |below: [u8; 4], above: [u8; 4], on: [u8; 4]| {
        let mut report = Report::new();
        report.frames = 1;
        for y in 0..SIDE {
            for x in 0..SIDE {
                report.pixels[y * SIDE + x] = match x.cmp(&y) {
                    std::cmp::Ordering::Less => below,
                    std::cmp::Ordering::Greater => above,
                    std::cmp::Ordering::Equal => on,
                };
            }
        }
        report
    };
    // The edge's texels may fall to either side.
    assert_eq!(expect::wrong(&frame(drawn, cleared, drawn)), Ok(()));
    assert_eq!(expect::wrong(&frame(drawn, cleared, cleared)), Ok(()));
    let said = |report: &Report| expect::wrong(report).expect_err("a wrong frame");
    assert!(
        said(&frame(cleared, cleared, cleared)).starts_with("texel (0, 1) is [99, 00, 33, ff]")
    );
    assert!(said(&frame(drawn, drawn, drawn)).starts_with("texel (1, 0) is [33, 66, cc, ff]"));
    assert!(said(&frame(cleared, drawn, drawn)).starts_with("texel (1, 0) is"));
    let other = [51, 102, 203, 255];
    assert!(said(&frame(other, cleared, cleared)).starts_with("texel (0, 1) is [33, 66, cb, ff]"));
    assert!(said(&frame(drawn, cleared, [0; 4])).starts_with("texel (0, 0) is"));
    let mut stopped = frame(drawn, cleared, drawn);
    (stopped.failed, stopped.hresult) = (12, 0x8007_0057_u32 as i32);
    let said = said(&stopped);
    assert!(
        said.starts_with(
            "the title stopped at step 12 (CreatePipelineState (device 0x2b0)), \
             which returned 0x80070057, after 1 frames:"
        ),
        "{said}"
    );
}
