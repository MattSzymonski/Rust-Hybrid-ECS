//! Standalone-host error composition around the shared host error system.
//!
//! # Responsibilities
//!
//! - Compose every [`pill_host::error::HostError`] subsystem failure.
//! - Declare the windowing- and GPU-specific failures that only the
//!   standalone windowed mode can produce.
//!
//! # Design
//!
//! The host crate owns the semantic diagnostics runtime; this enum only
//! adds the top-level windowing variants and delegates rendering through
//! the same runtime. The binary entry point converts the final
//! [`StandaloneError`] into a styled miette report exactly once.

// External crates
use pill_core_macros::engine_error;

#[engine_error(namespace = standalone, runtime = ::pill_core::error)]
pub enum StandaloneError {
    /// A subsystem of the shared host crate failed.
    #[transparent]
    Host(#[from] ::pill_core::error::HostError),

    /// The `winit` event loop could not be created.
    #[cfg(feature = "rendering")]
    #[message("failed to create the event loop")]
    EventLoopCreation {
        #[source]
        source: winit::error::EventLoopError,
    },

    /// The native window could not be created.
    #[cfg(feature = "rendering")]
    #[message("failed to create the native window")]
    WindowCreation {
        #[source]
        source: winit::error::OsError,
    },

    /// The GPU surface could not be created for the window.
    #[cfg(feature = "rendering")]
    #[message("failed to create the GPU surface")]
    SurfaceCreation {
        #[source]
        source: wgpu::CreateSurfaceError,
    },

    /// No GPU adapter could satisfy the surface request.
    #[cfg(feature = "rendering")]
    #[message("no GPU adapter found; is a Vulkan/Metal/DX12 driver available?")]
    AdapterUnavailable {
        #[source]
        source: wgpu::RequestAdapterError,
    },

    /// The GPU device could not be created from the adapter.
    #[cfg(feature = "rendering")]
    #[message("failed to create the GPU device")]
    DeviceCreation {
        #[source]
        source: wgpu::RequestDeviceError,
    },
}
