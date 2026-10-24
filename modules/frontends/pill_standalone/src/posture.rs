//! What this binary runs: the development host, or a shipped game.
//!
//! # Responsibilities
//!
//! - Name the project type, the headless and windowed drivers, and the two
//!   setup steps for the posture this binary was built with.
//!
//! # Design
//!
//! The loops in [`crate::runner`] are written once, against
//! [`pill_runtime::FrameDriver`]; only setting a driver up differs, and that
//! difference lives here, in two sibling modules selected by the `dev` and
//! `shipping` features. A shipping build compiles only the second, which
//! names nothing from `pill_host`.

/// The development posture: `pill_host` builds, loads and reloads the project.
#[cfg(feature = "dev")]
mod selected {
    // External crates
    use pill_core::error::HostError;
    #[cfg(feature = "rendering")]
    use pill_runtime::{RendererWindow, RenderingError};

    /// What says which project to run: its settings.
    pub type Project = pill_host::HostConfig;
    /// The headless driver.
    pub type Headless = pill_host::DevHost;
    /// The windowed driver.
    #[cfg(feature = "rendering")]
    pub type Windowed = pill_host::RenderingHost;

    /// Build, load and start watching the project and its extensions.
    ///
    /// # Errors
    ///
    /// Returns the host's setup failure.
    pub fn setup(project: Project) -> Result<Headless, HostError> {
        pill_host::setup(project)
    }

    /// Load the renderer module and attach it to `window`.
    ///
    /// # Errors
    ///
    /// Returns the renderer module's or the attach's failure.
    #[cfg(feature = "rendering")]
    pub fn attach<W: RendererWindow>(
        host: Headless,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Windowed, RenderingError> {
        pill_host::attach_renderer(host, window, width, height)
    }
}

/// The shipping posture: `pill_runtime` runs the project the bundle links in.
#[cfg(feature = "shipping")]
mod selected {
    // External crates
    use pill_core::error::HostError;
    #[cfg(feature = "rendering")]
    use pill_runtime::{RendererWindow, RenderingError};

    /// What says which project to run: the linked project.
    pub type Project = pill_runtime::StaticProject;
    /// The headless driver.
    pub type Headless = pill_runtime::Runtime;
    /// The windowed driver.
    #[cfg(feature = "rendering")]
    pub type Windowed = pill_runtime::RenderingRuntime;

    /// Register the linked extensions and project.
    ///
    /// # Errors
    ///
    /// Returns the first entry point's failure.
    pub fn setup(project: Project) -> Result<Headless, HostError> {
        pill_runtime::setup(project)
    }

    /// Register the linked renderer and attach it to `window`.
    ///
    /// The attach is asynchronous, for a browser's sake; a desktop frontend
    /// blocks on it, once, before the first frame.
    ///
    /// # Errors
    ///
    /// Returns the renderer's registration or attach failure.
    #[cfg(feature = "rendering")]
    pub fn attach<W: RendererWindow>(
        runtime: Headless,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Windowed, RenderingError> {
        pill_core::platform::futures::block_on(pill_runtime::attach_renderer(
            runtime, window, width, height,
        ))
    }
}

pub use selected::*;
