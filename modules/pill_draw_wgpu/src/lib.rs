//! wgpu rendering for [`pill_draw`] draw lists.
//!
//! # Responsibilities
//!
//! - [`UiRenderer`]: draws a [`pill_draw::DrawList`] with wgpu - one pipeline,
//!   an atlas texture uploaded from whole `pill_draw` textures and their
//!   patches, scissor clipping, and caller-registered external textures.
//! - [`offscreen`]: a wgpu device for tests, and a render-to-pixels readback
//!   for it.
//!
//! # Design
//!
//! An `rlib` compiled into each of its users, which own their device, queue
//! and target (plan decision D8): no wgpu object crosses a DLL boundary. The
//! drawing itself is plain data in, GPU work out - no global state, no window,
//! no custom events, and no knowledge of who built the lists: a game's egui,
//! a tool and a test all hand over `pill_draw` values.
//!
//! W-0043 retired the `pill_ui` widget library this crate used to be paired
//! with, and renamed the crate with it: everything it reads arrives in the
//! frame's `pill_draw` texture set, so the glyph-atlas upload path only the
//! old editor and the golden harness drove is gone too.

/// The windowless device helper and the render-to-pixels readback.
pub mod offscreen;
/// The draw-list renderer: pipeline, atlas uploads, clips, textures.
pub mod renderer;

pub use offscreen::{render_rgba, request_device};
pub use renderer::UiRenderer;
