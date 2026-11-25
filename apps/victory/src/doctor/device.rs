//! What the doctor asks Windows (or Wine) itself: which host this is, and
//! what its D3D12 device says and does. Deciding what a finding means is
//! `super`'s, where it is tested on every host.

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::{CastAsked, Casts, Device, DeviceFacts, HostKind};

/// The host: Wine with the version its ntdll gives (`wine_get_version`),
/// or Windows.
pub(super) fn host() -> HostKind {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: ntdll is always loaded, both names end with a NUL, and
    // `wine_get_version` takes nothing and returns a NUL-terminated string
    // that lives as long as ntdll.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if ntdll.is_null() {
            return HostKind::Windows;
        }
        let Some(version) = GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()) else {
            return HostKind::Windows;
        };
        let version: unsafe extern "C" fn() -> *const std::ffi::c_char =
            std::mem::transmute(version);
        let text = version();
        HostKind::Wine((!text.is_null()).then(|| {
            std::ffi::CStr::from_ptr(text)
                .to_string_lossy()
                .into_owned()
        }))
    }
}

/// Whether the processor has AVX2, asked the way `run --execute` asks.
pub(super) fn has_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        true
    }
}

/// The texture formats the doctor asks the device about, each with the
/// formats a texture of it is to be castable to. These are formats
/// `victory-gpu` offers for them (`resource::castable`: every format of
/// the same element); the last of the first row and the last two of the
/// second are the ones D3DMetal was measured to refuse (docs/macos.md).
const CASTS: [(u32, &[u32]); 2] = [
    (28, &[29, 87, 24, 26, 41, 42, 35, 67, 89]),
    (56, &[49, 57, 85, 68, 69]),
];

/// Makes the device a run makes, has `victory-gpu` make its own (which
/// runs its probes) and asks the device what the report shows.
pub(super) fn measure() -> Device {
    // The runtime's device first: it is the one a title's lists run on.
    let (available, logged) = super::capture::capture(victory_gpu::host_available);
    if !available {
        let why = logged
            .iter()
            .find(|l| l.message == super::RUNTIME_NO_DEVICE)
            .and_then(|l| l.field("error"))
            .unwrap_or("VIctory's D3D12 runtime gave no reason");
        return Device::None(why.to_owned());
    }
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain device creation with a valid out pointer, as the
    // runtime does it (`victory-gpu`, `host::create`).
    if let Err(e) = unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) } {
        return Device::None(e.message());
    }
    let Some(device) = device else {
        return Device::None("D3D12CreateDevice returned no device".to_owned());
    };
    Device::Made(DeviceFacts {
        adapter: adapter_name(&device),
        shader_model: shader_model(&device),
        mesh_tier: mesh_tier(&device),
        atomics_claimed: atomics_claimed(&device),
        casts: casts(&device),
        logged,
    })
}

/// The name the device's adapter gives itself.
fn adapter_name(device: &ID3D12Device) -> Option<String> {
    // SAFETY: plain DXGI queries with valid out pointers.
    let desc = unsafe {
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(device.GetAdapterLuid()))
            .and_then(|a| a.GetDesc1())
    }
    .ok()?;
    let name = &desc.Description;
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..end]))
}

/// The highest shader model the device takes, as D3D12 numbers them
/// (0x66 is 6.6). The query fails for a model the runtime does not know,
/// so it is asked from 6.9 down.
fn shader_model(device: &ID3D12Device) -> Option<u32> {
    (0x60..=0x69).rev().find_map(|asked| {
        let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
            HighestShaderModel: D3D_SHADER_MODEL(asked),
        };
        // SAFETY: the query writes its own structure of the given size.
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut model).cast(),
                size_of_val(&model) as u32,
            )
        }
        .ok()
        .map(|()| model.HighestShaderModel.0 as u32)
    })
}

/// The device's mesh shader tier, as D3D12 numbers them (0: none, 10:
/// tier 1).
fn mesh_tier(device: &ID3D12Device) -> Option<u32> {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    // SAFETY: the query writes its own structure of the given size.
    unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS7,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        )
    }
    .ok()
    .map(|()| options.MeshShaderTier.0 as u32)
}

/// Whether the device reports 64-bit atomics on typed resources and on
/// resources from the descriptor heap: the two capability bits the
/// runtime's probe starts from.
fn atomics_claimed(device: &ID3D12Device) -> Option<bool> {
    let mut typed = D3D12_FEATURE_DATA_D3D12_OPTIONS9::default();
    let mut heap = D3D12_FEATURE_DATA_D3D12_OPTIONS11::default();
    // SAFETY: each query writes its own structure of the given size.
    unsafe {
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS9,
                (&raw mut typed).cast(),
                size_of_val(&typed) as u32,
            )
            .ok()?;
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS11,
                (&raw mut heap).cast(),
                size_of_val(&heap) as u32,
            )
            .ok()?;
    }
    Some(
        typed.AtomicInt64OnTypedResourceSupported.as_bool()
            && heap.AtomicInt64OnDescriptorHeapResourceSupported.as_bool(),
    )
}

/// Asks the device about each castable format of [`CASTS`] by itself, on
/// a 4 by 4 texture, which is how `victory-gpu` asks after a refused list
/// (`casts::learn`). The runtime asks only when a title's texture was
/// refused, so without a title the doctor has to ask here.
fn casts(device: &ID3D12Device) -> Casts {
    let Ok(device) = device.cast::<ID3D12Device10>() else {
        return Casts::NotAsked("the device has no ID3D12Device10".to_owned());
    };
    let mut asked = Vec::new();
    for (own, formats) in CASTS {
        if !makes(&device, own, None) {
            return Casts::NotAsked(format!(
                "the device makes no 4 by 4 texture of format {own} even without castable formats"
            ));
        }
        for &cast in formats {
            asked.push(CastAsked {
                own,
                cast,
                taken: makes(&device, own, Some(cast)),
            });
        }
    }
    Casts::Asked(asked)
}

/// Whether the device makes a 4 by 4 texture of format `own`, castable to
/// `cast` if given.
fn makes(device: &ID3D12Device10, own: u32, cast: Option<u32>) -> bool {
    let small = D3D12_RESOURCE_DESC1 {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 4,
        Height: 4,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(own as i32),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let formats: Vec<DXGI_FORMAT> = cast.iter().map(|&f| DXGI_FORMAT(f as i32)).collect();
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: a description and an out pointer of ours; the list lives
    // through the call.
    unsafe {
        device.CreateCommittedResource3(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &small,
            D3D12_BARRIER_LAYOUT_COMMON,
            None,
            None::<&ID3D12ProtectedResourceSession>,
            cast.is_some().then_some(&formats[..]),
            &mut texture,
        )
    }
    .is_ok()
}

/// Loads the DXIL validator `path` and has it check each of `containers`
/// (a name and the bytes): what it said about each, or why it did not
/// load.
pub(super) fn validate(
    path: &std::path::Path,
    containers: &[(&'static str, &[u8])],
) -> Result<Vec<(&'static str, Result<(), String>)>, String> {
    let mut validator = victory_shader::emit::validator::Validator::load(Some(path))?;
    Ok(containers
        .iter()
        .map(|&(name, bytes)| {
            // The validator signs in place: a copy, never the embedded bytes.
            let mut copy = bytes.to_vec();
            (name, validator.validate(&mut copy))
        })
        .collect())
}
