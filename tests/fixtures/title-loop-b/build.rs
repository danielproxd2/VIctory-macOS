fn main() {
    // As for title-quiet: this DLL has no runtime and defines its own
    // entry point, which the MinGW start files would define too.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        println!("cargo:rustc-link-arg-cdylib=-nostartfiles");
    }
}
