//! wgpu rendering for [`pill_ui`] draw lists.
//!
//! # Responsibilities
//!
//! - [`UiRenderer`]: draws a [`pill_ui::DrawList`] with wgpu - one pipeline, an
//!   atlas texture uploaded from the atlas's dirty regions, scissor clipping,
//!   and caller-registered external textures.
//! - [`offscreen`]: a wgpu device for tests and the golden harness, and a
//!   render-to-pixels readback for it.
//!
//! # Design
//!
//! An `rlib` compiled into each of its users, which own their device, queue
//! and target (plan decision D8): no wgpu object crosses a DLL boundary. The
//! drawing itself is plain data in, GPU work out - no global state, no window,
//! no custom events.
//!
//! Golden images (plan decision D26, level 2) render the widget gallery and a
//! sample panel offscreen and compare them to the stored images under
//! `goldens/`:
//!
//! ```text
//! cd modules
//! cargo run -p pill_ui_wgpu --example golden              # compare
//! cargo run -p pill_ui_wgpu --example golden -- --update  # regenerate after an intended visual change
//! ```

/// The windowless device helper and the render-to-pixels readback.
pub mod offscreen;
/// The draw-list renderer: pipeline, atlas uploads, clips, textures.
pub mod renderer;

pub use offscreen::{render_rgba, request_device};
pub use renderer::UiRenderer;
