//! The replaceable-function machinery a hot reload dispatches through.
//!
//! # Responsibilities
//!
//! - Defines the slots a patched function is installed into and reset from.
//! - Holds the per-artifact registries the attribute macros submit into.
//! - Implements the macro-free prologue patching route on Windows x86-64.
//! - Verifies the one ABI assumption the dispatch slot rests on.
//!
//! # Design
//!
//! This crate is linked by every artifact that participates in hot reloading -
//! the host executable, each optional module and the project - and each links
//! its own copy. That is deliberate: the `inventory` registries below are
//! per-artifact, so a generation that stops declaring a function stops
//! submitting it, and an unmapped image takes its descriptors with it.
//!
//! It was extracted from `pill_engine`, which re-exports it as
//! `pill_engine::hot_patch` so that the paths two code generators emit keep
//! resolving. Nothing here knows about entities, components or systems; the
//! three helpers that do - the signature-identity functions, which need
//! `SystemParamFunction` - stayed behind in `pill_engine`.

// Standard library
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

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

/// The thread allowed to rewrite live code, claimed by the first patch.
///
/// Patching assumes no thread is executing inside the bytes being overwritten.
/// The host upholds that by patching at a frame boundary with no system
/// running - but that is a convention, and a convention that is only ever
/// stated in a comment is one that quietly stops being true. Recording the
/// thread turns it into something checkable: a second thread attempting a patch
/// is refused rather than racing the first.
static PATCHING_THREAD: AtomicU64 = AtomicU64::new(0);

/// A stable-ish numeric identity for the current thread.
///
/// `ThreadId` has no stable integer form on stable Rust, so the address of a
/// thread-local is used instead: distinct per thread, constant within one.
fn current_thread_token() -> u64 {
    thread_local! {
        static MARKER: u8 = const { 0 };
    }
    MARKER.with(|marker| marker as *const u8 as u64)
}

/// Declare the calling thread the only one permitted to rewrite live code.
///
/// A host calls this from its frame loop. Idempotent, and cheap enough to call
/// every frame: one relaxed load in the common case.
///
/// Nothing forces a host to call it - a process that never does keeps the older
/// behaviour of allowing any thread, which is what tests and offline tools want.
pub fn declare_patching_thread() {
    let token = current_thread_token();
    if PATCHING_THREAD.load(Ordering::Relaxed) != token {
        PATCHING_THREAD.store(token, Ordering::Release);
    }
}

/// Refuse a patch attempted from anywhere but the declared thread.
///
/// # Errors
///
/// Returns the message to refuse with. Undeclared means unrestricted.
fn check_patching_thread() -> Result<(), String> {
    let declared = PATCHING_THREAD.load(Ordering::Acquire);
    if declared == 0 || declared == current_thread_token() {
        return Ok(());
    }
    Err(
        "a patch was attempted from a thread other than the one the host \
         declared; rewriting live code is only sound from the thread that owns \
         the frame boundary, where no system is executing"
            .to_string(),
    )
}

/// Bytes an absolute jump occupies: `mov rax, imm64` then `jmp rax`.
///
/// The absolute form is used rather than a 5-byte `E9 rel32` because ASLR
/// routinely places a freshly loaded patch image several gigabytes from the
/// base image - measured at +7.4 GB during the original research - so the
/// relative form would need a trampoline on essentially every patch anyway.
/// Twelve bytes is more of the function to overwrite, which is the trade.
const ABSOLUTE_JUMP_LENGTH: usize = 12;

/// Read/write/execute page protection.
#[cfg(all(windows, target_arch = "x86_64"))]
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// One entry in the x86-64 exception directory: a function's exact extent.
///
/// Windows requires every function that touches the stack to publish one of
/// these so the unwinder can walk it, which makes the table an authoritative
/// map of where each function begins and ends - no disassembler needed.
#[cfg(all(windows, target_arch = "x86_64"))]
#[repr(C)]
struct RuntimeFunction {
    /// Offset of the function's first byte from the image base.
    begin_address: u32,
    /// Offset one past the function's last byte.
    end_address: u32,
    /// Offset of the unwind data; unused here.
    _unwind_data: u32,
}

#[cfg(all(windows, target_arch = "x86_64"))]
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
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn function_extent(address: usize) -> Option<(usize, usize)> {
    let mut image_base: u64 = 0;
    // SAFETY: `RtlLookupFunctionEntry` reads the loaded image's exception
    // directory for an arbitrary address and reports a null entry when it finds
    // none. `image_base` is a writable local, and passing a null history table
    // asks for no caching.
    let entry =
        unsafe { RtlLookupFunctionEntry(address as u64, &mut image_base, core::ptr::null_mut()) };
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
#[cfg(all(windows, target_arch = "x86_64"))]
pub unsafe fn patch_prologue(target: usize, replacement: usize) -> Result<Vec<u8>, String> {
    if target == 0 || replacement == 0 {
        return Err("cannot patch a null address".to_string());
    }
    if target == replacement {
        return Err("refusing to patch a function to itself".to_string());
    }
    check_patching_thread()?;

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
    let original =
        unsafe { core::slice::from_raw_parts(target_pointer, ABSOLUTE_JUMP_LENGTH) }.to_vec();

    // mov rax, imm64 ; jmp rax
    let mut instructions = [0u8; ABSOLUTE_JUMP_LENGTH];
    instructions[0] = 0x48;
    instructions[1] = 0xB8;
    instructions[2..10].copy_from_slice(&(replacement as u64).to_le_bytes());
    instructions[10] = 0xFF;
    instructions[11] = 0xE0;

    // SAFETY: the page is writable and the slice length matches exactly.
    unsafe {
        core::ptr::copy_nonoverlapping(instructions.as_ptr(), target_pointer, ABSOLUTE_JUMP_LENGTH);
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
#[cfg(all(windows, target_arch = "x86_64"))]
pub unsafe fn restore_prologue(target: usize, original: &[u8]) -> Result<(), String> {
    if target == 0 {
        return Err("cannot restore a null address".to_string());
    }
    // The same guard `patch_prologue` carries, for the same reason: this writes
    // twelve bytes over live code and no twelve-byte write is atomic, so a
    // thread executing inside those bytes could observe a torn instruction
    // stream. Restoring is not the gentler operation it sounds like - it is the
    // identical hazard in the other direction, and the doc comment above already
    // claims the contract. Enforced here rather than assumed.
    check_patching_thread()?;
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

    // The bytes there now must be the jump this module installed. Extent alone
    // is not enough: an image can be unloaded and a different one mapped over
    // the same address, and that lookup would succeed. Writing the saved bytes
    // then corrupts an unrelated live function - during a rollback, when the
    // developer is already recovering from something.
    //
    // SAFETY: the extent check above established that `target` is the entry of a
    // mapped function at least `original.len()` bytes long, so this many bytes
    // are readable.
    let installed = unsafe { core::slice::from_raw_parts(target_pointer, original.len()) };
    if installed.len() < 2 || installed[0] != 0x48 || installed[1] != 0xB8 {
        return Err(format!(
            "0x{target:016x} does not begin with the jump this patch installed, so \
             it is no longer the function those bytes came from; refusing to \
             write over it"
        ));
    }

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

// =============================================================================
// Prologue patching (other targets)
// =============================================================================
//
// The implementation above is Windows x86-64 only: it hand-encodes an absolute
// jump and reads the exception directory through `RtlLookupFunctionEntry`, and
// neither has a portable equivalent. Declaring those imports unconditionally
// broke the build for every other target, which is why this exists.
//
// The stubs refuse rather than pretend. Everything that does not depend on
// rewriting live code - the dispatch slots, the registries, whole-module
// reloading - is portable Rust and works unchanged, so a non-Windows target
// keeps `#[pill_hot]` and `#[pill_hot_fn]` and loses only the macro-free route.

/// Where the function containing `address` begins and how long it is.
///
/// Always `None` off Windows x86-64: no supported way to establish a function's
/// extent, and guessing is how a patch corrupts the function after it.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn function_extent(_address: usize) -> Option<(usize, usize)> {
    None
}

/// Overwrite a function's first bytes with a jump to `replacement`.
///
/// # Errors
///
/// Always, off Windows x86-64. Annotating the function with `#[pill_hot_fn]`
/// gives it a dispatch slot, which is portable and needs no code rewriting.
///
/// # Safety
///
/// Nothing is written, so there is no contract to uphold - the signature
/// matches the Windows one so callers need no `cfg` of their own.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub unsafe fn patch_prologue(_target: usize, _replacement: usize) -> Result<Vec<u8>, String> {
    Err(
        "prologue patching is implemented for Windows x86-64 only; annotate the function \
         with #[pill_hot_fn] to patch it through a dispatch slot instead"
            .to_string(),
    )
}

/// Write saved prologue bytes back.
///
/// # Errors
///
/// Always, off Windows x86-64: nothing can have been patched, so there is
/// nothing to restore.
///
/// # Safety
///
/// Nothing is written, so there is no contract to uphold.
#[cfg(not(all(windows, target_arch = "x86_64")))]
pub unsafe fn restore_prologue(_target: usize, _original: &[u8]) -> Result<(), String> {
    Err("prologue patching is implemented for Windows x86-64 only".to_string())
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

    #[test]
    fn registry_reports_nothing_for_an_unknown_name() {
        assert!(resolve_hot_function("nothing::declares::this").is_none());
    }
}

/// End-to-end checks for the macro-free prologue route: patching real function
/// bodies in this binary, restoring them, and refusing the addresses that
/// cannot be patched safely.
///
/// Behind the same `hot_patch` feature these carried in `pill_engine`, so the
/// set of tests that run in each build configuration is unchanged by the move.
#[cfg(all(test, feature = "hot_patch"))]
mod prologue_tests {
    use super::*;

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
        let generation_one =
            unsafe { patch_prologue(target, first_replacement) }.expect("first patch");
        assert_eq!(std::hint::black_box(patch_target_twice)(1), 1001);

        // SAFETY: as above. What this returns is generation one's jump.
        let generation_two =
            unsafe { patch_prologue(target, second_replacement) }.expect("second patch");
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

    /// A restore is refused when the bytes there are not the jump this module
    /// installed.
    ///
    /// Extent alone is not enough: an image can be unloaded and another mapped
    /// over the same address, and the extent lookup would succeed. Writing the
    /// saved bytes then corrupts an unrelated live function - during a rollback,
    /// when the developer is already recovering from something.
    #[test]
    fn restoring_over_code_that_is_not_our_jump_is_refused() {
        let target = patch_restore_guard as *const () as usize;
        assert_eq!(std::hint::black_box(patch_restore_guard)(1), 3);

        // Bytes shaped like a saved prologue, but the function was never
        // patched, so what is there now is its own code.
        let pretend_original = vec![0x90u8; ABSOLUTE_JUMP_LENGTH];

        // SAFETY: expected to refuse before writing, which is what this asserts.
        let result = unsafe { restore_prologue(target, &pretend_original) };
        let detail = result.expect_err("an unpatched function must be refused");
        assert!(
            detail.contains("does not begin with the jump"),
            "unexpected reason: {detail}"
        );
        assert_eq!(
            std::hint::black_box(patch_restore_guard)(1),
            3,
            "a refused restore must not have written anything"
        );
    }

    /// Its own target, so a refused restore cannot disturb the other tests.
    #[inline(never)]
    fn patch_restore_guard(value: u32) -> u32 {
        value + 2
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
}
