//! Version constants, status codes, and feature-parity bit flags.
//!
//! # Responsibilities
//!
//! - Pin the ABI revision both sides validate at load time.
//! - Define the status codes every fallible boundary call returns.
//! - Define the feature bits that guard structural parity between a host
//!   binary and the runtime dynamic library it loads.
//!
//! # Design
//!
//! `struct_size` guards catch layout drift, while [`PILL_RUNTIME_ABI_VERSION`]
//! guards intentional contract changes. Cargo features change which fields and
//! subsystems exist on either side without changing any struct layout, so they
//! need a third, independent guard: a bit mask each side computes from its own
//! compilation and compares on `create`.

// =============================================================================
// Constants
// =============================================================================

/// Revision of the host↔runtime contract.
///
/// Bumped whenever a change is not purely additive. A host refuses to load a
/// runtime reporting a different value and keeps the generation it already
/// has.
pub const PILL_RUNTIME_ABI_VERSION: u32 = 1;

/// Revision of the captured-world-state payload written by the runtime.
///
/// Independent from the ABI version because the envelope layout can evolve
/// while the function table stays untouched. A runtime refuses to restore a
/// payload written by a different format version and falls back to a fresh
/// world.
pub const PILL_RUNTIME_STATE_FORMAT_VERSION: u32 = 1;

/// Status returned by a boundary call that completed successfully.
pub const PILL_OK: i32 = 0;

/// Status returned by a boundary call that failed.
///
/// The caller reads the human-readable reason through
/// [`PillRuntimeApiV1::last_error_utf8`](crate::PillRuntimeApiV1::last_error_utf8)
/// immediately after the failing call. Richer status codes can be added
/// additively later without breaking v1 callers, which only test for
/// [`PILL_OK`].
pub const PILL_ERR: i32 = 1;

/// No project module is loaded and none should be loaded on `create`.
pub const PILL_PROJECT_BACKEND_NONE: u32 = 0;

/// The project is a native shared library exporting `project_init`.
pub const PILL_PROJECT_BACKEND_NATIVE: u32 = 1;

/// The project is a managed assembly hosted by the collectible C# loader.
pub const PILL_PROJECT_BACKEND_CSHARP: u32 = 2;

/// The `rendering` feature: window surface, GPU device, and sprite pipeline.
pub const PILL_RUNTIME_FEATURE_RENDERING: u32 = 1 << 0;

/// The `metrics` feature: repeated numerical measurements through the sink.
pub const PILL_RUNTIME_FEATURE_METRICS: u32 = 1 << 1;

/// The `profiling` feature: Tracy zones routed through the log sink.
pub const PILL_RUNTIME_FEATURE_PROFILING: u32 = 1 << 2;

/// The `dev-logs` feature: developer scratch logging macros.
pub const PILL_RUNTIME_FEATURE_DEV_LOGS: u32 = 1 << 3;

/// Every feature bit currently defined, used to reject unknown bits.
const ALL_FEATURE_BITS: u32 = PILL_RUNTIME_FEATURE_RENDERING
    | PILL_RUNTIME_FEATURE_METRICS
    | PILL_RUNTIME_FEATURE_PROFILING
    | PILL_RUNTIME_FEATURE_DEV_LOGS;

/// Human-readable name of each feature bit, ordered by bit index.
const FEATURE_NAMES: [(u32, &str); 4] = [
    (PILL_RUNTIME_FEATURE_RENDERING, "rendering"),
    (PILL_RUNTIME_FEATURE_METRICS, "metrics"),
    (PILL_RUNTIME_FEATURE_PROFILING, "profiling"),
    (PILL_RUNTIME_FEATURE_DEV_LOGS, "dev-logs"),
];

// =============================================================================
// Free Functions
// =============================================================================

/// Whether a host mask and a runtime mask describe the same build.
///
/// The comparison is exact: a runtime compiled with an extra subsystem is as
/// incompatible as one missing a required subsystem, because both sides
/// exchange data whose presence depends on the same features. Bits outside
/// [`ALL_FEATURE_BITS`] are rejected so a newer peer cannot be mistaken for a
/// compatible one.
pub fn features_are_compatible(host_features: u32, runtime_features: u32) -> bool {
    host_features & !ALL_FEATURE_BITS == 0
        && runtime_features & !ALL_FEATURE_BITS == 0
        && host_features == runtime_features
}

/// Describe how two feature masks differ, for a diagnostic message.
///
/// Returns a `(missing_in_runtime, unexpected_in_runtime)` pair of
/// comma-separated feature names. Both halves are empty when the masks match.
pub fn feature_mask_difference(host_features: u32, runtime_features: u32) -> (String, String) {
    let missing = host_feature_mask_names(host_features & !runtime_features);
    let unexpected = host_feature_mask_names(runtime_features & !host_features);
    (missing, unexpected)
}

/// Render a feature mask as a comma-separated list of feature names.
///
/// Unknown bits are reported as `unknown(0x…)` so a mask produced by a future
/// contract revision still yields a legible message.
pub fn host_feature_mask_names(features: u32) -> String {
    let mut names: Vec<&str> = Vec::new();
    for (bit, name) in FEATURE_NAMES {
        if features & bit != 0 {
            names.push(name);
        }
    }

    let unknown_bits = features & !ALL_FEATURE_BITS;
    let mut rendered = names.join(", ");
    if unknown_bits != 0 {
        if !rendered.is_empty() {
            rendered.push_str(", ");
        }
        rendered.push_str(&format!("unknown(0x{unknown_bits:08X})"));
    }
    rendered
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical masks are the only accepted combination.
    #[test]
    fn identical_feature_masks_are_compatible() {
        let mask = PILL_RUNTIME_FEATURE_RENDERING | PILL_RUNTIME_FEATURE_METRICS;
        assert!(features_are_compatible(mask, mask));
    }

    /// A runtime missing or adding a feature is rejected in both directions.
    #[test]
    fn differing_feature_masks_are_rejected() {
        assert!(!features_are_compatible(
            PILL_RUNTIME_FEATURE_RENDERING,
            PILL_RUNTIME_FEATURE_RENDERING | PILL_RUNTIME_FEATURE_METRICS
        ));
        assert!(!features_are_compatible(
            PILL_RUNTIME_FEATURE_RENDERING | PILL_RUNTIME_FEATURE_METRICS,
            PILL_RUNTIME_FEATURE_RENDERING
        ));
    }

    /// Bits from a future contract revision never validate as compatible.
    #[test]
    fn unknown_feature_bits_are_rejected() {
        let future_bit = 1 << 31;
        assert!(!features_are_compatible(future_bit, future_bit));
    }

    /// The difference report names both the missing and the extra features.
    #[test]
    fn feature_difference_names_both_directions() {
        let (missing, unexpected) = feature_mask_difference(
            PILL_RUNTIME_FEATURE_RENDERING | PILL_RUNTIME_FEATURE_PROFILING,
            PILL_RUNTIME_FEATURE_RENDERING | PILL_RUNTIME_FEATURE_METRICS,
        );
        assert_eq!(missing, "profiling");
        assert_eq!(unexpected, "metrics");
    }

    /// Unknown bits stay visible in the rendered feature list.
    #[test]
    fn unknown_bits_are_rendered_explicitly() {
        let rendered = host_feature_mask_names(PILL_RUNTIME_FEATURE_METRICS | (1 << 20));
        assert_eq!(rendered, "metrics, unknown(0x00100000)");
    }
}
