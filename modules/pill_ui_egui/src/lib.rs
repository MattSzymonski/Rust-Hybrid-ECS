//! The egui UI library for the second editor frontend (W-0017).
//!
//! # Responsibilities
//!
//! - Re-exports `egui` 0.32.3 and `egui_dock` 0.17 as the one copy the egui
//!   editor's world shares: its frontend, its reloadable egui shell module
//!   and its model crate all name them through this crate.
//! - Carries the shell boundary: [`boundary::EguiShellInput`] and
//!   [`boundary::EguiShellFrame`], the plain data the frontend and the
//!   shell exchange through world resources (plan decision E7).
//! - Provides the engine-input to egui bridge ([`input`]) and the editor's
//!   egui style ([`style`]).
//! - Documents the reload-safety contract egui state follows, below.
//!
//! # Design
//!
//! A shared `rlib`+`dylib`, like `pill_ui`: one compiled copy per process,
//! so the egui `Context` and the types crossing the frontend/shell boundary
//! have one identity across every DLL (plan decision E3). It links no wgpu
//! and no winit - the frontend owns the device and draws the frame with
//! `egui-wgpu` (`test_engine_layout.py` checks this).
//!
//! The egui editor's module world (this crate plus the shell) and host
//! world (this crate plus the frontend and `egui-wgpu`) must resolve
//! `egui`, `egui_dock` and `epaint` identically; the manifest's pin block
//! requests the union, the same discipline `pill_ui` follows.
//!
//! # The reload-safety contract
//!
//! egui's `Context` can keep arbitrary typed values and callbacks alive
//! past the frame that stored them. Reloadable crates (`extensions/**`,
//! `examples/**`) must never store their own types or closures in it: after
//! a reload, the stored value's drop code or vtable points into the
//! unloaded image. In egui 0.32.3 the APIs that keep boxed `Any`/`Fn`
//! values inside the context are:
//!
//! - `Context::data` and `Context::data_mut` (the `IdTypeMap` behind them)
//! - `Context::set_request_repaint_callback`
//! - `Context::on_begin_pass` and `Context::on_end_pass` (plugins)
//! - any shape carrying `egui::PaintCallback`
//!
//! Long-lived egui state therefore lives in a world resource defined by a
//! shared crate (the egui shell's `EguiShellState`, defined in
//! `pill_editor_core_egui` and re-claimed by every shell generation);
//! transient state stays inside the frame that made it.
//! `test_coding_standards.py` flags those calls in reloadable crates.

pub use egui;
pub use egui_dock;

/// The plain data crossing the frontend/shell boundary.
pub mod boundary;
/// The engine-input to egui bridge.
pub mod input;
/// The editor's egui theme.
pub mod style;
