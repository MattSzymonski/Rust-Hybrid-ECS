//! The UI painter: wgpu rendering for [`pill_draw`] draw lists.
//!
//! # Responsibilities
//!
//! - [`UiRenderer`]: draws a [`pill_draw::DrawList`] with wgpu - one pipeline,
//!   an atlas texture uploaded from whole `pill_draw` textures and their
//!   patches, scissor clipping, and the external textures the set carries.
//!
//! # Design
//!
//! The drawing is plain data in, GPU work out - no global state, no window,
//! no custom events, and no knowledge of who built the lists: a game's egui
//! hands over `pill_draw` values like any other producer would. It uses the
//! renderer's own device and target, so no wgpu object crosses a DLL boundary
//! (plan decision D8).
//!
//! This was the `pill_draw_wgpu` crate until this renderer became its only
//! user; it now lives beside the overlay pass it draws after.

/// The draw-list renderer: pipeline, atlas uploads, clips, textures.
mod renderer;

pub(crate) use renderer::UiRenderer;
