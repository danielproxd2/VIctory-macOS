//! The stand-in executable has no C runtime: its entry point is its own
//! function and its stack reserve is a value the tests know.

/// `SizeOfStackReserve` of the built executable: far from every default
/// (1 MiB, 2 MiB, 8 MiB), so that a thread with this much stack got it from
/// the header. crates/victory-loader/tests/load_exe.rs has the same number.
const STACK_RESERVE: u32 = 0x60_0000;
const ENTRY: &str = "probe_exe_entry";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for arg in [
            format!("/ENTRY:{ENTRY}"),
            format!("/STACK:{STACK_RESERVE:#x}"),
            // Without `main` the linker cannot tell; a title is not a GUI
            // program to the PC it never ran on, so the plain one.
            "/SUBSYSTEM:CONSOLE".to_owned(),
        ] {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
    } else {
        for arg in [
            // The MinGW start files define the entry point and want `main`.
            "-nostartfiles".to_owned(),
            format!("-Wl,--entry={ENTRY}"),
            format!("-Wl,--stack={STACK_RESERVE:#x}"),
        ] {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
    }
}
