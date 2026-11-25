//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees, `external` host
//! libraries recording into title lists. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod bindless;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod clamp_bytecode;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(windows)]
pub mod external;
pub mod fence;
pub mod frame;
mod holders;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
pub mod list;
#[cfg(windows)]
mod memory;
pub mod object;
pub mod pipeline;
mod primitive;
pub mod query;
pub mod queue;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shadow;
pub mod signature;
mod states;
mod switch;
#[cfg(windows)]
mod tdr;
pub mod test_support;
#[cfg(test)]
mod testing;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod twin;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use watch::{
    Writing as KernelWrite, title_memory_aliased, title_memory_remapped,
    writing as kernel_will_write,
};

/// Whether there is a host D3D12 device for command lists to run on. The
/// first call makes the device, as the first use by a title would; where
/// the host has no D3D12 at all (any system but Windows or Wine) there is
/// none. Tests that need the device ask through [`test_support::gpu`].
pub fn host_available() -> bool {
    #[cfg(windows)]
    {
        host::host().is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}
