fn main() {
    // The MinGW start files define the entry point (and bring static TLS
    // with them); this DLL defines its own. MSVC's linker takes ours as it
    // is, since nothing here refers to the C runtime.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        println!("cargo:rustc-link-arg-cdylib=-nostartfiles");
    }
}
