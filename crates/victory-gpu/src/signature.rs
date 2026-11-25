//! Command signatures: the record layout of indirect execution. Kept for
//! translating it later.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, COMMAND_SIGNATURE, E_INVALIDARG, E_OUTOFMEMORY, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const SIGNATURE_IIDS: [Iid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Desc {
    stride: u32,
    count: u32,
    /// Each a type and up to three 32-bit fields.
    arguments: *const [u32; 4],
    nodes: u32,
}

#[repr(C)]
struct Signature {
    object: Object,
    stride: u32,
    arguments: Vec<[u32; 4]>,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    _root: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.arguments.is_null() && desc.count > 0 {
        return E_INVALIDARG;
    }
    // The count sizes a copy: every argument of public D3D12 takes at
    // least 4 bytes of a record, so a record of `stride` bytes has no more
    // than a quarter as many. (A zero stride says nothing.)
    if desc.stride != 0 && desc.count > desc.stride / 4 {
        return E_INVALIDARG;
    }
    let mut arguments = Vec::new();
    if desc.count > 0 {
        if arguments.try_reserve_exact(desc.count as usize).is_err() {
            return E_OUTOFMEMORY;
        }
        // SAFETY: the title passes `count` argument descriptions.
        let given = unsafe { std::slice::from_raw_parts(desc.arguments, desc.count as usize) };
        arguments.extend_from_slice(given);
    }
    tracing::debug!(stride = desc.stride, ?arguments, "command signature");
    let signature = Box::new(Signature {
        object: Object::new(&FUNCTIONS),
        stride: desc.stride,
        arguments,
    });
    object::hand_out(COMMAND_SIGNATURE, signature, iid, &SIGNATURE_IIDS, out)
}

/// A signature's record stride and arguments (type, then up to three
/// fields each).
pub(crate) fn layout<'a>(this: This) -> Option<(u32, &'a [[u32; 4]])> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our signatures.
    let s = unsafe { &*this.cast::<Signature>() };
    Some((s.stride, &s.arguments))
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &SIGNATURE_IIDS).then_some(this);
    object::answer(COMMAND_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Signature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<COMMAND_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    fn create_with(stride: u32, count: u32, arguments: *const [u32; 4]) -> (HRESULT, This) {
        let desc = Desc {
            stride,
            count,
            arguments,
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut out = null;
        let hr = create(null, &desc, null, &SIGNATURE_IIDS[0], &mut out);
        (hr, out)
    }

    #[test]
    fn signatures_keep_their_arguments() {
        // Draw, indexed draw, a constant: (type, fields).
        let arguments = [[0u32, 0, 0, 0], [1, 0, 0, 0], [5, 2, 0, 1]];
        for (stride, count) in [(16u32, 0u32), (16, 1), (40, 3)] {
            let (hr, s) = create_with(stride, count, arguments.as_ptr());
            assert_eq!(hr, S_OK, "{count} arguments");
            let (kept_stride, kept) = layout(s).unwrap();
            assert_eq!(kept_stride, stride);
            assert_eq!(kept, &arguments[..count as usize]);
            assert_eq!(slot::<extern "system" fn(This) -> u32>(s, 0x10)(s), 0);
        }
        // No arguments need no pointer.
        let (hr, s) = create_with(0, 0, std::ptr::null());
        assert_eq!(hr, S_OK);
        assert_eq!(layout(s).map(|l| l.1.len()), Some(0));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(s, 0x10)(s), 0);
        assert!(layout(std::ptr::null()).is_none());
    }

    /// The argument count is the title's: one without arguments behind
    /// it, or more arguments than the record has room for, is refused
    /// before anything is read or allocated for it.
    #[test]
    fn signatures_with_impossible_counts_are_refused() {
        let one = [[0u32; 4]];
        let null: This = std::ptr::null();
        let out = |r: (HRESULT, This)| (r.0, r.1.is_null());
        assert_eq!(
            out(create_with(16, 1, std::ptr::null())),
            (E_INVALIDARG, true)
        );
        assert_eq!(
            out(create_with(16, u32::MAX, one.as_ptr())),
            (E_INVALIDARG, true)
        );
        assert_eq!(out(create_with(16, 5, one.as_ptr())), (E_INVALIDARG, true));
        assert_eq!(
            out(create_with(u32::MAX, u32::MAX, one.as_ptr())),
            (E_INVALIDARG, true)
        );
        let mut s = null;
        assert_eq!(
            create(null, std::ptr::null(), null, &SIGNATURE_IIDS[0], &mut s),
            E_INVALIDARG
        );
    }
}
