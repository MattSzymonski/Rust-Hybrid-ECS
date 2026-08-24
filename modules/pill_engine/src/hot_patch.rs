//! Per-function hot patching: stable dispatch slots for registered systems.
//!
//! # Responsibilities
//!
//! - Defines [`HotSlot`], the stable indirection a registered system dispatches
//!   through, so its implementation can be replaced without re-registering it.
//! - Defines [`HotPatchRegistry`], the engine-owned map from system name to slot.
//! - Computes [`signature_hash`], the gate that refuses a patch whose ABI no
//!   longer matches the running system.
//! - Provides [`verify_abi`], a startup self-check for the one platform
//!   assumption the design rests on.
//!
//! # Design
//!
//! A system's boxed closure calls the user's function through an address held in
//! a slot. Replacing the implementation is a single atomic store, so the
//! `Box<dyn System>`, its vtable, the scheduler graph, the system's access
//! metadata and its change-detection ticks all stay exactly where they are. The
//! `World` is never touched.
//!
//! The address is type-erased because `F` differs per system and a registry
//! cannot be typed over all of them. The transmute back happens inside the
//! monomorphized closure in [`crate::system`], where `F` and `Input` are both
//! known.
//!
//! ## The ZST stand-in
//!
//! A slot holds the address of a `fn(&mut F, Input) -> F::Output`, where `F` is
//! the zero-sized fn-item type of the user's system. A separately compiled patch
//! cannot name that type, so it declares the parameter as `&mut ()` instead.
//! That substitution is sound because a reference to any zero-sized type is a
//! single non-null pointer-sized value, and the callee never reads it.
//!
//! It is an ABI assumption rather than a guarantee Rust makes, so
//! [`verify_abi`] checks it at startup in debug builds: a toolchain or target
//! that breaks it fails loudly at initialization instead of silently corrupting
//! a system's arguments at runtime.
//!
//! ## Feature gating
//!
//! The dispatch behavior is gated behind the `hot_patch` feature, but
//! [`HotPatchRegistry`] is a field on [`Engine`](crate::Engine)
//! **unconditionally**. Gating the field would change `Engine`'s layout between
//! feature configurations, and a module built with the other configuration
//! reconstructs `&mut Engine` from a raw pointer - so a layout difference is
//! memory corruption, not a compile error. An unused empty map costs a few bytes
//! once per engine and nothing at runtime; a layout divergence costs
//! correctness. The same reasoning is why `rendering` is mirrored into every
//! module build.

// Standard library
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

// Current crate
use crate::system::{SystemParam, SystemParamFunction};

// =============================================================================
// Errors
// =============================================================================

/// Why a hot patch was refused.
///
/// Every variant is a refusal, never a partial application: the running system
/// is untouched whenever one of these is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotPatchError {
    /// No system was registered under this name, or its slot has been cleared.
    UnknownSystem {
        /// The name the caller asked to patch.
        name: String,
    },
    /// The replacement's signature no longer matches the running system's.
    ///
    /// This is the gate that keeps a changed parameter list or return type from
    /// being installed behind a call site compiled for the old one.
    SignatureMismatch {
        /// The name of the system whose patch was refused.
        name: String,
        /// Hash the running system was registered with.
        expected: u64,
        /// Hash the replacement reported.
        found: u64,
    },
    /// The replacement address was null.
    NullAddress {
        /// The name of the system whose patch was refused.
        name: String,
    },
}

impl std::fmt::Display for HotPatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSystem { name } => {
                write!(formatter, "no hot-patchable system registered as `{name}`")
            }
            Self::SignatureMismatch {
                name,
                expected,
                found,
            } => write!(
                formatter,
                "signature changed for `{name}` (registered {expected:#018x}, patch {found:#018x})"
            ),
            Self::NullAddress { name } => {
                write!(formatter, "replacement address for `{name}` is null")
            }
        }
    }
}

impl std::error::Error for HotPatchError {}

// =============================================================================
// HotSlot
// =============================================================================

/// The stable indirection one registered system dispatches through.
///
/// Created when the system is registered and owned jointly by the system's
/// closure and the engine's [`HotPatchRegistry`], so a patch installed through
/// the registry is observed by the closure on its next invocation.
#[derive(Debug)]
pub struct HotSlot {
    /// Address of the currently active
    /// `fn(&mut F, Input) -> F::Output` monomorphization.
    current: AtomicUsize,
    /// Signature identity of the system this slot was created for.
    signature_hash: AtomicU64,
    /// The implementation recorded at registration, kept so a patch can be
    /// rolled back to the code the running artifact was actually built with.
    ///
    /// Separate from `current` because `current` is overwritten by every
    /// install, and without this the original address would be unrecoverable
    /// after the first patch.
    original: AtomicUsize,
}

impl HotSlot {
    /// An empty slot, filled by [`Self::initialize`] during registration.
    pub fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            signature_hash: AtomicU64::new(0),
            original: AtomicUsize::new(0),
        }
    }

    /// Record the baseline implementation and the signature to gate against.
    ///
    /// Called once, from the boxing code in [`crate::system`], where the
    /// concrete `F` and `Input` are known.
    pub fn initialize(&self, address: usize, signature_hash: u64) {
        self.current.store(address, Ordering::Release);
        self.signature_hash.store(signature_hash, Ordering::Release);
        // Unconditionally, not only when unset: a reload builds a fresh slot
        // for the new artifact, and its baseline is that artifact's code.
        self.original.store(address, Ordering::Release);
    }

    /// The implementation recorded at registration.
    ///
    /// This is what a rollback to generation zero installs, and it stays valid
    /// for as long as the artifact that registered the system is loaded - which
    /// the host guarantees by never unmapping a retired generation.
    pub fn original(&self) -> usize {
        self.original.load(Ordering::Acquire)
    }

    /// The implementation to call now.
    ///
    /// `Acquire` pairs with the `Release` in [`Self::install`] so a freshly
    /// loaded patch library's code is visible to whichever thread runs the
    /// system next. x86-64 is total-store-ordered and would tolerate a relaxed
    /// load; aarch64 would not, and the engine targets both.
    #[inline]
    pub fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    /// Signature identity this slot was registered with.
    pub fn signature_hash(&self) -> u64 {
        self.signature_hash.load(Ordering::Acquire)
    }

    /// Replace the implementation, refusing anything whose signature moved.
    ///
    /// # Errors
    ///
    /// Returns [`HotPatchError::NullAddress`] for a null replacement and
    /// [`HotPatchError::SignatureMismatch`] when `signature_hash` differs from
    /// the one recorded at registration. On either, the running implementation
    /// is left in place.
    pub fn install(
        &self,
        name: &str,
        address: usize,
        signature_hash: u64,
    ) -> Result<(), HotPatchError> {
        if address == 0 {
            return Err(HotPatchError::NullAddress {
                name: name.to_string(),
            });
        }
        let expected = self.signature_hash.load(Ordering::Acquire);
        if expected != signature_hash {
            return Err(HotPatchError::SignatureMismatch {
                name: name.to_string(),
                expected,
                found: signature_hash,
            });
        }
        self.current.store(address, Ordering::Release);
        Ok(())
    }
}

impl Default for HotSlot {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// HotPatchRegistry
// =============================================================================

/// Engine-owned map from registered system name to its dispatch slot.
///
/// Lives on the [`Engine`](crate::Engine) rather than in a global, because
/// `pill_engine` is an rlib: a global would be duplicated per loaded artifact,
/// so slots created inside a project DLL would be invisible to the host.
#[derive(Debug, Default)]
pub struct HotPatchRegistry {
    slots: HashMap<String, Arc<HotSlot>>,
}

impl HotPatchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Record a system's slot under its registration name.
    ///
    /// A repeated name replaces the previous entry, matching the engine's
    /// existing behavior of allowing duplicate system names.
    pub fn insert(&mut self, name: impl Into<String>, slot: Arc<HotSlot>) {
        self.slots.insert(name.into(), slot);
    }

    /// Forget one system's slot, so a cleared system can never be patched.
    ///
    /// Every entry referring to the same slot is dropped, not just the one
    /// matching `name`. A system is registered under both its display name and
    /// its function path, and leaving either behind would let a patch install
    /// into a system the scheduler has already dropped.
    pub fn remove(&mut self, name: &str) {
        let Some(target) = self.slots.get(name).cloned() else {
            return;
        };
        self.slots.retain(|_, slot| !Arc::ptr_eq(slot, &target));
    }

    /// The slot registered for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<HotSlot>> {
        self.slots.get(name)
    }

    /// Names of every hot-patchable system, for diagnostics and tooling.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.slots.keys().map(|name| name.as_str())
    }

    /// How many systems are hot-patchable.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no system is hot-patchable.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

// =============================================================================
// Signature identity
// =============================================================================

/// Stable identity of a system's call signature.
///
/// Derived from the parameter tuple and return type names, which is exactly
/// what changes when a signature changes. Computed once per registration, so
/// its cost never reaches the frame loop.
///
/// Type names rather than [`TypeId`](std::any::TypeId) because a patch is a
/// separately compiled artifact: it must be able to compute the same value from
/// the same source text, and `type_name` is what both sides can agree on.
pub fn signature_hash<F, Input>() -> u64
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::any::type_name::<Input>().hash(&mut hasher);
    std::any::type_name::<F::Output>().hash(&mut hasher);
    hasher.finish()
}

/// Signature identity of a system, inferred from the function itself.
///
/// The turbofish form of [`signature_hash`] cannot infer a function-item type
/// from nothing, so this takes the function by reference instead. Convenient
/// wherever the function is in scope as a value.
pub fn signature_hash_of<F, Input>(_function: &F) -> u64
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    signature_hash::<F, Input>()
}

/// The fully-qualified path of a system function, as `#[pill_hot]` names it.
///
/// A function-item type's name is its path - `project::physics_system` - which
/// is exactly what `module_path!() + "::" + fn name` produces in the attribute.
/// That agreement is what lets the host patch by the function's real path even
/// though [`Engine::register_system`](crate::Engine::register_system) also
/// records a separate, arbitrary display name (`"ball_physics"`).
///
/// Returns `None` for anything that is not a plain function item - a closure,
/// for instance, whose `type_name` is a compiler-generated placeholder with no
/// stable path a patch could name.
pub fn function_path<F>() -> Option<String> {
    let name = std::any::type_name::<F>();
    // Closures render as `crate::outer::{{closure}}`, and generic instantiations
    // carry angle brackets; neither is a name a patch can be keyed by.
    if name.contains('{') || name.contains('<') || !name.contains("::") {
        return None;
    }
    Some(name.to_string())
}

/// Address of the dispatch entry point for a locally compiled system function.
///
/// This is the value [`HotSlot::install`] expects. A real patch resolves the
/// equivalent address from a freshly compiled library's exports instead; this
/// helper covers the in-process case, which is what tests and any statically
/// linked replacement need.
pub fn local_implementation_address<F, Input>(_function: &F) -> usize
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    <F as SystemParamFunction<Input>>::run as *const () as usize
}

// =============================================================================
// Plain-function slots
// =============================================================================

/// A dispatch slot for an ordinary function marked `#[pill_hot_fn]`.
///
/// Systems get their indirection for free: the engine holds their boxed closure
/// and can swap what it calls. An ordinary `pub fn` has no such holder - callers
/// call it directly - so the indirection has to live inside the function itself.
/// `#[pill_hot_fn]` renames the real body and turns the public name into a
/// dispatcher that reads this slot.
///
/// Unlike [`HotSlot`], this is a `static` inside whichever artifact compiled the
/// function, not a registry entry on the engine. That is deliberate: a crate
/// linked into several artifacts has an independent copy of its code in each,
/// and each copy's callers must be redirected separately.
#[derive(Debug)]
pub struct PlainSlot {
    /// Installed replacement, or zero while the original is still current.
    current: AtomicUsize,
}

impl PlainSlot {
    /// A slot holding no replacement.
    pub const fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
        }
    }

    /// The address to call: the installed replacement, or `original`.
    ///
    /// `Relaxed` would be enough on x86-64, but the acquire pairs with
    /// `install`'s release so a freshly loaded patch's code is visible to
    /// whichever thread calls next on weaker orderings too.
    #[inline]
    pub fn current_or(&self, original: usize) -> usize {
        match self.current.load(Ordering::Acquire) {
            0 => original,
            installed => installed,
        }
    }

    /// Install a replacement implementation.
    pub fn install(&self, address: usize) {
        self.current.store(address, Ordering::Release);
    }

    /// The installed replacement, or zero when the slot has never been filled.
    ///
    /// Callers that own their fallback inline - a dispatcher generated around a
    /// method body, which has no separate implementation to name - branch on
    /// this rather than passing an address to [`Self::current_or`].
    #[inline]
    pub fn installed(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    /// Empty the slot, so calls fall back to the body compiled into this
    /// artifact.
    ///
    /// A plain function has no single baseline address the host could reinstall:
    /// every artifact linking the crate holds its own copy. Emptying the slot is
    /// how each of them returns to its own original code.
    pub fn reset(&self) {
        self.current.store(0, Ordering::Release);
    }

    /// Whether a replacement is currently installed.
    pub fn is_patched(&self) -> bool {
        self.current.load(Ordering::Acquire) != 0
    }
}

impl Default for PlainSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// One `#[pill_hot_fn]` declared in this artifact.
pub struct PillHotSlotDescriptor {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// The dispatcher's slot, so a host can redirect it.
    pub slot: &'static PlainSlot,
    /// The signature as written, used as the compatibility gate.
    ///
    /// Text rather than a `TypeId`, because a patch is a separately compiled
    /// artifact and both sides must derive the same value from the same source.
    pub signature: &'static str,
    /// Address of the body itself, reached through a fn pointer because casting
    /// a function to `usize` is not permitted in the constant context that
    /// builds this struct.
    ///
    /// The body and not the dispatcher: a patch hands this address to every
    /// artifact holding a copy of the function, and naming the dispatcher would
    /// make each call take an extra hop through a slot that is never installed.
    ///
    /// `None` for an inherent method declared in a running artifact. A method's
    /// body cannot be hoisted into a separately addressable function, because
    /// every item inside a method body is barred from naming `Self`
    /// (`error[E0401]`) - so the body stays inline in the dispatcher and has no
    /// symbol of its own. Only a patch needs an address, and a patch names the
    /// receiver type concretely, so it always supplies one.
    pub implementation_address: Option<fn() -> usize>,
}

inventory::collect!(PillHotSlotDescriptor);

/// Redirect a `#[pill_hot_fn]` declared in THIS artifact.
///
/// # Errors
///
/// Returns [`HotPatchError::UnknownSystem`] when this artifact declares no such
/// function, [`HotPatchError::NullAddress`] for a null replacement, and
/// [`HotPatchError::SignatureMismatch`] when the signature text differs - which
/// is what stops a reshaped function being installed behind call sites compiled
/// for the old shape.
pub fn install_plain_function(
    qualified_name: &str,
    address: usize,
    signature: &str,
) -> Result<(), HotPatchError> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .ok_or_else(|| HotPatchError::UnknownSystem {
            name: qualified_name.to_string(),
        })?;

    if address == 0 {
        return Err(HotPatchError::NullAddress {
            name: qualified_name.to_string(),
        });
    }
    if descriptor.signature != signature {
        return Err(HotPatchError::SignatureMismatch {
            name: qualified_name.to_string(),
            expected: text_hash(descriptor.signature),
            found: text_hash(signature),
        });
    }
    descriptor.slot.install(address);
    Ok(())
}

/// Return one `#[pill_hot_fn]` to the body compiled into this artifact.
///
/// # Errors
///
/// Returns [`HotPatchError::UnknownSystem`] when this artifact declares no such
/// function.
pub fn reset_plain_function(qualified_name: &str) -> Result<(), HotPatchError> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .ok_or_else(|| HotPatchError::UnknownSystem {
            name: qualified_name.to_string(),
        })?;
    descriptor.slot.reset();
    Ok(())
}

/// Names of every `#[pill_hot_fn]` this artifact declares.
pub fn plain_function_names() -> impl Iterator<Item = &'static str> {
    inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.qualified_name)
}

/// The signature text this artifact recorded for a `#[pill_hot_fn]`.
///
/// Callers pass this straight back to [`install_plain_function`] rather than
/// reconstructing it: the exact spelling comes from `stringify!` inside the
/// macro, so writing it by hand is guesswork that fails the gate on a stray
/// space. A patch artifact derives its own copy from the same source through
/// the same macro, which is what makes the two comparable.
pub fn plain_function_signature(qualified_name: &str) -> Option<&'static str> {
    inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .map(|descriptor| descriptor.signature)
}

/// Address and signature of a `#[pill_hot_fn]` this artifact declares.
///
/// This is the pair a host needs in order to install this artifact's
/// implementation into another artifact's copy of the same function: the
/// address to jump to, and the signature text that copy compares against its
/// own before accepting it.
pub fn plain_function_entry(qualified_name: &str) -> Option<(usize, &'static str)> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)?;
    // `None` means this artifact declares the function but cannot address its
    // body - true of every inherent method outside a patch. Reporting nothing
    // is correct: the caller is asking a patch where its replacement lives.
    let address = descriptor.implementation_address?;
    Some((address(), descriptor.signature))
}

// =============================================================================
// Macro-free function inventory (SPIKE)
// =============================================================================

/// One function this artifact can report the address of, contributed by a
/// crate's build script rather than by an attribute.
///
/// This is the macro-free half of the Live++ style approach: a build script
/// scans the crate's own sources, finds every function, and emits one of these
/// per function. Nothing in the source is annotated, and because `inventory`
/// collects per artifact, a DLL ends up with an entry for every function in
/// every crate linked into it - which is exactly the fan-out a multi-artifact
/// engine needs.
pub struct PillFunctionAddress {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// The function's address in THIS artifact.
    ///
    /// A fn pointer rather than a `usize` because casting a function to an
    /// integer is not permitted in the constant context that builds this.
    pub address: fn() -> usize,
    /// The declaration as written, with whitespace collapsed.
    ///
    /// The compatibility gate for the prologue route, which has no other one:
    /// overwriting a function's first bytes cannot check anything about the
    /// replacement, so the check has to happen before the write. A host compares
    /// this against the signature it read from the edited source and refuses
    /// when they differ.
    pub signature: &'static str,
}

inventory::collect!(PillFunctionAddress);

/// Address of one function inside this artifact, by qualified path.
pub fn function_address(qualified_name: &str) -> Option<usize> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .find(|entry| entry.qualified_name == qualified_name)
        .map(|entry| (entry.address)())
}

/// The declaration this artifact was built with, by qualified path.
pub fn function_signature(qualified_name: &str) -> Option<&'static str> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .find(|entry| entry.qualified_name == qualified_name)
        .map(|entry| entry.signature)
}

/// Every function this artifact can report, for diagnostics.
pub fn function_addresses() -> impl Iterator<Item = (&'static str, usize)> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .map(|entry| (entry.qualified_name, (entry.address)()))
}

// =============================================================================
// Prologue patching (SPIKE, Windows x86-64 only)
// =============================================================================

/// Bytes an absolute jump occupies: `mov rax, imm64` then `jmp rax`.
///
/// The absolute form is used rather than a 5-byte `E9 rel32` because ASLR
/// routinely places a freshly loaded patch image several gigabytes from the
/// base image - measured at +7.4 GB during the original research - so the
/// relative form would need a trampoline on essentially every patch anyway.
/// Twelve bytes is more of the function to overwrite, which is the trade.
const ABSOLUTE_JUMP_LENGTH: usize = 12;

/// Read/write/execute page protection.
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// One entry in the x86-64 exception directory: a function's exact extent.
///
/// Windows requires every function that touches the stack to publish one of
/// these so the unwinder can walk it, which makes the table an authoritative
/// map of where each function begins and ends - no disassembler needed.
#[repr(C)]
struct RuntimeFunction {
    /// Offset of the function's first byte from the image base.
    begin_address: u32,
    /// Offset one past the function's last byte.
    end_address: u32,
    /// Offset of the unwind data; unused here.
    _unwind_data: u32,
}

extern "system" {
    fn VirtualProtect(
        address: *mut core::ffi::c_void,
        size: usize,
        new_protection: u32,
        old_protection: *mut u32,
    ) -> i32;
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn FlushInstructionCache(
        process: *mut core::ffi::c_void,
        base: *const core::ffi::c_void,
        size: usize,
    ) -> i32;
    fn RtlLookupFunctionEntry(
        control_address: u64,
        image_base: *mut u64,
        history_table: *mut core::ffi::c_void,
    ) -> *const RuntimeFunction;
}

/// How many of this artifact's registered functions have a known extent.
///
/// Diagnostic: a prologue patch can only overwrite a function whose length the
/// exception directory records, so this says how much of an artifact is
/// reachable by that route at all.
pub fn functions_with_known_extent() -> (usize, usize) {
    let mut known = 0usize;
    let mut total = 0usize;
    for (_, address) in function_addresses() {
        total += 1;
        if function_extent(address).is_some() {
            known += 1;
        }
    }
    (known, total)
}

/// Where the function containing `address` begins and how long it is.
///
/// Read from the running image's exception directory, so the answer is the
/// linker's own record rather than a guess. Returns `None` when the address has
/// no entry, which is the case for a leaf function small enough that Windows
/// permits omitting its unwind data - exactly the functions too short to
/// overwrite safely, so an absent entry is treated as "refuse", never as
/// "assume it is long enough".
pub fn function_extent(address: usize) -> Option<(usize, usize)> {
    let mut image_base: u64 = 0;
    // SAFETY: `RtlLookupFunctionEntry` reads the loaded image's exception
    // directory for an arbitrary address and reports a null entry when it finds
    // none. `image_base` is a writable local, and passing a null history table
    // asks for no caching.
    let entry = unsafe {
        RtlLookupFunctionEntry(address as u64, &mut image_base, core::ptr::null_mut())
    };
    if entry.is_null() {
        return None;
    }
    // SAFETY: the pointer is non-null and points into the image's static
    // exception directory, which stays mapped for as long as the module is.
    let entry = unsafe { &*entry };
    let begin = image_base as usize + entry.begin_address as usize;
    let end = image_base as usize + entry.end_address as usize;
    if end <= begin {
        return None;
    }
    Some((begin, end - begin))
}

/// Overwrite a function's first bytes with an absolute jump to `replacement`.
///
/// Returns the original bytes, so the patch can be undone by writing them back.
///
/// The function's own length is checked against the exception directory first,
/// so a function too short to hold the jump is refused rather than overwritten
/// past its end - which would corrupt whichever function the linker placed
/// next.
///
/// # Safety
///
/// `replacement` must be a function with a signature compatible with the one at
/// `target`, and **no thread may be executing inside the overwritten bytes**.
/// The caller guarantees the second condition by patching at a frame boundary,
/// on the thread that runs the frame loop, while no other thread is inside the
/// function. `target` is validated here rather than assumed.
pub unsafe fn patch_prologue(target: usize, replacement: usize) -> Result<Vec<u8>, String> {
    if target == 0 || replacement == 0 {
        return Err("cannot patch a null address".to_string());
    }
    if target == replacement {
        return Err("refusing to patch a function to itself".to_string());
    }

    // How much room there actually is. Without this the write runs past a short
    // function and into whatever the linker placed after it, which is usually
    // another live function - a corruption that surfaces far from its cause.
    let Some((begin, length)) = function_extent(target) else {
        return Err(format!(
            "0x{target:016x} is a leaf function with no entry in the exception \
             directory, so its length cannot be established and overwriting it \
             is unsafe; annotate it with #[pill_hot_fn] to patch it through a \
             dispatch slot instead, which needs no length at all"
        ));
    };
    if begin != target {
        return Err(format!(
            "0x{target:016x} is not a function entry point (the function containing \
             it begins at 0x{begin:016x})"
        ));
    }
    if length < ABSOLUTE_JUMP_LENGTH {
        return Err(format!(
            "the function at 0x{target:016x} is {length} bytes, too short for the \
             {ABSOLUTE_JUMP_LENGTH}-byte jump a patch installs"
        ));
    }

    let target_pointer = target as *mut u8;
    let mut previous_protection: u32 = 0;

    // SAFETY: the caller guarantees `target` addresses executable code; making
    // its page writable is exactly what this function exists to do.
    let changed = unsafe {
        VirtualProtect(
            target_pointer as *mut core::ffi::c_void,
            ABSOLUTE_JUMP_LENGTH,
            PAGE_EXECUTE_READWRITE,
            &mut previous_protection,
        )
    };
    if changed == 0 {
        return Err("VirtualProtect refused to make the page writable".to_string());
    }

    // SAFETY: the page is now writable and the range was validated above.
    let original = unsafe { core::slice::from_raw_parts(target_pointer, ABSOLUTE_JUMP_LENGTH) }
        .to_vec();

    // mov rax, imm64 ; jmp rax
    let mut instructions = [0u8; ABSOLUTE_JUMP_LENGTH];
    instructions[0] = 0x48;
    instructions[1] = 0xB8;
    instructions[2..10].copy_from_slice(&(replacement as u64).to_le_bytes());
    instructions[10] = 0xFF;
    instructions[11] = 0xE0;

    // SAFETY: the page is writable and the slice length matches exactly.
    unsafe {
        core::ptr::copy_nonoverlapping(
            instructions.as_ptr(),
            target_pointer,
            ABSOLUTE_JUMP_LENGTH,
        );
    }

    // SAFETY: restoring the protection the call above reported.
    unsafe {
        let mut discarded: u32 = 0;
        VirtualProtect(
            target_pointer as *mut core::ffi::c_void,
            ABSOLUTE_JUMP_LENGTH,
            previous_protection,
            &mut discarded,
        );
        // Required on any core whose instruction cache may hold the old bytes.
        FlushInstructionCache(
            GetCurrentProcess(),
            target_pointer as *const core::ffi::c_void,
            ABSOLUTE_JUMP_LENGTH,
        );
    }

    Ok(original)
}

/// Write saved prologue bytes back, undoing a patch.
///
/// # Safety
///
/// The same contract as [`patch_prologue`]: `target` must be the address the
/// bytes came from, and no thread may be executing inside them.
pub unsafe fn restore_prologue(target: usize, original: &[u8]) -> Result<(), String> {
    if target == 0 {
        return Err("cannot restore a null address".to_string());
    }
    // The same bound the patch was checked against. A restore aimed at an
    // address whose image has since been replaced would otherwise write into
    // whatever now occupies it, so the extent is re-read rather than trusted.
    let Some((begin, length)) = function_extent(target) else {
        return Err(format!(
            "0x{target:016x} has no entry in the exception directory; refusing to \
             write over it"
        ));
    };
    if begin != target || length < original.len() {
        return Err(format!(
            "0x{target:016x} no longer names a function long enough for the {} saved \
             bytes; the image was probably replaced",
            original.len()
        ));
    }
    let target_pointer = target as *mut u8;
    let mut previous_protection: u32 = 0;
    // SAFETY: as documented on this function.
    let changed = unsafe {
        VirtualProtect(
            target_pointer as *mut core::ffi::c_void,
            original.len(),
            PAGE_EXECUTE_READWRITE,
            &mut previous_protection,
        )
    };
    if changed == 0 {
        return Err("VirtualProtect refused to make the page writable".to_string());
    }
    // SAFETY: the page is writable and the length matches the saved slice.
    unsafe {
        core::ptr::copy_nonoverlapping(original.as_ptr(), target_pointer, original.len());
        let mut discarded: u32 = 0;
        VirtualProtect(
            target_pointer as *mut core::ffi::c_void,
            original.len(),
            previous_protection,
            &mut discarded,
        );
        FlushInstructionCache(
            GetCurrentProcess(),
            target_pointer as *const core::ffi::c_void,
            original.len(),
        );
    }
    Ok(())
}

/// Stable hash of a signature string, used only to report a mismatch.
fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// =============================================================================
// Compile-time registry of hot-patchable functions
// =============================================================================

/// One function declared with `#[pill_hot]`.
///
/// Submitted into this artifact's registry by the attribute macro. Every field
/// is const-constructible so the descriptor can live in a static, matching how
/// [`PillComponentDescriptor`](crate::component_registry::PillComponentDescriptor)
/// works.
///
/// The registry is **per linked artifact**: the host executable, each optional
/// module DLL and the project DLL each carry exactly the descriptors their own
/// sources declared. That is what makes it correct across a reload - a
/// generation that stops declaring a function simply stops submitting it, and
/// an evicted DLL takes its descriptors with it.
pub struct PillHotFunctionDescriptor {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// Resolves this function's dispatch address and signature identity.
    ///
    /// A function rather than two constants because both values require the
    /// generic machinery in [`local_implementation_address`] and
    /// [`signature_hash_of`], which cannot run in a `const`.
    pub resolve: fn() -> (usize, u64),
}

inventory::collect!(PillHotFunctionDescriptor);

/// Look up a hot-patchable function declared in THIS artifact.
///
/// Returns its dispatch address and signature hash, or `None` when no
/// `#[pill_hot]` function carries that qualified name. A host calls the
/// equivalent through the artifact's exported resolver rather than directly.
pub fn resolve_hot_function(qualified_name: &str) -> Option<(usize, u64)> {
    inventory::iter::<PillHotFunctionDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .map(|descriptor| (descriptor.resolve)())
}

/// Every hot-patchable function this artifact declares, for diagnostics.
pub fn hot_function_names() -> impl Iterator<Item = &'static str> {
    inventory::iter::<PillHotFunctionDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.qualified_name)
}

// =============================================================================
// ABI self-check
// =============================================================================

/// Verify that a `&mut ZST` parameter can be received as `&mut ()`.
///
/// This is the one platform assumption the dispatch slot rests on. Both are
/// references to zero-sized types, so both are a single non-null pointer-sized
/// argument, and every ABI the engine targets passes them identically - but
/// that is an observation, not a guarantee Rust makes.
///
/// Returns `true` when arguments survive the substitution intact. A host should
/// call this once at startup in debug builds so a toolchain or target that
/// breaks the assumption fails at initialization rather than corrupting a
/// system's arguments mid-frame.
pub fn verify_abi() -> bool {
    /// Stands in for a system's zero-sized fn-item type.
    struct ZeroSized;

    /// The shape a patch exports: the stand-in plus real arguments.
    fn receiver(_stand_in: &mut (), first: u64, second: u64, third: u64) -> u64 {
        first
            .wrapping_mul(1_000_000)
            .wrapping_add(second.wrapping_mul(1_000))
            .wrapping_add(third)
    }

    let mut zero_sized = ZeroSized;
    // SAFETY: `receiver` declares its first parameter as `&mut ()` and never
    // reads it; `&mut ZeroSized` is likewise a non-null pointer-sized value for
    // a zero-sized type. This call is exactly the substitution under test, and
    // a mismatched ABI shifts the arguments rather than faulting - which is why
    // the result is checked instead of merely reaching this line.
    let observed = unsafe {
        let call: fn(&mut ZeroSized, u64, u64, u64) -> u64 =
            std::mem::transmute(receiver as *const ());
        call(&mut zero_sized, 11, 22, 33)
    };
    observed == 11_022_033
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The stand-in substitution holds on this target.
    ///
    /// If this ever fails, the dispatch slot is unsound here and the hot-patch
    /// feature must not be enabled.
    #[test]
    fn zero_sized_stand_in_is_abi_compatible() {
        assert!(verify_abi(), "&mut ZST is not passed like &mut () here");
    }

    /// `Input` must be inferable from the function value alone, with no
    /// turbofish. A concrete function-item type satisfies exactly one arity of
    /// [`SystemParamFunction`], so the solver has a unique choice - and the
    /// `#[pill_hot]` macro relies on this, because reconstructing the parameter
    /// tuple from syntax would have to strip patterns and guess lifetimes.
    #[test]
    fn input_infers_from_the_function_value_alone() {
        fn nullary() {}
        fn fallible() -> Result<(), crate::error::SystemError> {
            Ok(())
        }

        assert_ne!(local_implementation_address(&nullary), 0);
        assert_ne!(local_implementation_address(&fallible), 0);
        assert_ne!(
            signature_hash_of(&nullary),
            signature_hash_of(&fallible),
            "differing return types must produce differing signatures"
        );
    }

    #[test]
    fn slot_reports_its_baseline() {
        let slot = HotSlot::new();
        slot.initialize(0x1234, 0xABCD);
        assert_eq!(slot.current(), 0x1234);
        assert_eq!(slot.signature_hash(), 0xABCD);
    }

    #[test]
    fn install_replaces_the_implementation() {
        let slot = HotSlot::new();
        slot.initialize(0x1000, 0xFEED);
        assert!(slot.install("movement", 0x2000, 0xFEED).is_ok());
        assert_eq!(slot.current(), 0x2000);
    }

    /// The gate must refuse a moved signature and leave the running code alone.
    #[test]
    fn install_refuses_a_signature_mismatch() {
        let slot = HotSlot::new();
        slot.initialize(0x1000, 0xFEED);

        let error = slot.install("movement", 0x2000, 0xBEEF).unwrap_err();
        assert_eq!(
            error,
            HotPatchError::SignatureMismatch {
                name: "movement".to_string(),
                expected: 0xFEED,
                found: 0xBEEF,
            }
        );
        assert_eq!(slot.current(), 0x1000, "refused patch must not be applied");
    }

    #[test]
    fn install_refuses_a_null_address() {
        let slot = HotSlot::new();
        slot.initialize(0x1000, 0xFEED);
        assert!(slot.install("movement", 0, 0xFEED).is_err());
        assert_eq!(slot.current(), 0x1000);
    }

    #[test]
    fn registry_round_trips_and_forgets() {
        let mut registry = HotPatchRegistry::new();
        assert!(registry.is_empty());

        let slot = Arc::new(HotSlot::new());
        slot.initialize(0x1000, 0x1);
        registry.insert("movement", Arc::clone(&slot));

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("movement").unwrap().current(), 0x1000);
        assert!(registry.get("absent").is_none());

        registry.remove("movement");
        assert!(registry.get("movement").is_none());
        assert!(registry.is_empty());
    }

    /// A slot shared with a system's closure observes installs made through the
    /// registry - the property the whole design depends on.
    #[test]
    fn registry_and_closure_share_one_slot() {
        let mut registry = HotPatchRegistry::new();
        let slot = Arc::new(HotSlot::new());
        slot.initialize(0x1000, 0x7);
        registry.insert("movement", Arc::clone(&slot));

        registry
            .get("movement")
            .unwrap()
            .install("movement", 0x2000, 0x7)
            .expect("install");

        assert_eq!(slot.current(), 0x2000, "the closure's handle must see it");
    }

    // -------------------------------------------------------------------------
    // Registry
    // -------------------------------------------------------------------------
    //
    // `#[pill_hot]` cannot be used inside `pill_engine` itself: its generated
    // code refers to `::pill_engine`, which does not resolve in the defining
    // crate. These tests therefore submit a descriptor by hand, exactly as the
    // macro would, which is the same approach `component_registry` takes. The
    // macro's own expansion is covered downstream, where it can actually run.

    /// Stands in for a `#[pill_hot]` system.
    fn registry_probe_system() {}

    fn registry_probe_descriptor() -> (usize, u64) {
        (
            local_implementation_address(&registry_probe_system),
            signature_hash_of(&registry_probe_system),
        )
    }

    inventory::submit! {
        PillHotFunctionDescriptor {
            qualified_name: "pill_engine::hot_patch::tests::registry_probe_system",
            resolve: registry_probe_descriptor,
        }
    }

    #[test]
    fn registry_resolves_a_submitted_function() {
        let (address, hash) =
            resolve_hot_function("pill_engine::hot_patch::tests::registry_probe_system")
                .expect("the submitted descriptor must be discoverable");

        assert_ne!(address, 0, "a resolved address must be callable");
        assert_eq!(
            address,
            local_implementation_address(&registry_probe_system),
            "the registry must report the same dispatch address the engine registers"
        );
        assert_eq!(hash, signature_hash_of(&registry_probe_system));
    }

    #[test]
    fn registry_reports_nothing_for_an_unknown_name() {
        assert!(resolve_hot_function("nothing::declares::this").is_none());
    }

    #[test]
    fn registry_lists_its_functions() {
        assert!(
            hot_function_names().any(|name| name.ends_with("registry_probe_system")),
            "a submitted function must appear in the listing"
        );
    }

    /// Two systems with different signatures hash differently; the same
    /// signature hashes identically across calls.
    #[test]
    fn signature_hash_distinguishes_signatures() {
        fn takes_nothing() {}
        fn takes_nothing_too() {}

        let first = signature_hash::<fn(), ()>();
        let second = signature_hash::<fn(), ()>();
        assert_eq!(first, second, "same signature must hash stably");

        let _ = (takes_nothing, takes_nothing_too);
    }
}

// =============================================================================
// Integration tests
// =============================================================================

/// End-to-end checks that a system registered through the normal engine API can
/// have its implementation replaced, and that everything around it stays put.
#[cfg(all(test, feature = "hot_patch"))]
mod integration_tests {
    use super::*;
    use crate::error::SystemError;
    use crate::{Engine, SystemOwner};

    /// Declares an isolated observable counter plus the two system bodies that
    /// drive it.
    ///
    /// One set per test: the harness runs tests in parallel, so a single shared
    /// counter would see other tests' increments and make any assertion about
    /// how many times a system ran meaningless.
    macro_rules! isolated_systems {
        ($name:ident) => {
            mod $name {
                use std::sync::atomic::{AtomicU32, Ordering};

                static OBSERVED: AtomicU32 = AtomicU32::new(0);

                pub fn original() {
                    OBSERVED.fetch_add(1, Ordering::SeqCst);
                }

                pub fn replacement() {
                    OBSERVED.fetch_add(10, Ordering::SeqCst);
                }

                pub fn observed() -> u32 {
                    OBSERVED.load(Ordering::SeqCst)
                }
            }
        };
    }

    isolated_systems!(replaces_behavior);
    isolated_systems!(registration_intact);
    isolated_systems!(signature_refused);
    isolated_systems!(unknown_refused);
    isolated_systems!(cleared_slots);
    isolated_systems!(second_patch);
    isolated_systems!(rolled_back);

    /// A different signature, used to prove the gate refuses it.
    fn differently_shaped_system() -> Result<(), SystemError> {
        Ok(())
    }

    fn sequential_engine() -> Engine {
        let mut engine = Engine::new();
        engine.set_parallel_execution(false);
        engine
    }

    /// The headline behavior: a patched system runs new code on the next frame.
    #[test]
    fn patch_replaces_behavior_on_the_next_frame() {
        use replaces_behavior as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 1, "baseline runs");

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("patch accepted");

        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "patched body must run, continuing from the previous value"
        );
    }

    /// The system's slot count and enabled state survive a patch: only the
    /// implementation moved.
    #[test]
    fn patch_leaves_the_registration_intact() {
        use registration_intact as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);
        let slots_before = engine.hot_patch_registry().len();

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("patch accepted");

        assert_eq!(
            engine.hot_patch_registry().len(),
            slots_before,
            "no system was added or removed"
        );
        assert_eq!(engine.is_system_enabled("counter"), Some(true));
    }

    /// A signature change must be refused, and the running code must survive it.
    #[test]
    fn patch_with_a_changed_signature_is_refused() {
        use signature_refused as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);
        engine.process_frame().expect("frame");

        let error = engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&differently_shaped_system),
                signature_hash_of::<_, ()>(&differently_shaped_system),
            )
            .expect_err("a changed signature must be refused");
        assert!(matches!(error, HotPatchError::SignatureMismatch { .. }));

        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            2,
            "the original implementation must still be running"
        );
    }

    #[test]
    fn patching_an_unregistered_system_is_refused() {
        use unknown_refused as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        let error = engine
            .hot_patch(
                "not_registered",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect_err("unknown system must be refused");
        assert!(matches!(error, HotPatchError::UnknownSystem { .. }));
    }

    /// Clearing a module's systems must also retire their slots, so a late
    /// patch cannot install into a system that no longer runs.
    #[test]
    fn cleared_systems_stop_being_patchable() {
        use cleared_slots as systems;
        let mut engine = sequential_engine();
        let owner = SystemOwner::optional_module(0);

        // Distinct functions, so each system gets its own function-path alias
        // and the two do not share a registry entry.
        engine.begin_module_registration(owner);
        engine.register_system("module_counter", systems::original);
        engine.end_module_registration();
        engine.register_system("project_counter", systems::replacement);

        assert!(engine.hot_patch_registry().get("module_counter").is_some());
        assert!(engine.hot_patch_registry().get("project_counter").is_some());

        engine.clear_systems_owned_by(owner);

        assert!(
            engine.hot_patch_registry().get("module_counter").is_none(),
            "a cleared system's slot must be forgotten"
        );
        assert!(
            engine.hot_patch_registry().get("project_counter").is_some(),
            "another owner's system must survive"
        );

        let error = engine
            .hot_patch(
                "module_counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect_err("a cleared system must not be patchable");
        assert!(matches!(error, HotPatchError::UnknownSystem { .. }));
    }

    /// A system is patchable by its function path as well as its display name.
    ///
    /// The two routinely differ - the example project registers
    /// `physics_system` as `"ball_physics"` - and a generated patch only knows
    /// the path, because that is what `#[pill_hot]` derives.
    #[test]
    fn a_system_is_patchable_by_its_function_path() {
        use registration_intact as systems;
        let mut engine = sequential_engine();
        engine.register_system("a_display_name", systems::original);

        let path = function_path::<fn()>();
        assert!(path.is_none(), "a bare fn pointer type has no path");

        // The path the engine recorded for this registration.
        let recorded: Vec<String> = engine
            .hot_patch_registry()
            .names()
            .map(|name| name.to_string())
            .collect();
        let function_path_entry = recorded
            .iter()
            .find(|name| name.ends_with("::original"))
            .expect("the function path must be registered alongside the display name");

        assert!(
            engine.hot_patch_registry().get(function_path_entry).is_some(),
            "both names must resolve to a slot"
        );

        // Clearing through one name must retire the other too.
        engine.clear_systems();
        assert!(engine.hot_patch_registry().is_empty());
    }

    /// A system can be returned to the code its artifact was built with, after
    /// any number of patches.
    ///
    /// This is generation zero of a rollback. It works because the slot records
    /// the registration address separately from the current one - without that,
    /// the first patch would make the original unreachable.
    #[test]
    fn a_system_can_be_rolled_back_to_its_baseline() {
        use rolled_back as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        let (baseline, baseline_hash) = engine
            .hot_patch_baseline("counter")
            .expect("a registered system must record a baseline");

        // Three generations, as a live-coding session produces.
        for _ in 0..3 {
            engine
                .hot_patch(
                    "counter",
                    local_implementation_address::<_, ()>(&systems::replacement),
                    signature_hash_of::<_, ()>(&systems::replacement),
                )
                .expect("patch");
        }
        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 10, "the patch must be running");

        // The baseline is still reachable and still installable.
        engine
            .hot_patch("counter", baseline, baseline_hash)
            .expect("rollback to the baseline must be accepted");
        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "generation zero must run the original body"
        );
    }

    /// A plain function's slot falls back to the artifact's own body once
    /// emptied, which is how a plain function rolls back to generation zero.
    #[test]
    fn an_emptied_plain_slot_falls_back_to_the_original() {
        fn original() -> u32 {
            1
        }
        fn replacement() -> u32 {
            2
        }

        let slot = PlainSlot::new();
        let original_address = original as *const () as usize;
        assert_eq!(slot.current_or(original_address), original_address);

        slot.install(replacement as *const () as usize);
        assert_eq!(
            slot.current_or(original_address),
            replacement as *const () as usize,
            "an installed replacement must win"
        );

        slot.reset();
        assert_eq!(
            slot.current_or(original_address),
            original_address,
            "an emptied slot must fall back to the artifact's own body"
        );
    }

    // =========================================================================
    // Prologue patching
    // =========================================================================

    /// A function whose prologue gets overwritten and put back.
    ///
    /// `inline(never)` because the test needs a real symbol with a real entry in
    /// the exception directory; an inlined body has neither.
    #[inline(never)]
    fn patch_target(value: u32) -> u32 {
        value + 1
    }

    /// Its stand-in, with the same call shape.
    #[inline(never)]
    fn patch_replacement(value: u32) -> u32 {
        value + 1000
    }

    /// The headline behaviour: callers of a patched function reach the
    /// replacement, and restoring the saved bytes puts the original back.
    ///
    /// One test rather than three, because the bytes are process-wide state and
    /// the harness runs tests on several threads.
    #[test]
    fn a_patched_prologue_redirects_and_restores() {
        let target = patch_target as *const () as usize;
        let replacement = patch_replacement as *const () as usize;
        assert_eq!(std::hint::black_box(patch_target)(1), 2);

        // SAFETY: both addresses name real functions in this binary with the
        // same call shape, and no other thread calls `patch_target`.
        let original = unsafe { patch_prologue(target, replacement) }
            .expect("a normal function must be patchable");
        assert_eq!(original.len(), ABSOLUTE_JUMP_LENGTH);
        assert_eq!(
            std::hint::black_box(patch_target)(1),
            1001,
            "callers must reach the replacement"
        );

        // SAFETY: the same address the bytes were saved from, unchanged since.
        unsafe { restore_prologue(target, &original) }.expect("restore must succeed");
        assert_eq!(
            std::hint::black_box(patch_target)(1),
            2,
            "restoring must bring the original body back"
        );
    }

    /// A second stand-in, so a function can be patched twice.
    #[inline(never)]
    fn patch_second_replacement(value: u32) -> u32 {
        value + 2000
    }

    /// Patching twice and rolling back needs the FIRST generation's saved
    /// bytes, not the newest.
    ///
    /// This is the assumption `restore_prologue_baseline` rests on: generation
    /// two overwrote the jump generation one had written, so its "original" is a
    /// jump. Restoring from it would reinstate a patch instead of removing one.
    #[test]
    fn the_first_generation_holds_the_artifacts_own_code() {
        let target = patch_target_twice as *const () as usize;
        let first_replacement = patch_replacement as *const () as usize;
        let second_replacement = patch_second_replacement as *const () as usize;
        assert_eq!(std::hint::black_box(patch_target_twice)(1), 2);

        // SAFETY: real functions in this binary with the same call shape, and
        // no other thread calls `patch_target_twice`.
        let generation_one = unsafe { patch_prologue(target, first_replacement) }
            .expect("first patch");
        assert_eq!(std::hint::black_box(patch_target_twice)(1), 1001);

        // SAFETY: as above. What this returns is generation one's jump.
        let generation_two = unsafe { patch_prologue(target, second_replacement) }
            .expect("second patch");
        assert_eq!(std::hint::black_box(patch_target_twice)(1), 2001);

        assert_ne!(
            generation_one, generation_two,
            "the second patch must have saved the first patch's jump, not the original"
        );

        // Restoring from the NEWEST saved bytes reinstates generation one - the
        // mistake this test exists to pin down.
        // SAFETY: the address these bytes were saved from, unchanged since.
        unsafe { restore_prologue(target, &generation_two) }.expect("restore");
        assert_eq!(
            std::hint::black_box(patch_target_twice)(1),
            1001,
            "restoring the newest saved bytes brings back generation one, not the original"
        );

        // Restoring from the FIRST saved bytes is what actually reaches
        // generation zero.
        // SAFETY: as above.
        unsafe { restore_prologue(target, &generation_one) }.expect("restore");
        assert_eq!(
            std::hint::black_box(patch_target_twice)(1),
            2,
            "the first generation's bytes are the artifact's own code"
        );
    }

    /// Its own target, so the test above cannot disturb the round-trip test.
    #[inline(never)]
    fn patch_target_twice(value: u32) -> u32 {
        value + 1
    }

    /// The guard that P0-1 was about: an address the exception directory does
    /// not describe has no known length, so patching it is refused rather than
    /// writing twelve bytes over whatever happens to be there.
    #[test]
    fn an_address_with_no_function_entry_is_refused() {
        static NOT_A_FUNCTION: [u8; 64] = [0x90; 64];
        let target = NOT_A_FUNCTION.as_ptr() as usize;
        let replacement = patch_replacement as *const () as usize;

        // SAFETY: the call is expected to refuse before writing anything, which
        // is exactly what this asserts.
        let result = unsafe { patch_prologue(target, replacement) };
        let detail = result.expect_err("a non-function address must be refused");
        assert!(
            detail.contains("exception directory"),
            "unexpected reason: {detail}"
        );
        assert_eq!(
            NOT_A_FUNCTION[..ABSOLUTE_JUMP_LENGTH],
            [0x90; ABSOLUTE_JUMP_LENGTH],
            "a refused patch must not have written anything"
        );
    }

    /// Patching the middle of a function is refused: the saved bytes would not
    /// be a prologue, and a later restore would corrupt the body.
    #[test]
    fn an_address_inside_a_function_is_refused() {
        let target = patch_target as *const () as usize + 1;
        let replacement = patch_replacement as *const () as usize;

        // SAFETY: expected to refuse before writing.
        let result = unsafe { patch_prologue(target, replacement) };
        let detail = result.expect_err("an interior address must be refused");
        assert!(
            detail.contains("not a function entry point"),
            "unexpected reason: {detail}"
        );
    }

    /// The extent lookup reports the linker's own record, so a real function is
    /// described and its reported start is the address the caller holds.
    #[test]
    fn the_exception_directory_describes_a_real_function() {
        let target = patch_target as *const () as usize;
        let (begin, length) = function_extent(target).expect("a real function must be described");
        assert_eq!(begin, target, "the entry must start where the symbol does");
        assert!(
            length >= ABSOLUTE_JUMP_LENGTH,
            "an ordinary function should have room for the jump, got {length} bytes"
        );
    }

    /// Patching twice runs the newest implementation, which is what a second
    /// edit in a live-coding session does.
    #[test]
    fn a_second_patch_supersedes_the_first() {
        use second_patch as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("first patch");
        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 10);

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::original),
                signature_hash_of::<_, ()>(&systems::original),
            )
            .expect("second patch, back to the original");
        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "the newest implementation must win"
        );
    }
}
