//! Shared helpers for the Hybrid ECS engine's Criterion benchmarks.
//!
//! This module is intended to hold common fixtures - component type
//! definitions, world builders, and data generators - reused across the
//! benchmark files in this directory so each `[[bench]]` target stays
//! focused on the subsystem it measures.
//!
//! # Responsibilities
//!
//! - Provide reusable benchmark fixtures (component types, world builders).
//! - Keep benchmark boilerplate in one place instead of duplicating it in
//!   every harness file.
//!
//! # Design
//!
//! Each benchmark file in `engine/benches/` is a standalone Criterion
//! harness (`harness = false`) declared in `engine/Cargo.toml`. This module
//! is not itself a bench target; benchmark files can pull it in with
//! `mod common;`. It currently contains no code because no harness
//! references it yet - it is a placeholder awaiting shared helpers to be
//! extracted from the existing benchmark files.
