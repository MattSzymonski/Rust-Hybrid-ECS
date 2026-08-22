//! Constants shared between the host and optional engine modules.

/// Revision of the optional-module C-ABI export contract.
///
/// Defined here so the host and every optional module read the same value —
/// the `#[pill_module]` macro generates `pill_module_abi_version` straight
/// from this constant, so a module and host built from one workspace can never
/// drift apart. A module built against a different revision is rejected at
/// load time with a clear error instead of misbehaving silently.
pub const MODULE_ABI_VERSION: u32 = 1;
