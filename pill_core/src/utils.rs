//! Small general-purpose helpers shared by Pill crates.
//!
//! # Responsibilities
//!
//! - Reflect type and enum-variant names for diagnostics and tooling.
//! - Validate asset paths against a whitelist of file formats.
//! - Construct and inspect fixed-width bitmasks.
//! - Format project-level error chains for reporting.
//!
//! # Design
//!
//! Pure helpers with no engine state. The single error type,
//! [`AssetPathError`], is defined locally with `thiserror` so callers can
//! match on variants without depending on an engine crate.

// Standard library
use std::{
    any::type_name,
    fmt::Binary,
    ops::{Add, Not, Shl, Sub},
    path::Path,
};

// =============================================================================
// Type-name reflection
// =============================================================================

/// Short name of `T` with any module path stripped.
///
/// # Examples
///
/// ```
/// use pill_core::utils::get_type_name;
///
/// assert_eq!(get_type_name::<Vec<u8>>(), "Vec<u8>");
/// assert_eq!(get_type_name::<u32>(), "u32");
/// ```
pub fn get_type_name<T>() -> String {
    let full_type_name = type_name::<T>();
    full_type_name
        .rsplit_once(':')
        .map_or(full_type_name, |(_, name)| name)
        .to_string()
}

/// Short name of the type of `value`, with any module path stripped.
///
/// # Examples
///
/// ```
/// use pill_core::utils::get_value_type_name;
///
/// let value = 42u32;
/// assert_eq!(get_value_type_name(&value), "u32");
/// ```
pub fn get_value_type_name<T>(_: &T) -> String {
    let full_type_name = type_name::<T>();
    full_type_name
        .rsplit_once(':')
        .map_or(full_type_name, |(_, name)| name)
        .to_string()
}

/// Whether two references point to the same enum variant.
///
/// Compares [`std::mem::discriminant`], so payload values are ignored.
///
/// # Examples
///
/// ```
/// use pill_core::utils::enum_variant_eq;
///
/// #[derive(Debug)]
/// enum Choice {
///     First,
///     Second,
/// }
///
/// assert!(enum_variant_eq(&Choice::First, &Choice::First));
/// assert!(!enum_variant_eq(&Choice::First, &Choice::Second));
/// ```
pub fn enum_variant_eq<T>(a: &T, b: &T) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Name of the enum variant that `value` currently holds.
///
/// Uses the `Debug` representation, so `value` must implement `Debug`.
///
/// # Examples
///
/// ```
/// use pill_core::utils::get_enum_variant_type_name;
///
/// #[derive(Debug)]
/// enum Shape {
///     Circle(f32),
///     Square,
/// }
///
/// assert_eq!(get_enum_variant_type_name(&Shape::Circle(2.0)), "Circle");
/// assert_eq!(get_enum_variant_type_name(&Shape::Square), "Square");
/// ```
pub fn get_enum_variant_type_name<T: core::fmt::Debug>(value: &T) -> String {
    let full_type_name = format!("{value:?}");
    let name = full_type_name.split('(').next().unwrap_or(&full_type_name);
    name.trim().to_string()
}

// =============================================================================
// Asset path validation
// =============================================================================

/// Error returned by [`validate_asset_path`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetPathError {
    /// The path does not exist or carries no file extension.
    #[error("asset path is not usable: {path}")]
    InvalidPath {
        /// The offending path, for diagnostics.
        path: String,
    },
    /// The file extension is not among the allowed formats.
    #[error("asset format `{extension}` is not allowed (expected one of: {allowed})")]
    InvalidFormat {
        /// The file extension that was rejected.
        extension: String,
        /// Comma-separated list of allowed formats.
        allowed: String,
    },
}

/// Check that an asset path exists and has a whitelisted file extension.
///
/// # Errors
///
/// Returns [`AssetPathError::InvalidPath`] when `path` does not exist or has
/// no extension, and [`AssetPathError::InvalidFormat`] when the extension is
/// not listed in `allowed_formats`.
///
/// # Examples
///
/// ```
/// use pill_core::utils::{validate_asset_path, AssetPathError};
///
/// let result = validate_asset_path(
///     std::path::Path::new("assets/scene.ron"),
///     &["ron", "json"],
/// );
/// // The example file does not exist, so the path itself is rejected.
/// assert!(matches!(result, Err(AssetPathError::InvalidPath { .. })));
/// ```
pub fn validate_asset_path(
    path: &Path,
    allowed_formats: &'static [&'static str],
) -> Result<(), AssetPathError> {
    if !path.exists() {
        return Err(AssetPathError::InvalidPath {
            path: path.display().to_string(),
        });
    }

    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if allowed_formats.contains(&extension) => Ok(()),
        Some(extension) => Err(AssetPathError::InvalidFormat {
            extension: extension.to_string(),
            allowed: allowed_formats.join(", "),
        }),
        None => Err(AssetPathError::InvalidPath {
            path: path.display().to_string(),
        }),
    }
}

// =============================================================================
// Error-chain formatting
// =============================================================================

/// Format an error and its full source chain into a single message.
///
/// The first line is the top-level error; every [`std::error::Error::source`]
/// is indented below it in order.
///
/// # Examples
///
/// ```
/// use pill_core::utils::get_project_error_message;
///
/// #[derive(Debug)]
/// struct ProjectError;
///
/// impl std::fmt::Display for ProjectError {
///     fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
///         formatter.write_str("project exploded")
///     }
/// }
///
/// impl std::error::Error for ProjectError {}
///
/// let result: Result<(), ProjectError> = Err(ProjectError);
/// let message = get_project_error_message(result).expect("error should format");
/// assert!(message.contains("project exploded"));
/// ```
pub fn get_project_error_message<E: std::error::Error>(result: Result<(), E>) -> Option<String> {
    result.err().map(|error| {
        let mut message = format!("Pill project error: {error}\n");
        let mut source = error.source();
        let mut index = 0usize;
        while let Some(inner_source) = source {
            message.push_str(&format!("  {index}: {inner_source}\n"));
            index += 1;
            source = inner_source.source();
        }
        message
    })
}

/// Generate a C-ABI project entry point plus an in-process constructor.
///
/// The macro expands to an `extern "C" fn get_project()` returning a raw
/// pointer to a boxed project, and a plain Rust `create_project()` that
/// returns the boxed project for embedding runtimes.
#[macro_export]
macro_rules! create_project {
    ($project_constructor:expr, $project_trait:path) => {
        #[no_mangle]
        pub extern "C" fn get_project() -> *mut std::ffi::c_void {
            let project: Box<dyn $project_trait> = Box::new($project_constructor);
            Box::into_raw(Box::new(project)) as *mut std::ffi::c_void
        }

        /// Returns a boxed project for embedding runtimes.
        pub fn create_project() -> Box<dyn $project_trait> {
            Box::new($project_constructor)
        }
    };
}

// =============================================================================
// Bitmask helpers
// =============================================================================

/// Build a bitmask of `T`'s width with one contiguous run of set bits.
///
/// Bits are numbered from the most significant bit (MSB, index 0) to the
/// least significant. `mask_range` selects the run of bits to set.
///
/// # Examples
///
/// ```
/// use pill_core::utils::create_bitmask_from_range;
///
/// let mask = create_bitmask_from_range::<u16>(&(0..4));
/// assert_eq!(mask, 0b1111_1000_0000_0000);
/// ```
///
/// # Panics
///
/// Panics when `mask_range.end` reaches or exceeds the bit width of `T`.
pub fn create_bitmask_from_range<T>(mask_range: &core::ops::Range<T>) -> T
where
    T: Copy
        + Default
        + Binary
        + From<u8>
        + Ord
        + Shl<Output = T>
        + Sub<Output = T>
        + Add<Output = T>
        + Not<Output = T>,
{
    let mask_size = T::from(std::mem::size_of::<T>() as u8 * 8);

    if mask_range.end >= mask_size {
        panic!("Provided mask range exceeds mask size");
    }

    let range_length: T = mask_range.end - mask_range.start + T::from(1);
    let mask_shift = mask_size - mask_range.end - T::from(1);

    match range_length == mask_size {
        // Prevent overflow (when shifting by the same value as the bit width).
        true => !(T::from(0)) << mask_shift,
        false => !(!T::from(0) << range_length) << mask_shift,
    }
}

/// Build a `u16` bitmask with a single set bit at `index`.
///
/// Bits are numbered from the most significant bit (index 0) to the least
/// significant (index 15). Indexes outside `0..=15` produce a zero mask.
///
/// # Examples
///
/// ```
/// use pill_core::utils::create_bitmask_with_one;
///
/// assert_eq!(create_bitmask_with_one(3), 0b0001_0000_0000_0000);
/// assert_eq!(create_bitmask_with_one(16), 0b0000_0000_0000_0000);
/// ```
pub fn create_bitmask_with_one(index: u16) -> u16 {
    pub const FIRST_BIT: u16 = 0b1000_0000_0000_0000;
    let mut mask: u16 = 0b0000_0000_0000_0000;
    if (0_u16..=15_u16).contains(&index) {
        mask |= FIRST_BIT;
        for _ in 0..index {
            mask >>= 1;
        }
    }
    mask
}

/// Indexes (from the MSB) of every set bit in a `u16` bitmask.
///
/// # Examples
///
/// ```
/// use pill_core::utils::get_indices_of_set_elements;
///
/// assert_eq!(
///     get_indices_of_set_elements(0b0001_0000_0100_0001),
///     vec![3, 9, 15],
/// );
/// ```
pub fn get_indices_of_set_elements(bitmask: u16) -> Vec<usize> {
    let mut test_mask: u16 = 0b1000_0000_0000_0000;
    let mut indices = Vec::<usize>::new();
    for i in 0..=15 {
        if bitmask & test_mask > 0 {
            indices.push(i);
        }
        test_mask >>= 1;
    }
    indices
}
