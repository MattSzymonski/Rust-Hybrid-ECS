//! The master renderer's draw paths.
//!
//! # Responsibilities
//!
//! - One module per draw path; [`mesh_drawer`] batches a frame's queued
//!   instances and records their instanced draws inside the render pass.

/// The mesh drawer: batches queued entities and records the instanced draws.
pub mod mesh_drawer;
