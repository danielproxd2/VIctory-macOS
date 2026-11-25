//! How imports get their addresses.

use victory_pe::ImportSymbol;

/// Who provides a bound import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provenance {
    /// Our implementation of the console API.
    Hle,
    /// The host's own implementation (e.g. Windows API sets that behave the
    /// same on console and PC).
    Host,
    /// A DLL the title ships, mapped like the executable.
    Title,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Found {
        address: usize,
        provenance: Provenance,
    },
    /// Nobody implements it; the loader installs a reporting stub.
    Missing,
}

/// Supplies an address for each `dll!symbol` the title imports.
///
/// `dll` is passed exactly as written in the image; implementations should
/// compare case-insensitively.
pub trait ImportResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution;

    /// Base address of the title's own module that goes by the name `dll`,
    /// if this resolver knows one: what `LoadLibrary(dll)` gives the title.
    /// The loader stores it where a delay-load descriptor keeps its DLL's
    /// module handle. `None`, the default, for every other provider: our
    /// runtime and the host have no module the title may be handed here.
    fn module(&self, dll: &str) -> Option<usize> {
        let _ = dll;
        None
    }
}
