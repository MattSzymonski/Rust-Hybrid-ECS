//! The source scanner, shared with every crate's build script.
//!
//! The implementation lives in `pill_hot_scan` because the host and the build
//! scripts must agree byte for byte about where a function starts and what its
//! declaration says. They previously had separate scanners, and the moment they
//! disagreed - a build script naming a method through its type while the host did
//! not - every method silently failed to patch.
//!
//! Re-exported rather than aliased so existing `source::` call sites read the
//! same as before.

pub use pill_hot_scan::*;
