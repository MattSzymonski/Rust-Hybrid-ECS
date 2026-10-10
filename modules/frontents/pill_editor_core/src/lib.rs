//! The editor's UI-independent core: the dock layout model and the editor's
//! view of the engine.
//!
//! # Responsibilities
//!
//! - Own the serializable dock layout: model, actions, the validating reducer,
//!   geometry and on-disk persistence ([`layout`]).
//! - Own the editor's snapshots of the engine and the commands it applies at
//!   the frame boundary ([`editor_state`]).
//! - Own the clipboard payload documents the copy and paste commands share
//!   ([`clipboard`]).
//! - Declare the layout persistence failures ([`error`]).
//!
//! # Design
//!
//! Nothing here depends on a UI framework, a window or a GPU: the frozen
//! Dioxus editor and the native editor use the same code. The crate is a
//! shared `dylib` (see its manifest) because its values cross between the
//! editor executable and the reloadable editor shell module.

/// The clipboard payload documents the copy and paste commands share.
pub mod clipboard;
/// The command registry: named actions, menus and rebindable shortcuts.
pub mod commands;
/// The editor's snapshots of the engine and the commands it applies to it.
pub mod editor_state;
/// Layout persistence failures.
pub mod error;
/// The undo and redo history of editor edits.
pub mod history;
/// Serializable dock model, validated mutations, and deterministic geometry.
pub mod layout;
/// Build diagnostics: the problems the Problems panel shows.
pub mod problems;
