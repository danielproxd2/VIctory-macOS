//! Castable formats the host does not take.
//!
//! A host texture is made with the list of formats its views may cast it
//! to (`resource::castable`: every format of the same element), so that
//! title textures of one memory in several formats share one host texture
//! and a view in another format (sRGB of UNORM, say) reads as that format.
//! The host takes the list whole or refuses the texture, and upstream then
//! makes the texture without any castable format.
//!
//! Apple's D3DMetal refuses every list that names a format it has no Metal
//! pixel format for (docs/macos.md, "Open differences under D3DMetal";
//! measured: `R10G10B10_XR_BIAS_A2_UNORM` among the formats of 32 bits,
//! `R8G8_B8G8_UNORM` and `G8R8_G8B8_UNORM` among those of 16), with
//! `E_INVALIDARG` from `CreateCommittedResource3` and
//! `CreatePlacedResource2` alike. Left at that, no RGBA8, BGRA8, R32,
//! R10G10B10A2, R11G11B10 or R16 texture would be castable to anything
//! there, nor placed in a heap.
//!
//! So a refused list is asked about format by format ([`learn`], on a 4 by
//! 4 texture of the same description, once per texture format and
//! castable format in a process), and textures are made castable to the
//! formats the host takes ([`taken`]). A host that takes the whole list
//! (the PC hardware upstream's tests were measured on) is never asked and
//! nothing is left out there.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

/// What the host was asked: whether it refused a texture of the first
/// format castable to the second.
static ASKED: Mutex<Option<HashMap<(u32, u32), bool>>> = Mutex::new(None);

/// `formats` (the castable formats of a texture of `format`) without those
/// this host is known to refuse for it. All of them until a list was
/// refused and [`learn`] found out which.
pub(crate) fn taken(format: u32, mut formats: Vec<u32>) -> Vec<u32> {
    let asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(asked) = asked.as_ref() {
        formats.retain(|&f| asked.get(&(format, f)) != Some(&true));
    }
    formats
}

/// After the host refused a texture of description `plain` castable to
/// `formats`: whether it refuses one of those formats by itself, so that
/// the texture is worth making again castable to what [`taken`] leaves.
/// False where the refusal is not a format's (the host refuses the small
/// texture without any list too, or takes each format alone): the caller
/// goes on as it did before.
pub(crate) fn learn(device: &ID3D12Device, plain: &D3D12_RESOURCE_DESC, formats: &[u32]) -> bool {
    let Ok(device) = device.cast::<ID3D12Device10>() else {
        return false;
    };
    let own = plain.Format.0 as u32;
    let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    let asked = asked.get_or_insert_with(HashMap::new);
    let new: Vec<u32> = formats
        .iter()
        .copied()
        .filter(|&f| !asked.contains_key(&(own, f)))
        .collect();
    if !new.is_empty() && makes(&device, plain, None) {
        let mut refused = Vec::new();
        for f in new {
            let no = !makes(&device, plain, Some(f));
            asked.insert((own, f), no);
            if no {
                refused.push(f);
            }
        }
        if !refused.is_empty() {
            tracing::warn!(
                format = own,
                ?refused,
                "the host refuses these castable formats: textures are made castable to the others"
            );
        }
    }
    formats.iter().any(|&f| asked.get(&(own, f)) == Some(&true))
}

/// Whether the host makes a 4 by 4 texture of `plain`'s format, dimension
/// and uses, castable to `cast` if given.
fn makes(device: &ID3D12Device10, plain: &D3D12_RESOURCE_DESC, cast: Option<u32>) -> bool {
    let small = D3D12_RESOURCE_DESC1 {
        Dimension: plain.Dimension,
        Width: 4,
        Height: plain.Height.min(4),
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: plain.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: plain.Layout,
        Flags: plain.Flags,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::castable;

    /// A 64 by 64 texture of `format`, as a title's plain textures are.
    fn plain(format: u32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT(format as i32),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    /// On the device, for RGBA8, R32_UINT, R16_UNORM, RGBA16F and BC1
    /// textures: the host takes every castable format of each, as the PC
    /// hardware does, and nothing is left out of their lists; or it is
    /// D3DMetal and refuses exactly the formats measured there (89 for the
    /// two of 32 bits, 68 and 69 for the one of 16, none for the others),
    /// and the lists the textures are made with are the rest, which the
    /// host takes whole.
    #[test]
    fn castable_lists_leave_out_only_what_the_host_refuses() {
        let test = "casts::castable_lists_leave_out_only_what_the_host_refuses";
        let Some((_gpu, host)) = crate::testing::gpu() else {
            return;
        };
        let device: ID3D12Device10 = host.device.cast().expect("a device with castable formats");
        let d3dmetal = crate::testing::d3dmetal(host);
        let measured: [(u32, &[u32]); 5] = [
            (28, &[89]),
            (42, &[89]),
            (56, &[68, 69]),
            (10, &[]),
            (71, &[]),
        ];
        let mut differs = false;
        for (format, on_d3dmetal) in measured {
            let (desc, all) = (plain(format), castable(format));
            assert!(makes(&device, &desc, None), "format {format}: a texture");
            let refused: Vec<u32> = all
                .iter()
                .copied()
                .filter(|&f| !makes(&device, &desc, Some(f)))
                .collect();
            if d3dmetal {
                assert_eq!(refused, on_d3dmetal, "format {format}: as measured");
            } else {
                assert_eq!(
                    refused, [0u32; 0],
                    "format {format}: the hardware takes all"
                );
            }
            differs |= !refused.is_empty();
            // What a refused creation learns, and what textures are then
            // made with: the rest, which the host takes as one list.
            assert_eq!(
                learn(&host.device, &desc, &all),
                !refused.is_empty(),
                "format {format}"
            );
            let rest = taken(format, all.clone());
            let expected: Vec<u32> = all
                .iter()
                .copied()
                .filter(|f| !refused.contains(f))
                .collect();
            assert_eq!(rest, expected, "format {format}");
            assert!(
                !learn(&host.device, &desc, &rest),
                "format {format}: settled"
            );
            let list: Vec<DXGI_FORMAT> = rest.iter().map(|&f| DXGI_FORMAT(f as i32)).collect();
            let mut texture: Option<ID3D12Resource> = None;
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            // SAFETY: a description and an out pointer of ours.
            let made = unsafe {
                device.CreateCommittedResource3(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC1 {
                        Dimension: desc.Dimension,
                        Width: 64,
                        Height: 64,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: desc.Format,
                        SampleDesc: desc.SampleDesc,
                        ..Default::default()
                    },
                    D3D12_BARRIER_LAYOUT_COMMON,
                    None,
                    None::<&ID3D12ProtectedResourceSession>,
                    Some(&list),
                    &mut texture,
                )
            };
            assert_eq!(made, Ok(()), "format {format}: castable to the rest");
        }
        if differs {
            crate::test_support::host_differs(
                test,
                "D3DMetal refuses castable formats 89 (R10G10B10_XR_BIAS_A2_UNORM), \
                 68 (R8G8_B8G8_UNORM) and 69 (G8R8_G8B8_UNORM): textures are not castable to them",
            );
        }
    }
}
