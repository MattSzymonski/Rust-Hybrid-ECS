//! What this binary runs: the development host, or a shipped game.
//!
//! # Responsibilities
//!
//! - Name the project type, the headless and windowed drivers, and the setup
//!   and renderer-attach steps for the posture this binary was built with.
//!
//! # Design
//!
//! The loops in [`crate::runner`] are written once, against
//! [`pill_player::FrameDriver`]; only setting a driver up differs, and that
//! difference lives here, in two mutually exclusive definitions of one private
//! module, chosen by the `dev` and `shipping` features. Exactly one posture is
//! compiled - `main.rs` refuses a build with neither or with both - and a
//! shipping build does not compile `pill_development_host` at all. Each
//! posture's windowed half exists only under the `rendering` feature.

// =============================================================================
// Development Posture
// =============================================================================

/// The development posture: `pill_development_host` builds, loads and reloads
/// the project.
#[cfg(feature = "dev")]
mod selected {
    // External crates
    use pill_core::error::HostError;
    #[cfg(feature = "rendering")]
    use pill_player::{RendererWindow, RenderingError};

    /// What the development host runs: the project and extensions it builds,
    /// watches and reloads.
    pub type Project = pill_development_host::HostConfig;
    /// The development host before a renderer window is attached.
    pub type Headless = pill_development_host::DevHost;
    /// The development host with the renderer attached to one window.
    #[cfg(feature = "rendering")]
    pub type Windowed = pill_development_host::RenderingHost;

    /// Builds and loads the project and its extensions, then starts their
    /// source watchers.
    ///
    /// # Errors
    ///
    /// Returns a typed [`HostError`] naming the failing subsystem:
    /// configuration, build, library loading, watcher startup or managed
    /// backend startup.
    pub fn setup(project: Project) -> Result<Headless, HostError> {
        pill_development_host::setup(project)
    }

    /// Loads the renderer module and attaches it to `window`.
    ///
    /// Call it after [`setup`], once a window exists, so a slow first build
    /// never shows a blank surface.
    ///
    /// # Errors
    ///
    /// Returns a [`RenderingError`]: a [`HostError`] when the renderer module
    /// cannot be built, loaded or initialized, or a `RendererError` when
    /// attaching it to the window fails.
    #[cfg(feature = "rendering")]
    pub fn attach<W: RendererWindow>(
        host: Headless,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Windowed, RenderingError> {
        pill_development_host::attach_renderer(host, window, width, height)
    }
}

// =============================================================================
// Shipping Posture
// =============================================================================

/// The shipping posture: `pill_player` runs the project the bundle links in.
#[cfg(feature = "shipping")]
mod selected {
    // External crates
    use pill_core::error::HostError;
    #[cfg(feature = "rendering")]
    use pill_player::{RendererWindow, RenderingError};

    /// What the shipping runtime runs: the project and extensions linked into
    /// this binary.
    pub type Project = pill_player::StaticProject;
    /// The engine and its frame, before a renderer window is attached.
    pub type Headless = pill_player::Runtime;
    /// The linked project running in a window.
    #[cfg(feature = "rendering")]
    pub type Windowed = pill_player::RenderingRuntime;

    /// Registers the linked extensions and project, in that order, mounting the
    /// pack the project embeds first.
    ///
    /// # Errors
    ///
    /// Returns a [`HostError`] when the embedded pack is not a pack, when an
    /// extension's or the project's entry point reports a non-zero
    /// initialization status, or when an external backend fails to start.
    pub fn setup(project: Project) -> Result<Headless, HostError> {
        pill_player::setup(project)
    }

    /// Registers the linked renderer and attaches it to `window`.
    ///
    /// The attach is asynchronous because building the renderer is: this
    /// frontend blocks on it once, before the first frame.
    ///
    /// # Errors
    ///
    /// Returns a [`RenderingError`]: the renderer's registration failed, it
    /// could not attach to the window, or this build links none.
    #[cfg(feature = "rendering")]
    pub fn attach<W: RendererWindow>(
        runtime: Headless,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Windowed, RenderingError> {
        pill_core::platform::futures::block_on(pill_player::attach_renderer(
            runtime, window, width, height,
        ))
    }
}

// =============================================================================
// Re-exports
// =============================================================================

/// The compiled posture's project type, drivers and setup steps.
pub use selected::*;
