//! XPackage: the title's own installation, complete and launchable, with
//! every chunk ready; no other package (content, add-ons) is installed.
//! The package is the folder of the title's executable (`G:` to the title).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::OnceLock;

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, put_string,
    query_interface, text, token,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xaf40_6016,
    0xe850,
    0x4aa8,
    [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
);

/// As Grand Theft Auto VI (2026) asks for it. Its slots up to 0xd0 take the
/// arguments the public API lists in the same order; 0x110 uninstalls a
/// package, 0x128/0x130 mount one with UI, 0x138 enumerates packages.
pub const IID_2026: Guid = Guid::new(
    0x0b9d_556e,
    0xab70,
    0x43f4,
    [0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40],
);

pub static OBJECT: Object = Object::new("package", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

/// The documented "could not resolve an identifier for the package".
pub const NO_PACKAGE: HRESULT = 0x8924_5208_u32 as i32;

/// Our installation's identifier: the title id in hex (titles keep it in 33
/// bytes and hand it back to us).
pub fn identifier() -> String {
    format!("{:08X}", runtime::title().id)
}

fn is_ours(id: *const c_char) -> bool {
    text(id).is_some_and(|id| id == identifier())
}

/// Bytes in the package folder, counted once.
fn package_bytes() -> u64 {
    fn size(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => size(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let exe = runtime::main_image().and_then(runtime::image_path);
        exe.as_deref().and_then(Path::parent).map_or(0, size)
    })
}

type This = *const Object;

extern "system" fn current_identifier(_: This, size: usize, buffer: *mut u8) -> HRESULT {
    put_string(&identifier(), size, buffer, std::ptr::null_mut())
}

extern "system" fn is_packaged(_: This) -> bool {
    true
}

/// Installation monitors all watch the same finished installation, so the
/// handle is a fixed address.
static MONITOR: u8 = 0;
fn monitor() -> *const c_void {
    (&raw const MONITOR).cast()
}

extern "system" fn create_monitor(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _queue: *const c_void,
    out: *mut *const c_void,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, monitor())
}
extern "system" fn close_monitor(_: This, _: *const c_void) {}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub total_bytes: u64,
    pub installed_bytes: u64,
    pub launch_bytes: u64,
    pub launchable: bool,
    pub completed: bool,
}

extern "system" fn progress(_: This, _: *const c_void, out: *mut Progress) {
    let bytes = package_bytes();
    put(
        out,
        Progress {
            total_bytes: bytes,
            installed_bytes: bytes,
            launch_bytes: bytes,
            launchable: true,
            completed: true,
        },
    );
}

/// The installation is complete, so progress never changes.
extern "system" fn register_progress(
    _: This,
    _monitor: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}

/// `XPackageUpdateInstallationMonitor`: "If any aspect of the progress
/// snapshot has changed, returns true." The installation is complete, so
/// nothing ever has.
extern "system" fn update_monitor(_: This, _monitor: *const c_void) -> bool {
    false
}

/// `XPackageUnregisterInstallationProgressChanged`: false only while a
/// callback of the registration is executing, and none ever runs.
extern "system" fn unregister_progress(
    _: This,
    _monitor: *const c_void,
    _token: u64,
    _wait: bool,
) -> bool {
    true
}

/// 0: ready.
// shortcut: every selector counts as installed, including ones the
// package's layout does not have (the console answers those with an
// invalid-selector error); Grand Theft Auto VI asks only about its own
// chunks, which a full install has.
extern "system" fn find_availability(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    out: *mut u32,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, 0)
}

extern "system" fn change_order(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

static INSTALL: u8 = 0;

/// Everything is installed: done at once, the result is a monitor.
extern "system" fn install_chunks_async(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = if is_ours(id) {
        Ok(xasync::bytes(monitor()))
    } else {
        Err(NO_PACKAGE)
    };
    xasync::finished_with(block, (&raw const INSTALL).cast(), result)
}
extern "system" fn install_chunks_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const INSTALL).cast(), out)
}

/// Nothing to download.
extern "system" fn estimate_download(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    bytes: *mut u64,
    confirm: *mut bool,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    if !confirm.is_null() {
        // SAFETY: checked non-null.
        unsafe { confirm.write(false) };
    }
    put(bytes, 0)
}

// shortcut: accepted but nothing is removed (the package folder is the
// user's), so the chunks stay ready; Grand Theft Auto VI's one call site
// (title+0x293b4b0, its chunk manager) would then see them still installed.
extern "system" fn uninstall_chunks(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

/// Mount handles only come from mounting another package, and there is
/// none.
extern "system" fn mount_path(_: This, _: *const c_void, _: usize, _: *mut u8) -> HRESULT {
    xasync::E_INVALIDARG
}
/// `XPackageGetMountPathSize`: as [`mount_path`], for the same reason.
extern "system" fn mount_path_size(_: This, _mount: *const c_void, _out: *mut usize) -> HRESULT {
    xasync::E_INVALIDARG
}
extern "system" fn close_mount(_: This, _: *const c_void) {}

/// Only other packages are uninstalled this way, and none is installed.
extern "system" fn uninstall_package(_: This, id: *const c_char) -> bool {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package uninstall refused"
    );
    false
}

static MOUNT: u8 = 0;

extern "system" fn mount_async(_: This, id: *const c_char, block: *mut AsyncBlock) -> HRESULT {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package mount: not installed"
    );
    xasync::finished(block, (&raw const MOUNT).cast(), NO_PACKAGE)
}
extern "system" fn mount_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const MOUNT).cast(), out)
}

/// Titles enumerate content packages (kind 1); there are none.
// shortcut: the game kind (0) also lists nothing, not this installation;
// Grand Theft Auto VI enumerates only content (kind 1, title+0x2937bf0).
extern "system" fn enumerate(
    _: This,
    kind: u32,
    _scope: u32,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    tracing::debug!(kind, "packages enumerated: none");
    S_OK
}

static VTABLE: Methods<40> = Methods([
    query_interface as *const (),
    add_ref as *const (),
    add_ref as *const (),
    current_identifier as *const (),    // 0x18
    is_packaged as *const (),           // 0x20
    create_monitor as *const (),        // 0x28
    close_monitor as *const (),         // 0x30
    progress as *const (),              // 0x38
    update_monitor as *const (),        // 0x40
    register_progress as *const (),     // 0x48
    unregister_progress as *const (),   // 0x50
    missing::<11> as *const (),         // 0x58 user locale
    find_availability as *const (),     // 0x60
    missing::<13> as *const (),         // 0x68 enumerate chunk availability
    change_order as *const (),          // 0x70
    missing::<15> as *const (),         // 0x78 install chunks
    install_chunks_async as *const (),  // 0x80
    install_chunks_result as *const (), // 0x88
    estimate_download as *const (),     // 0x90
    uninstall_chunks as *const (),      // 0x98
    missing::<20> as *const (),         // 0xa0
    missing::<21> as *const (),         // 0xa8
    missing::<22> as *const (),         // 0xb0
    missing::<23> as *const (),         // 0xb8 mount (deprecated)
    mount_path_size as *const (),       // 0xc0
    mount_path as *const (),            // 0xc8
    close_mount as *const (),           // 0xd0
    missing::<27> as *const (),         // 0xd8
    missing::<28> as *const (),         // 0xe0 enumerate packages (older)
    missing::<29> as *const (),         // 0xe8
    missing::<30> as *const (),         // 0xf0
    missing::<31> as *const (),         // 0xf8
    missing::<32> as *const (),         // 0x100
    missing::<33> as *const (),         // 0x108
    uninstall_package as *const (),     // 0x110
    missing::<35> as *const (),         // 0x118
    missing::<36> as *const (),         // 0x120
    mount_async as *const (),           // 0x128
    mount_result as *const (),          // 0x130
    enumerate as *const (),             // 0x138
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn the_installation_is_complete_and_alone() {
        let this: This = &OBJECT;
        type Id = extern "system" fn(This, usize, *mut u8) -> HRESULT;
        let mut id = [0u8; 33];
        assert_eq!(method::<Id>(&OBJECT, 0x18)(this, 33, id.as_mut_ptr()), S_OK);
        let len = id.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&id[..len]).unwrap(), identifier());
        assert!(method::<extern "system" fn(This) -> bool>(&OBJECT, 0x20)(
            this
        ));

        type Find =
            extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
        let mut availability = 9;
        let ours = format!("{}\0", identifier());
        let find = method::<Find>(&OBJECT, 0x60);
        let none = std::ptr::null();
        assert_eq!(
            find(this, ours.as_ptr().cast(), 0, none, &mut availability),
            S_OK
        );
        assert_eq!(availability, 0, "ready");
        assert_eq!(
            find(this, c"dlc".as_ptr(), 0, none, &mut availability),
            NO_PACKAGE
        );

        type Progress_ = extern "system" fn(This, *const c_void, *mut Progress);
        let mut p = Progress {
            total_bytes: 1,
            installed_bytes: 0,
            launch_bytes: 0,
            launchable: false,
            completed: false,
        };
        method::<Progress_>(&OBJECT, 0x38)(this, monitor(), &mut p);
        assert!(p.completed && p.launchable && p.installed_bytes == p.total_bytes);

        type Mount = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            method::<Mount>(&OBJECT, 0x128)(this, c"dlc".as_ptr(), &mut block),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut block, true), NO_PACKAGE);
    }

    fn ours() -> std::ffi::CString {
        std::ffi::CString::new(identifier()).unwrap()
    }

    /// A finished installation never changes: a refresh reports no
    /// change, and a registration that never fires unregisters at once.
    #[test]
    fn a_complete_installation_reports_no_change() {
        let this: This = &OBJECT;
        type Register = extern "system" fn(
            This,
            *const c_void,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        type Update = extern "system" fn(This, *const c_void) -> bool;
        type Unregister = extern "system" fn(This, *const c_void, u64, bool) -> bool;
        type Progress_ = extern "system" fn(This, *const c_void, *mut Progress);
        let snapshot = || {
            let mut p = std::mem::MaybeUninit::<Progress>::zeroed();
            method::<Progress_>(&OBJECT, 0x38)(this, monitor(), p.as_mut_ptr());
            // SAFETY: written by the call.
            unsafe { p.assume_init() }
        };
        let before = snapshot();
        assert!(before.completed);
        let mut token = 0;
        let none = std::ptr::null_mut();
        let register = method::<Register>(&OBJECT, 0x48);
        assert_eq!(
            register(this, monitor(), none, std::ptr::null(), &mut token),
            S_OK
        );
        assert_ne!(token, 0);
        assert!(!method::<Update>(&OBJECT, 0x40)(this, monitor()));
        assert_eq!(snapshot(), before);
        let unregister = method::<Unregister>(&OBJECT, 0x50);
        assert!(unregister(this, monitor(), token, false));
        assert!(unregister(this, monitor(), token, true));
    }

    /// No other package can be mounted, so nothing is a mount handle:
    /// asking for its path size fails as asking for its path does.
    #[test]
    fn what_is_no_mount_handle_has_no_path_size() {
        let this: This = &OBJECT;
        type Size = extern "system" fn(This, *const c_void, *mut usize) -> HRESULT;
        type Path_ = extern "system" fn(This, *const c_void, usize, *mut u8) -> HRESULT;
        let mut size = 77;
        let mut path = [0x55u8; 8];
        for handle in [std::ptr::null(), monitor()] {
            let sized = method::<Size>(&OBJECT, 0xc0)(this, handle, &mut size);
            let got = method::<Path_>(&OBJECT, 0xc8)(this, handle, path.len(), path.as_mut_ptr());
            assert_eq!((sized, got), (xasync::E_INVALIDARG, xasync::E_INVALIDARG));
        }
        assert_eq!((size, path), (77, [0x55; 8]), "nothing was written");
    }

    extern "system" fn count_package(_details: *const c_void, context: *mut c_void) -> bool {
        // SAFETY: the test's counter.
        unsafe { *context.cast::<u32>() += 1 };
        true
    }

    /// Pins a shortcut: enumerating packages lists nothing for any kind,
    /// the game kind (0) included, although this installation is one.
    /// What its details would hold (the package identifier's form, the
    /// store id, the version, the display name) is not established by
    /// anything public we can cite, so it is not made up.
    #[test]
    fn package_enumeration_lists_nothing_for_any_kind() {
        type Enumerate = extern "system" fn(
            This,
            u32,
            u32,
            *mut c_void,
            extern "system" fn(*const c_void, *mut c_void) -> bool,
        ) -> HRESULT;
        let enumerate = method::<Enumerate>(&OBJECT, 0x138);
        let mut seen = 0u32;
        for kind in [0, 1] {
            for scope in [0, 1] {
                let context = (&raw mut seen).cast();
                assert_eq!(
                    enumerate(&OBJECT, kind, scope, context, count_package),
                    S_OK
                );
            }
        }
        assert_eq!(seen, 0);
    }

    /// Pins a shortcut: whatever chunks a call names (here a selector no
    /// layout has), they count as installed, before and after they are
    /// "uninstalled"; only the package identifier is checked. The console
    /// answers selectors a package lacks with an error; telling them
    /// apart needs the package's layout, which we do not read.
    #[test]
    fn every_selector_counts_as_installed_today() {
        let this: This = &OBJECT;
        type Find =
            extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
        type Chunks = extern "system" fn(This, *const c_char, u32, *const c_void) -> HRESULT;
        type Estimate = extern "system" fn(
            This,
            *const c_char,
            u32,
            *const c_void,
            *mut u64,
            *mut bool,
        ) -> HRESULT;
        type Install = extern "system" fn(
            This,
            *const c_char,
            u32,
            *const c_void,
            u32,
            bool,
            *mut AsyncBlock,
        ) -> HRESULT;
        type Installed = extern "system" fn(This, *mut AsyncBlock, *mut *const c_void) -> HRESULT;
        // A selector by chunk id, with an id no package has.
        let selector: [u64; 2] = [1, 0xFFFF_FFFF];
        let selectors = selector.as_ptr().cast::<c_void>();
        let ours = ours();
        let block = || AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let availability = |id: &std::ffi::CStr| {
            let mut out = 9;
            let hr = method::<Find>(&OBJECT, 0x60)(this, id.as_ptr(), 1, selectors, &mut out);
            (hr, out)
        };
        assert_eq!(availability(&ours), (S_OK, 0), "ready");
        assert_eq!(
            method::<Chunks>(&OBJECT, 0x70)(this, ours.as_ptr(), 1, selectors),
            S_OK
        );
        let (mut bytes, mut confirm) = (5, true);
        let estimate = method::<Estimate>(&OBJECT, 0x90);
        assert_eq!(
            estimate(this, ours.as_ptr(), 1, selectors, &mut bytes, &mut confirm),
            S_OK
        );
        assert_eq!((bytes, confirm), (0, false), "nothing to download");
        let mut b = block();
        let install = method::<Install>(&OBJECT, 0x80);
        assert_eq!(
            install(this, ours.as_ptr(), 1, selectors, 0, true, &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut installed = std::ptr::null();
        assert_eq!(
            method::<Installed>(&OBJECT, 0x88)(this, &mut b, &mut installed),
            S_OK
        );
        assert_eq!(installed, monitor());
        // Uninstalling is accepted and removes nothing.
        assert_eq!(
            method::<Chunks>(&OBJECT, 0x98)(this, ours.as_ptr(), 1, selectors),
            S_OK
        );
        assert_eq!(availability(&ours), (S_OK, 0), "still ready");

        // Another package is none of ours, whatever it is asked for.
        let other = c"dlc";
        assert_eq!(availability(other), (NO_PACKAGE, 9));
        assert_eq!(
            method::<Chunks>(&OBJECT, 0x70)(this, other.as_ptr(), 1, selectors),
            NO_PACKAGE
        );
        assert_eq!(
            method::<Chunks>(&OBJECT, 0x98)(this, other.as_ptr(), 1, selectors),
            NO_PACKAGE
        );
        assert_eq!(
            estimate(this, other.as_ptr(), 1, selectors, &mut bytes, &mut confirm),
            NO_PACKAGE
        );
        let mut b = block();
        assert_eq!(
            install(this, other.as_ptr(), 1, selectors, 0, true, &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), NO_PACKAGE);
        // Packages are never uninstalled: there is none but the game.
        type Uninstall = extern "system" fn(This, *const c_char) -> bool;
        assert!(!method::<Uninstall>(&OBJECT, 0x110)(this, other.as_ptr()));
        assert!(!method::<Uninstall>(&OBJECT, 0x110)(this, ours.as_ptr()));
    }

    /// Pins two calls left unimplemented, and why. `XPackageGetUserLocale`
    /// (0x58) answers "the user locale that most closely matches the
    /// package locale" and nothing "if no locale information is declared
    /// for the package": both depend on the languages the package
    /// declares, which we do not read, and the host's own locale is not
    /// that answer. `XPackageEnumerateChunkAvailability` (0x68) lists the
    /// package's chunks, features or languages, which needs its layout.
    #[test]
    fn user_locale_and_chunk_enumeration_are_not_implemented() {
        type Method = extern "system" fn(This) -> HRESULT;
        for slot in [0x58, 0x68] {
            assert_eq!(
                method::<Method>(&OBJECT, slot)(&OBJECT),
                xasync::E_NOTIMPL,
                "{slot:#x}"
            );
        }
    }
}
