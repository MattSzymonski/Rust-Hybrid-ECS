//! Scheduler-aware C# backend for the native game host.
//!
//! # Responsibilities
//!
//! - Starts the .NET runtime used by managed gameplay assemblies.
//! - Exposes native ECS component storage to managed query iterators.
//! - Converts reflected C# query access into Rust scheduler metadata.
//!
//! # Design
//!
//! [`cs_runtime`] owns the low-level .NET hosting boundary, while [`cs_api`]
//! owns the ECS-specific ABI and managed-system registration. Only
//! [`CSharpRuntime`] is exposed to the parent host module.

mod cs_api;
mod cs_runtime;

pub(crate) use cs_api::CSharpRuntime;
