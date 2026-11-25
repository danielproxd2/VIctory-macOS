//! What the tests of the stand-in title that draws (`draw_title.rs`) hand
//! it and expect back, whichever way they run it: the two console shader
//! containers, made here from the test kit's programs and translated by
//! VIctory when the title's pipeline is first drawn with, the clear
//! colour, and every texel of the frame the title read back.

use super::title::{Report, SIDE, STEPS};

/// What the pixel shader writes, and what the title clears its target to.
/// Each channel is a whole number of 255ths, so that the 8-bit texel it
/// becomes does not depend on how a host rounds a half.
pub const DRAWN: [f32; 4] = [0.2, 0.4, 0.8, 1.0];
pub const CLEARED: [f32; 4] = [0.6, 0.0, 0.2, 1.0];

/// The console container of the title's vertex shader: the triangle below
/// the target's diagonal.
pub fn vertex_shader() -> Vec<u8> {
    victory_shader::testkit::vs_half_screen_triangle()
}

/// The console container of the title's pixel shader: [`DRAWN`].
pub fn pixel_shader() -> Vec<u8> {
    victory_shader::testkit::ps_constant(DRAWN)
}

/// `colour` as an `R8G8B8A8_UNORM` texel.
pub fn texel(colour: [f32; 4]) -> [u8; 4] {
    colour.map(|c| (c * 255.0).round() as u8)
}

/// The frame as rows of texels, for a failure's message.
fn picture(report: &Report) -> String {
    let mut text = String::new();
    for y in 0..SIDE {
        for x in 0..SIDE {
            let [r, g, b, a] = report.pixel(x, y);
            text.push_str(&format!(" {r:02x}{g:02x}{b:02x}{a:02x}"));
        }
        text.push('\n');
    }
    text
}

/// What is wrong with a run that drew [`DRAWN`] over [`CLEARED`], if
/// anything: the title got through every step; every texel below the
/// diagonal (column less than row) has the shader's colour and every one
/// above it the clear colour; and a texel on the diagonal, whose centre
/// lies on the triangle's edge, has one of the two.
pub fn wrong(report: &Report) -> Result<(), String> {
    if report.failed != 0 {
        let step = STEPS.get(report.failed as usize).unwrap_or(&"?");
        return Err(format!(
            "the title stopped at step {} ({step}), which returned {:#010x}, after {} frames:\n{}",
            report.failed,
            report.hresult,
            report.frames,
            picture(report)
        ));
    }
    let (drawn, cleared) = (texel(DRAWN), texel(CLEARED));
    for y in 0..SIDE {
        for x in 0..SIDE {
            let found = report.pixel(x, y);
            let right = match x.cmp(&y) {
                std::cmp::Ordering::Less => found == drawn,
                std::cmp::Ordering::Greater => found == cleared,
                std::cmp::Ordering::Equal => found == drawn || found == cleared,
            };
            if !right {
                return Err(format!(
                    "texel ({x}, {y}) is {found:02x?}; inside the triangle is {drawn:02x?}, \
                     outside {cleared:02x?} (frame {}):\n{}",
                    report.frames,
                    picture(report)
                ));
            }
        }
    }
    Ok(())
}

/// Fails the calling test with [`wrong`]'s message.
#[track_caller]
pub fn check(report: &Report) {
    if let Err(why) = wrong(report) {
        panic!("{why}");
    }
}
