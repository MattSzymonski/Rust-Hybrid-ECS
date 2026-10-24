//! The asset pack format's magic and version, shared by its writer and reader.
//!
//! # Responsibilities
//!
//! - Define the `PILLPACK` magic and the format version exactly once, for
//!   `pill_assets` (the writer, run from build scripts) and the engine's
//!   asset store (the reader, `pill_engine_core::asset_store`).
//!
//! # Design
//!
//! Deliberately dependency-free: `pill_assets` runs before anything else
//! compiles, so this crate must cost the build graph nothing, and the asset
//! store must not gain runtime dependencies through it. The pack's layout is
//! documented where it is read and written; only the two values that must
//! agree byte-for-byte live here.

/// The first bytes of every asset pack.
pub const ASSET_PACK_MAGIC: &[u8; 8] = b"PILLPACK";

/// The pack format version written and read by this workspace.
pub const ASSET_PACK_VERSION: u32 = 1;
