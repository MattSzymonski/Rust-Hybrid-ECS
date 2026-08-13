//! Shared core definitions of the Pill engine.
//!
//! # Responsibilities
//!
//! - Own the semantic error system used by every workspace crate.
//! - Keep the `PillStyle` string-styling vocabulary for terminal output.
//!
//! # Design
//!
//! The error system lives in [`error`]: subsystem enums declared with
//! `#[engine_error]` compose transparently into [`error::HostError`], and
//! the diagnostics runtime renders one semantic message definition in
//! either plain or styled form. Crates import these types from here instead
//! of defining their own error-handling infrastructure.

pub mod error;
pub mod style;

pub use style::PillStyle;
