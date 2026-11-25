//! What the tests of the commands share: reports caught in memory,
//! synthetic input files assembled in code, and the skip line.

use std::path::Path;

use anyhow::Result;
use victory_shader::HwStage;
use victory_shader::testkit;

/// What `command` wrote to its report, and how it ended.
pub(crate) fn report(
    command: impl FnOnce(&mut dyn std::io::Write) -> Result<()>,
) -> (String, Result<()>) {
    let mut out = Vec::new();
    let result = command(&mut out);
    (String::from_utf8(out).expect("reports are UTF-8"), result)
}

/// The report of a command that must succeed.
pub(crate) fn report_of(command: impl FnOnce(&mut dyn std::io::Write) -> Result<()>) -> String {
    let (text, result) = report(command);
    if let Err(e) = result {
        panic!("the command failed: {e:#}\n{text}");
    }
    text
}

/// The one line of `text` that starts with `start`.
#[track_caller]
pub(crate) fn line<'a>(text: &'a str, start: &str) -> &'a str {
    let mut found = text.lines().filter(|l| l.starts_with(start));
    match (found.next(), found.next()) {
        (Some(line), None) => line,
        (None, _) => panic!("no line starts with {start:?} in:\n{text}"),
        (Some(_), Some(_)) => panic!("several lines start with {start:?} in:\n{text}"),
    }
}

/// Says that the running test did not check something and why, as the
/// line `skipped: <reason> (<test>)` past the test harness's capture.
#[cfg(windows)]
pub(crate) fn skip(reason: &str) {
    let thread = std::thread::current();
    victory_gpu::test_support::skipped(&format!("{reason} ({})", thread.name().unwrap_or("?")));
}

/// A console container with the vertex program that covers the screen.
pub(crate) fn vertex_container() -> Vec<u8> {
    testkit::vs_fullscreen_triangle()
}

/// A console container with a pixel program that writes one colour.
pub(crate) fn pixel_container() -> Vec<u8> {
    testkit::ps_constant([1.0, 0.5, 0.25, 1.0])
}

/// A console container with a hull program (`s_endpgm` alone), a stage the
/// translator does not take.
pub(crate) fn hull_container() -> Vec<u8> {
    let end = victory_shader::gfx10::asm::S_ENDPGM;
    testkit::console_container(HwStage::Hs, &[end], &[], &[])
}

/// One SGD5 record holding `payload`, as `victory_game::sgd5` reads it
/// (its module comment has the layout): the name with its length byte,
/// the kind, the sizes around the marker, then the raw deflate stream.
pub(crate) fn sgd5_record(name: &str, kind: u8, payload: &[u8]) -> Vec<u8> {
    let packed = miniz_oxide::deflate::compress_to_vec(payload, 6);
    let n = (packed.len() + 12) as u32;
    let mut out = vec![0xF4, 0x01, 0, 0, name.len() as u8 + 1];
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&[0, kind]);
    for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&packed);
    out
}

/// An SGD5 file holding `records`.
pub(crate) fn sgd5_file(records: &[Vec<u8>]) -> Vec<u8> {
    let mut file = victory_game::sgd5::MAGIC.to_vec();
    file.extend_from_slice(&[0; 12]);
    for record in records {
        file.extend_from_slice(record);
    }
    file
}

/// Writes `bytes` to `dir/name`.
pub(crate) fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("writing a test file");
    path
}

/// A stand-in title image, assembled by `victory_pe::testing`: a DLL that
/// imports one function our runtime implements, one from a DLL nobody
/// has, one by ordinal, and one more through a delay-load table; it
/// exports one function by name and forwards another.
pub(crate) fn title_image() -> Vec<u8> {
    use victory_pe::ImportSymbol;
    use victory_pe::testing::{DELAYLOAD_RVA_BASED, Export, PeBuilder, scn};
    let named = |name: &str| ImportSymbol::Name {
        hint: 0,
        name: name.to_owned(),
    };
    PeBuilder::new()
        .dll(true)
        .entry_rva(0x1000)
        .section(".text", scn::CODE, &[0xC3; 0x20])
        .import("XGameRuntime.dll", &[named("XGameRuntimeInitialize")])
        .import(
            MISSING_DLL,
            &[named(MISSING_FUNCTION), ImportSymbol::Ordinal(7)],
        )
        .delay_import(
            "victory-test-delayed.dll",
            DELAYLOAD_RVA_BASED,
            &[named("VIctoryTestDelayed")],
        )
        .exports(
            "title.dll",
            1,
            vec![
                Export::named("title_entry", 0x1000),
                Export::forwarder("title_forwarded", "other.function"),
            ],
        )
        .build()
        .expect("the image is well formed")
        .bytes
}

/// A DLL no runtime has, and a function of it, in [`title_image`].
pub(crate) const MISSING_DLL: &str = "victory-test-missing.dll";
pub(crate) const MISSING_FUNCTION: &str = "VIctoryTestNotImplemented";
