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
//! [`csharp_runtime`] owns the low-level .NET hosting boundary. The remaining
//! modules separate ABI layout, component registration, scheduled invocation
//! scope, queries, commands, and backend lifecycle. Only [`CSharpRuntime`] is
//! exposed to the parent host module.

mod abi;
mod backend;
mod commands;
mod components;
mod context;
mod csharp_runtime;
mod queries;

#[cfg(test)]
mod tests;

pub(crate) use backend::CSharpRuntime;
