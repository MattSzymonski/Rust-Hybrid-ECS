//! Dummy extension used to pad out module-loading tests.
//!
//! # Responsibilities
//!
//! - Defines the [`Tint`] struct with two dummy color-blending methods.
//! - Exposes the [`grayscale`] free function and the hot-patchable
//!   [`get_color_a`].
//! - Declares [`Tint`] and [`TestStruct`] for the managed mirror, mirrors
//!   [`get_color_a`] to C# as `pill_dummy_color.PillDummyColor.GetColorA()`,
//!   and mirrors `TestStruct::aaa` through a qualified `pill_mirror_method`
//!   marker - the module's own C#-callable surface, proving the free-function,
//!   standalone-value-type and mirrored-method codegen paths.
//! - Registers through the extension ABI when the host loads it.
//!
//! # Design
//!
//! The crate carries no ECS state; [`register`] is a no-op that only reports
//! success, kept as a plain Rust function so the same crate can also be linked
//! statically into a monolithic build.

// External crates
// `pill_module` and `Engine` must resolve in every build: the attribute is
// applied to `register` in source, and `register` is compiled everywhere.
use pill_engine::{pill_hot_fn, pill_mirror_fn, pill_module, PillMirror};
use pill_engine::{pill_mirror_impl, Engine};

// The build script scans this crate and emits one address entry per function
// into `function_inventory.rs`; the `include!` is what makes every function
// resolvable by qualified path with nothing in this file annotated.
include!(concat!(env!("OUT_DIR"), "/function_inventory.rs"));

// =============================================================================
// Struct
// =============================================================================

/// A plain value type with a mirrored method.
///
/// [`PillMirror`] puts it in the managed mirror even though no component
/// exposes it, and the impl block below adds `aaa` to the module's mirrored
/// method table - the third codegen path beside the free function and the
/// method-less value type. The method marker is spelled with its full path
/// on purpose: a qualified `pill_engine::pill_mirror_method` must register
/// exactly like the bare one.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillMirror)]
pub struct TestStruct {
    pub value: i32,
}

#[pill_mirror_impl]
impl TestStruct {
    /// A mirrored method on a mirrored value type.
    #[pill_engine::pill_mirror_method]
    pub fn aaa(&self) -> u64 {
        self.value as u64
    }
}

/// Dummy RGB color used for blending demos.
///
/// A plain value type, declared for the managed mirror: the C# codegen emits
/// it in the module's generated file even though no component exposes it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillMirror)]
pub struct Tint {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Tint {
    /// Linearly blends this color halfway towards `other`.
    pub fn mix(&self, other: Tint) -> Tint {
        Tint {
            r: (self.r + other.r) * 0.5,
            g: (self.g + other.g) * 0.5,
            b: (self.b + other.b) * 0.5,
        }
    }

    /// Inverts each channel, assuming values in the `0.0..=1.0` range.
    pub fn invert(&self) -> Tint {
        Tint {
            r: 1.0 - self.r,
            g: 1.0 - self.g,
            b: 1.0 - self.b,
        }
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Averages the channels of `tint` into a single gray value.
pub fn grayscale(tint: Tint) -> f32 {
    (tint.r + tint.g + tint.b) / 3.0
}

/// Dummy alpha channel: `Tint` carries no alpha, so this always reports fully
/// opaque, for other crates to call as a stand-in.
///
/// Mirrored to C# as `pill_dummy_color.PillDummyColor.GetColorA()`, and
/// hot-patchable like any `#[pill_hot_fn]`. The mirror attribute is listed
/// first so it captures the original signature - the contract managed code
/// compiled against - while the body stays replaceable: a patch is what the
/// managed call then executes, with no reload needed.
#[pill_mirror_fn]
#[pill_hot_fn]
pub fn get_color_a() -> f32 {
    1.0
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module against the host engine. Returns zero on success.
///
/// Must be idempotent: the host calls it once per loaded generation and rolls
/// back to the previous library when it reports a non-zero status.
/// Public so a statically linked build can call it directly, and so the
/// generated `host_module_pill_dummy_color` wrapper can carry it as
/// `pill_module_init` for the host to find in a loaded DLL.
#[pill_module]
pub fn register(_engine: &mut Engine) -> u32 {
    0
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Call `get_color_a` the way C# does: through its mirror trampoline,
    /// reading the `f32` it writes to the return buffer.
    fn managed_get_color_a() -> f32 {
        let mut ret = [0u8; pill_engine::mirror::SLOT_SIZE];
        // SAFETY: the function takes no arguments, so no slot is read, and the
        // return buffer holds the four bytes an `f32` result writes.
        let status =
            unsafe { crate::pill_mirror_fn_get_color_a(std::ptr::null(), ret.as_mut_ptr()) };
        assert_eq!(status, pill_engine::mirror::STATUS_OK);
        f32::from_le_bytes([ret[0], ret[1], ret[2], ret[3]])
    }

    /// The function must be discoverable by its qualified path, which is how a
    /// host addresses it across the ABI.
    #[test]
    fn function_is_registered_under_its_qualified_path() {
        assert!(
            pill_engine::hot_patch::plain_function_names()
                .any(|name| name == "pill_dummy_color::get_color_a"),
            "declared functions: {:?}",
            pill_engine::hot_patch::plain_function_names().collect::<Vec<_>>()
        );
    }

    /// The mirrored declarations reach this artifact's registries: the free
    /// function as a free-function descriptor at the crate root, `Tint` and
    /// `TestStruct` as standalone value types, and `TestStruct::aaa` as a
    /// mirrored method on the latter.
    #[test]
    fn the_mirrored_declarations_reach_the_registries() {
        let descriptor = pill_engine::component_registry::mirror_method_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.name == "get_color_a" && descriptor.is_free_function)
            .expect("the mirrored free function is registered");
        assert_eq!(
            descriptor.type_name, "pill_dummy_color",
            "declared at the crate root"
        );
        assert_eq!(descriptor.return_tag, "f32");
        assert!(descriptor.arg_tags.is_empty(), "no arguments to pass");

        let method = pill_engine::component_registry::mirror_method_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.name == "aaa")
            .expect("the mirrored method on TestStruct is registered");
        assert_eq!(method.type_name, "pill_dummy_color::TestStruct");
        assert!(!method.is_free_function, "a method, not a free function");
        assert_eq!(method.return_tag, "u64");
        assert_eq!(method.receiver, "ref", "`aaa` borrows its receiver");

        let declared: Vec<&str> = pill_engine::component_registry::value_type_descriptors()
            .iter()
            .map(|descriptor| descriptor.type_name)
            .collect();
        assert!(
            declared.contains(&"pill_dummy_color::Tint"),
            "Tint is declared for the managed mirror"
        );
        assert!(
            declared.contains(&"pill_dummy_color::TestStruct"),
            "TestStruct is declared for the managed mirror"
        );
    }

    /// A signature that does not match is refused, leaving the original
    /// implementation in place. This is what stops a reshaped function being
    /// installed behind call sites compiled for the old shape.
    #[test]
    fn a_mismatched_signature_is_refused() {
        fn replacement() -> f32 {
            42.0
        }
        let result = pill_engine::hot_patch::install_plain_function(
            "pill_dummy_color::get_color_a",
            replacement as *const () as usize,
            "(some other shape)",
        );
        assert!(result.is_err(), "a changed signature must be refused");
    }

    /// The dispatcher forwards to the original body, an installed replacement
    /// redirects every caller of the public name, and a reset returns the
    /// function to its own code.
    ///
    /// One test rather than three, because the slot is a process-wide `static`
    /// and the test harness runs tests on several threads: separate tests would
    /// observe each other's installs in whatever order the threads happened to
    /// interleave. Asserting the sequence in one body is what makes it
    /// deterministic.
    #[test]
    fn the_slot_dispatches_installs_and_resets() {
        fn replacement() -> f32 {
            999.0
        }

        // Read rather than hardcode: this function's whole purpose is to have
        // its value edited, so asserting the literal made the test break every
        // time someone used it for what it is for.
        let original = get_color_a();
        assert_eq!(
            managed_get_color_a(),
            original,
            "the mirror trampoline forwards to the public name"
        );

        // The recorded text, not a hand-written guess: the spelling comes from
        // `stringify!` inside the macro.
        let signature =
            pill_engine::hot_patch::plain_function_signature("pill_dummy_color::get_color_a")
                .expect("the function must be registered");

        pill_engine::hot_patch::install_plain_function(
            "pill_dummy_color::get_color_a",
            replacement as *const () as usize,
            signature,
        )
        .expect("install with the recorded signature must be accepted");
        assert_eq!(get_color_a(), 999.0, "callers must see the replacement");
        assert_eq!(
            managed_get_color_a(),
            999.0,
            "and so must the managed call path - this is what a live patch reaches"
        );

        pill_engine::hot_patch::reset_plain_function("pill_dummy_color::get_color_a")
            .expect("reset must find the registered function");
        assert_eq!(
            get_color_a(),
            original,
            "a reset must return the function to its own body"
        );
        assert_eq!(
            managed_get_color_a(),
            original,
            "the managed call path returns to the compiled body"
        );
    }
}
