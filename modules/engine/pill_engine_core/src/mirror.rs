//! The C ABI every mirrored Rust function crosses to reach C#.
//!
//! # Responsibilities
//!
//! - Define the one trampoline shape every `#[pill_mirror_*]` attribute emits:
//!   `extern "C" fn(args: *const u8, ret: *mut u8) -> u8`, with each argument in
//!   its own 16-byte slot and the result written to `ret`.
//! - Carry what a status byte cannot: the error message of a failed call and a
//!   returned string, through two thread-local channels the C# bridge drains.
//! - Read argument slots and write results for every type the mirror
//!   vocabulary supports, through [`MirrorValue`] and [`MirrorReturn`].
//! - Implement the asset store operations every mirrored asset type shares
//!   ([`assets`]), so the generated per-type trampolines are one call each.
//!
//! # Design
//!
//! One trampoline signature, whatever the Rust function looks like, is what
//! lets the managed side call every mirrored function through one C-ABI
//! function pointer type: no per-signature delegate, no marshalling stub, and
//! nothing NativeAOT has to generate at build time. The price is that
//! arguments are packed by the caller and unpacked here, which is a handful of
//! unaligned reads per call.
//!
//! Slot layout, the contract with `TracyLive.MirrorCall` in the C# runtime:
//!
//! | Tag | Slot contents |
//! | --- | --- |
//! | primitive | the value at offset 0, little endian; `bool` is one byte |
//! | `str` | UTF-8 pointer at 0, byte length (`u32`) at 8 |
//! | `slice:<element>` | element pointer at 0, element count (`u32`) at 8 |
//! | `ref:`/`mut:` | the address of the value at 0 |
//! | named value type | see its [`MirrorValue`] implementation |
//! | `option:<inner>` | the inner encoding, presence byte at 15 |
//!
//! A returned value is written to `ret` the same way, except `option:` puts
//! its presence byte at offset 0 and the value at offset 16, and a string goes
//! through the return channel instead of `ret`.
//!
//! The thread-local channels live in this crate, the engine dylib every module
//! and the host link once, so a module's trampoline and the bridge that drains
//! the channel see the same thread-local.

// Standard library
use std::cell::RefCell;
use std::path::Path;

// External crates
use trait_type_map::TraitAccessible;

// Current crate
use crate::asset::{Asset, AssetGuid, AssetLoader, AssetManager, Handle};
use crate::asset_metadata::{AssetImport, ImportedAsset, MetadataPolicy, MetadataSource};
use crate::asset_standalone::StandaloneAsset;

// =============================================================================
// Constants
// =============================================================================

/// Width of one argument slot, in bytes.
pub const SLOT_SIZE: usize = 16;

/// The call succeeded and wrote its result.
pub const STATUS_OK: u8 = 0;

/// The call failed; the message is in the last-error channel.
pub const STATUS_ERROR: u8 = 1;

/// Offset of an `option:` argument's presence byte inside its slot.
pub const OPTION_ARGUMENT_FLAG_OFFSET: usize = 15;

/// Offset of an `option:` return's value inside the return buffer; the
/// presence byte is at offset 0.
pub const OPTION_RETURN_VALUE_OFFSET: usize = 16;

/// What a failed mirrored call reports.
pub type MirrorResult<T> = Result<T, String>;

// =============================================================================
// Channels
// =============================================================================

thread_local! {
    /// The message of the last failed mirrored call on this thread.
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The string the last mirrored call on this thread returned.
    static RETURN_STRING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Record why the current mirrored call failed.
pub fn set_last_error(message: impl Into<String>) {
    let message = message.into();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(message));
}

/// Take the message the last failed call recorded on this thread.
pub fn take_last_error() -> Option<String> {
    LAST_ERROR.with(|slot| slot.borrow_mut().take())
}

/// Record the string the current mirrored call returns.
pub fn set_return_string(value: String) {
    RETURN_STRING.with(|slot| *slot.borrow_mut() = Some(value));
}

/// Take the string the last mirrored call on this thread returned.
pub fn take_return_string() -> Option<String> {
    RETURN_STRING.with(|slot| slot.borrow_mut().take())
}

// =============================================================================
// Invocation
// =============================================================================

/// Run one trampoline body and fold its outcome into a status byte.
///
/// An `Err` is recorded in the last-error channel; a panic is caught and
/// recorded the same way, because unwinding out of an `extern "C"` function
/// aborts the process and the managed caller can do better with a message.
pub fn invoke(body: impl FnOnce() -> MirrorResult<()>) -> u8 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => STATUS_OK,
        Ok(Err(message)) => {
            set_last_error(message);
            STATUS_ERROR
        }
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a non-string panic payload".to_string());
            set_last_error(format!("the Rust function panicked: {detail}"));
            STATUS_ERROR
        }
    }
}

/// Turn any displayable error into a mirrored failure.
pub fn error_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

// =============================================================================
// Slots
// =============================================================================

/// Address of argument slot `index`.
///
/// # Safety
///
/// `args` must point at a buffer of at least `index + 1` slots.
pub unsafe fn slot(args: *const u8, index: usize) -> *const u8 {
    // SAFETY: the caller's contract.
    unsafe { args.add(index * SLOT_SIZE) }
}

/// Read a plain value from the start of a slot or element.
///
/// # Safety
///
/// `at` must reference `size_of::<T>()` readable bytes holding a valid `T`.
pub unsafe fn read<T: Copy>(at: *const u8) -> T {
    // SAFETY: the caller's contract; unaligned because slots are packed.
    unsafe { std::ptr::read_unaligned(at.cast::<T>()) }
}

/// Read a `bool` (one byte, nonzero is true).
///
/// # Safety
///
/// `at` must reference one readable byte.
pub unsafe fn read_bool(at: *const u8) -> bool {
    // SAFETY: the caller's contract.
    unsafe { *at != 0 }
}

/// Read the address a slot carries.
///
/// # Safety
///
/// `at` must reference eight readable bytes.
pub unsafe fn read_pointer(at: *const u8) -> *mut u8 {
    // SAFETY: the caller's contract.
    unsafe { read::<usize>(at) as *mut u8 }
}

/// Read the `(pointer, length)` pair a `str` or `slice:` slot carries.
///
/// # Safety
///
/// `at` must reference twelve readable bytes.
pub unsafe fn read_span(at: *const u8) -> (*const u8, usize) {
    // SAFETY: the caller's contract.
    unsafe {
        (
            read_pointer(at) as *const u8,
            read::<u32>(at.add(8)) as usize,
        )
    }
}

/// Borrow the value a `ref:` slot addresses.
///
/// # Safety
///
/// The slot must hold null or the address of a live `T` that nothing else
/// mutates for `'a`.
pub unsafe fn read_ref<'a, T>(at: *const u8, what: &str) -> MirrorResult<&'a T> {
    // SAFETY: the caller's contract.
    let pointer = unsafe { read_pointer(at) } as *const T;
    // SAFETY: the caller's contract.
    unsafe { pointer.as_ref() }.ok_or_else(|| format!("{what} was null"))
}

/// Borrow the value a `mut:` slot addresses, mutably.
///
/// # Safety
///
/// The slot must hold null or the address of a live `T` no one else uses for
/// `'a`.
pub unsafe fn read_mut<'a, T>(at: *const u8, what: &str) -> MirrorResult<&'a mut T> {
    // SAFETY: the caller's contract.
    let pointer = unsafe { read_pointer(at) } as *mut T;
    // SAFETY: the caller's contract.
    unsafe { pointer.as_mut() }.ok_or_else(|| format!("{what} was null"))
}

/// Read a `str` slot as a borrowed string, checking it is UTF-8.
///
/// # Safety
///
/// The slot must describe `length` readable bytes that stay valid for `'a`.
pub unsafe fn read_str<'a>(at: *const u8) -> MirrorResult<&'a str> {
    // SAFETY: the caller's contract.
    let (pointer, length) = unsafe { read_span(at) };
    if length == 0 {
        return Ok("");
    }
    if pointer.is_null() {
        return Err("a string argument was null".to_string());
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
    std::str::from_utf8(bytes).map_err(|error| format!("a string argument is not UTF-8: {error}"))
}

/// Read a `slice:` slot of plain elements into an owned vector.
///
/// Copied rather than borrowed: the managed caller packs elements without any
/// alignment promise, and a vector is what most Rust APIs take anyway.
///
/// # Safety
///
/// The slot must describe `count` readable, valid `T` values.
pub unsafe fn read_plain_vec<T: Copy>(at: *const u8) -> MirrorResult<Vec<T>> {
    // SAFETY: the caller's contract.
    let (pointer, count) = unsafe { read_span(at) };
    if count == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err("a slice argument was null".to_string());
    }
    let size = std::mem::size_of::<T>();
    // SAFETY: the caller's contract; every element is read unaligned.
    Ok((0..count)
        .map(|index| unsafe { read::<T>(pointer.add(index * size)) })
        .collect())
}

/// Read a `slice:str` slot into owned strings.
///
/// # Safety
///
/// The slot must describe `count` readable `(pointer, length)` pairs, each
/// describing readable bytes.
pub unsafe fn read_string_vec(at: *const u8) -> MirrorResult<Vec<String>> {
    // SAFETY: the caller's contract.
    let (pointer, count) = unsafe { read_span(at) };
    if count == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err("a slice argument was null".to_string());
    }
    (0..count)
        // SAFETY: each element is one slot-shaped `(pointer, length)` pair.
        .map(|index| unsafe { read_str(pointer.add(index * SLOT_SIZE)) }.map(str::to_owned))
        .collect()
}

/// Read a `slice:` slot of named values into an owned vector.
///
/// # Safety
///
/// The slot must describe `count` elements laid out as `T`'s
/// [`MirrorValue::ELEMENT_SIZE`] promises.
pub unsafe fn read_value_vec<T: MirrorValue>(at: *const u8) -> MirrorResult<Vec<T>> {
    // SAFETY: the caller's contract.
    let (pointer, count) = unsafe { read_span(at) };
    if count == 0 {
        return Ok(Vec::new());
    }
    if pointer.is_null() {
        return Err("a slice argument was null".to_string());
    }
    (0..count)
        // SAFETY: the caller's contract.
        .map(|index| unsafe { T::read_element(pointer.add(index * T::ELEMENT_SIZE)) })
        .collect()
}

/// Read an `option:` slot, decoding the inner value only when present.
///
/// # Safety
///
/// The slot must hold the inner encoding and a presence byte at
/// [`OPTION_ARGUMENT_FLAG_OFFSET`].
pub unsafe fn read_option<T>(
    at: *const u8,
    inner: impl FnOnce(*const u8) -> MirrorResult<T>,
) -> MirrorResult<Option<T>> {
    // SAFETY: the caller's contract.
    if unsafe { *at.add(OPTION_ARGUMENT_FLAG_OFFSET) } == 0 {
        return Ok(None);
    }
    inner(at).map(Some)
}

/// Write a plain value to the start of a return buffer.
///
/// # Safety
///
/// `ret` must reference `size_of::<T>()` writable bytes.
pub unsafe fn write<T: Copy>(ret: *mut u8, value: T) {
    // SAFETY: the caller's contract; unaligned for the same reason as `read`.
    unsafe { std::ptr::write_unaligned(ret.cast::<T>(), value) }
}

/// Write an `option:` result: the presence byte, and the value when present.
///
/// # Safety
///
/// `ret` must reference at least [`OPTION_RETURN_VALUE_OFFSET`] writable bytes
/// plus what `inner` writes.
pub unsafe fn write_option<T>(ret: *mut u8, value: Option<T>, inner: impl FnOnce(*mut u8, T)) {
    match value {
        // SAFETY: the caller's contract.
        Some(value) => unsafe {
            *ret = 1;
            inner(ret.add(OPTION_RETURN_VALUE_OFFSET), value);
        },
        // SAFETY: the caller's contract.
        None => unsafe { *ret = 0 },
    }
}

// =============================================================================
// Named Values
// =============================================================================

/// A type a mirrored call takes by value, read from a slot or a slice element.
///
/// Implemented by the mirror attributes for the types they declare -
/// `#[pill_mirror_object]` boxes, `#[derive(PillMirror)]` value types and
/// enums - and here for the engine types the vocabulary names directly.
///
/// # Safety
///
/// An implementation must read exactly the encoding the C# runtime writes for
/// the type, which is what the codegen's tag for it selects.
pub unsafe trait MirrorValue: Sized {
    /// Stride of one element in a `slice:` of this type.
    const ELEMENT_SIZE: usize;

    /// Read the value an argument slot carries.
    ///
    /// # Safety
    ///
    /// `slot` must reference a full slot written for this type.
    unsafe fn read_slot(slot: *const u8) -> MirrorResult<Self>;

    /// Read one element of a `slice:` of this type.
    ///
    /// # Safety
    ///
    /// `element` must reference [`Self::ELEMENT_SIZE`] bytes written for this
    /// type.
    unsafe fn read_element(element: *const u8) -> MirrorResult<Self>;
}

/// A type a mirrored call returns by value.
///
/// # Safety
///
/// An implementation must write exactly the encoding the C# runtime reads for
/// the type, and no more bytes than the return buffer it reserves for it.
pub unsafe trait MirrorReturn: Sized {
    /// Write the value to a return buffer.
    ///
    /// # Safety
    ///
    /// `ret` must reference the bytes the managed side reserved for this type.
    unsafe fn write_return(self, ret: *mut u8);
}

/// A Rust value C# holds by pointer: a boxed `T` it creates, passes and drops.
///
/// Implemented by `#[pill_mirror_object]`, which is what gives the type its
/// drop trampoline and the [`MirrorValue`] / [`MirrorReturn`] implementations
/// below; the helpers here are the one place the box crosses the boundary.
pub trait MirrorObject: Sized + Send + 'static {}

/// Move an object out of the box a slot or element addresses.
///
/// # Safety
///
/// `at` must hold null or a pointer from [`object_into_raw`] that the managed
/// side has given up, so nothing reads it again.
pub unsafe fn object_from_raw<T: MirrorObject>(at: *const u8) -> MirrorResult<T> {
    // SAFETY: the caller's contract.
    let pointer = unsafe { read_pointer(at) } as *mut T;
    if pointer.is_null() {
        return Err(format!(
            "a {} argument was null or already moved",
            std::any::type_name::<T>()
        ));
    }
    // SAFETY: the pointer came from `Box::into_raw` and is owned by this call.
    Ok(*unsafe { Box::from_raw(pointer) })
}

/// Box an object for the managed side and return its address.
pub fn object_into_raw<T: MirrorObject>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

/// Drop the boxed object a slot addresses; the body of every drop trampoline.
///
/// # Safety
///
/// As [`object_from_raw`].
pub unsafe fn drop_object<T: MirrorObject>(args: *const u8) -> u8 {
    invoke(|| {
        // SAFETY: the caller's contract.
        drop(unsafe { object_from_raw::<T>(slot(args, 0)) }?);
        Ok(())
    })
}

// SAFETY: two `u32`s, the layout `TracyLive.Handle<T>` declares.
unsafe impl<T: Asset> MirrorValue for Handle<T> {
    const ELEMENT_SIZE: usize = 8;

    unsafe fn read_slot(slot: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        unsafe { Self::read_element(slot) }
    }

    unsafe fn read_element(element: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        let (index, generation) = unsafe { (read::<u32>(element), read::<u32>(element.add(4))) };
        Ok(Handle::from_raw(index, generation))
    }
}

// SAFETY: as the `MirrorValue` implementation.
unsafe impl<T: Asset> MirrorReturn for Handle<T> {
    unsafe fn write_return(self, ret: *mut u8) {
        // SAFETY: the caller's contract.
        unsafe {
            write(ret, self.index());
            write(ret.add(4), self.generation());
        }
    }
}

/// `TracyLive.AssetLoader`'s encoding: data pointer at 0, length at 8, and the
/// kind at 12 (`0` a path below `res`, `1` the bytes themselves).
// SAFETY: the layout `MirrorCall.Loader` writes.
unsafe impl MirrorValue for AssetLoader {
    const ELEMENT_SIZE: usize = SLOT_SIZE;

    unsafe fn read_slot(slot: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        unsafe { Self::read_element(slot) }
    }

    unsafe fn read_element(element: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        let kind = unsafe { *element.add(12) };
        match kind {
            // SAFETY: the caller's contract.
            0 => Ok(AssetLoader::Path(unsafe { read_str(element) }?.into())),
            1 => {
                // SAFETY: the caller's contract.
                let (pointer, length) = unsafe { read_span(element) };
                if length > 0 && pointer.is_null() {
                    return Err("an asset loader's bytes were null".to_string());
                }
                let bytes = if length == 0 {
                    Box::default()
                } else {
                    // SAFETY: the caller's contract.
                    unsafe { std::slice::from_raw_parts(pointer, length) }.into()
                };
                Ok(AssetLoader::Bytes(bytes))
            }
            other => Err(format!("unknown asset loader kind {other}")),
        }
    }
}

macro_rules! plain_mirror_value {
    ($($ty:ty),*) => {$(
        // SAFETY: a primitive is its own little-endian bytes.
        unsafe impl MirrorValue for $ty {
            const ELEMENT_SIZE: usize = std::mem::size_of::<$ty>();

            unsafe fn read_slot(slot: *const u8) -> MirrorResult<Self> {
                // SAFETY: the caller's contract.
                Ok(unsafe { read::<$ty>(slot) })
            }

            unsafe fn read_element(element: *const u8) -> MirrorResult<Self> {
                // SAFETY: the caller's contract.
                Ok(unsafe { read::<$ty>(element) })
            }
        }

        // SAFETY: as above.
        unsafe impl MirrorReturn for $ty {
            unsafe fn write_return(self, ret: *mut u8) {
                // SAFETY: the caller's contract.
                unsafe { write(ret, self) }
            }
        }
    )*};
}

plain_mirror_value!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, usize, isize);

// SAFETY: one byte, nonzero is true.
unsafe impl MirrorValue for bool {
    const ELEMENT_SIZE: usize = 1;

    unsafe fn read_slot(slot: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        Ok(unsafe { read_bool(slot) })
    }

    unsafe fn read_element(element: *const u8) -> MirrorResult<Self> {
        // SAFETY: the caller's contract.
        Ok(unsafe { read_bool(element) })
    }
}

// SAFETY: one byte.
unsafe impl MirrorReturn for bool {
    unsafe fn write_return(self, ret: *mut u8) {
        // SAFETY: the caller's contract.
        unsafe { *ret = u8::from(self) }
    }
}

// =============================================================================
// Asset Store Operations
// =============================================================================

/// The asset store operations every mirrored asset type shares.
///
/// `#[pill_mirror_object(asset)]` emits one trampoline per operation per type,
/// each a single call into here, and the C# runtime's `AssetManager`
/// extension methods reach them by name (`__asset_add`, ...). Slot 0 always
/// holds the address of the world's `AssetManager`, which the managed side
/// fetched under the running system's declared access.
pub mod assets {
    use super::*;

    /// The `AssetManager` a slot addresses.
    ///
    /// # Safety
    ///
    /// The slot must hold null or the address of the world's live store, used
    /// by nothing else for the call.
    unsafe fn store<'a>(args: *const u8) -> MirrorResult<&'a mut AssetManager> {
        // SAFETY: the caller's contract.
        unsafe { read_mut::<AssetManager>(slot(args, 0), "the AssetManager") }
    }

    /// `assets.add(value)`: slot 1 the value; returns its handle.
    ///
    /// # Safety
    ///
    /// The trampoline contract: `args` as described, `ret` 16 writable bytes.
    pub unsafe fn add<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset> + MirrorValue,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let value = T::read_slot(slot(args, 1))?;
                assets.add(value).write_return(ret);
            }
            Ok(())
        })
    }

    /// `assets.add_named(name, value)`: slot 1 the name, slot 2 the value.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn add_named<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset> + MirrorValue,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let name = read_str(slot(args, 1))?;
                let value = T::read_slot(slot(args, 2))?;
                assets
                    .add_named(name, value)
                    .map_err(error_message)?
                    .write_return(ret);
            }
            Ok(())
        })
    }

    /// `assets.add_named_with_guid(name, guid, value)`: slot 1 the name, slot 2
    /// the guid as 32 hexadecimal digits, slot 3 the value.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn add_named_with_guid<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset> + MirrorValue,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let name = read_str(slot(args, 1))?;
                let guid = parse_guid(read_str(slot(args, 2))?)?;
                let value = T::read_slot(slot(args, 3))?;
                assets
                    .add_named_with_guid(name, guid, value)
                    .map_err(error_message)?
                    .write_return(ret);
            }
            Ok(())
        })
    }

    /// `assets.remove(handle)`: slot 1 the handle; returns the removed value's
    /// box, or null when the handle was stale.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn remove<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset> + MirrorObject,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let handle = Handle::<T>::read_slot(slot(args, 1))?;
                let removed = assets
                    .remove(handle)
                    .map_or(std::ptr::null_mut(), object_into_raw);
                write(ret, removed as usize);
            }
            Ok(())
        })
    }

    /// `assets.contains(handle)`: slot 1 the handle; returns a `bool`.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn contains<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let handle = Handle::<T>::read_slot(slot(args, 1))?;
                assets.contains(handle).write_return(ret);
            }
            Ok(())
        })
    }

    /// `assets.handle_by_name(name)`: slot 1 the name; returns an
    /// `option:handle`.
    ///
    /// # Safety
    ///
    /// As [`add`], with `ret` holding an `option:` result.
    pub unsafe fn handle_by_name<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let name = read_str(slot(args, 1))?;
                write_option(ret, assets.handle_by_name::<T>(name), |at, handle| {
                    handle.write_return(at)
                });
            }
            Ok(())
        })
    }

    /// `assets.handle_by_guid(guid)`: slot 1 the guid as hexadecimal; returns
    /// an `option:handle`.
    ///
    /// # Safety
    ///
    /// As [`handle_by_name`].
    pub unsafe fn handle_by_guid<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let guid = parse_guid(read_str(slot(args, 1))?)?;
                write_option(ret, assets.handle_by_guid::<T>(guid), |at, handle| {
                    handle.write_return(at)
                });
            }
            Ok(())
        })
    }

    /// `assets.guid_of(handle)`: slot 1 the handle; returns the guid as a
    /// string (empty when the asset has none).
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn guid_of<T>(args: *const u8, _ret: *mut u8) -> u8
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let handle = Handle::<T>::read_slot(slot(args, 1))?;
                set_return_string(
                    assets
                        .guid_of(handle)
                        .map(|guid| guid.to_string())
                        .unwrap_or_default(),
                );
            }
            Ok(())
        })
    }

    /// `assets.import(path, policy, settings)`: slot 1 the path below `res`,
    /// slot 2 the policy (`0` read if present, `1` create if missing), slot 3
    /// the initial settings as JSON (empty for the type's defaults).
    ///
    /// Writes the handle at 0, whether the path was already loaded at 8 and
    /// where the guid came from at 9 (`0` a file, `1` written now, `2` memory
    /// only); returns the guid through the string channel.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn import<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: ImportedAsset,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let path = read_str(slot(args, 1))?;
                let policy = match read::<u8>(slot(args, 2)) {
                    0 => MetadataPolicy::ReadIfPresent,
                    1 => MetadataPolicy::CreateIfMissing,
                    other => return Err(format!("unknown metadata policy {other}")),
                };
                let settings = read_str(slot(args, 3))?;
                let mut request = AssetImport::<T>::new(path, policy);
                if !settings.trim().is_empty() {
                    let initial = serde_json::from_str(settings).map_err(|error| {
                        format!(
                            "the initial settings do not fit {}: {error}",
                            T::metadata_type_name()
                        )
                    })?;
                    request = request.with_initial_settings(initial);
                }
                let outcome = assets
                    .import(request)
                    .map_err(|error| format!("import of `{path}` failed: {error}"))?;
                outcome.handle.write_return(ret);
                *ret.add(8) = u8::from(outcome.already_loaded);
                *ret.add(9) = metadata_source_code(outcome.metadata);
                set_return_string(outcome.guid.to_string());
            }
            Ok(())
        })
    }

    /// `assets.import_standalone(path)`: slot 1 the path below `res`; writes
    /// what [`import`] writes.
    ///
    /// # Safety
    ///
    /// As [`add`].
    pub unsafe fn import_standalone<T>(args: *const u8, ret: *mut u8) -> u8
    where
        T: StandaloneAsset,
    {
        invoke(|| {
            // SAFETY: the caller's contract.
            unsafe {
                let assets = store(args)?;
                let path = read_str(slot(args, 1))?;
                let outcome = assets
                    .import_standalone::<T>(Path::new(path))
                    .map_err(|error| format!("import of `{path}` failed: {error}"))?;
                outcome.handle.write_return(ret);
                *ret.add(8) = u8::from(outcome.already_loaded);
                *ret.add(9) = metadata_source_code(outcome.metadata);
                set_return_string(outcome.guid.to_string());
            }
            Ok(())
        })
    }

    /// Parse a guid the managed side wrote as 32 hexadecimal digits.
    fn parse_guid(text: &str) -> MirrorResult<AssetGuid> {
        AssetGuid::parse(text).ok_or_else(|| format!("`{text}` is not a 32-digit hexadecimal guid"))
    }

    /// The byte a metadata source crosses as.
    fn metadata_source_code(source: MetadataSource) -> u8 {
        match source {
            MetadataSource::ReadFromFile => 0,
            MetadataSource::CreatedOnDisk => 1,
            MetadataSource::InMemoryOnly => 2,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A test object: a heap-owning value the managed side would hold by box.
    #[derive(Debug, PartialEq)]
    struct Label(String);
    impl MirrorObject for Label {}

    /// Pack slots the way the managed runtime does.
    fn slots(count: usize) -> Vec<u8> {
        vec![0; count * SLOT_SIZE]
    }

    #[test]
    fn an_error_and_a_panic_both_become_status_one_with_a_message() {
        assert_eq!(invoke(|| Ok(())), STATUS_OK);
        assert_eq!(take_last_error(), None);

        assert_eq!(invoke(|| Err("no such mesh".to_string())), STATUS_ERROR);
        assert_eq!(take_last_error().as_deref(), Some("no such mesh"));

        assert_eq!(invoke(|| panic!("boom")), STATUS_ERROR);
        let message = take_last_error().expect("a panic records a message");
        assert!(message.contains("boom"), "{message}");
        assert_eq!(take_last_error(), None, "taking the message clears it");
    }

    #[test]
    fn strings_and_plain_slices_read_from_their_slots() {
        let text = "pill";
        let numbers = [1.5f32, -2.0, 4.25];
        let mut args = slots(2);
        // SAFETY: two slots, each written within its sixteen bytes.
        unsafe {
            write(args.as_mut_ptr(), text.as_ptr() as usize);
            write(args.as_mut_ptr().add(8), text.len() as u32);
            write(args.as_mut_ptr().add(16), numbers.as_ptr() as usize);
            write(args.as_mut_ptr().add(24), numbers.len() as u32);
            assert_eq!(read_str(slot(args.as_ptr(), 0)), Ok("pill"));
            assert_eq!(
                read_plain_vec::<f32>(slot(args.as_ptr(), 1)),
                Ok(numbers.to_vec())
            );
        }
    }

    #[test]
    fn invalid_utf8_is_refused_with_a_message() {
        let bytes = [0xff_u8, 0xfe];
        let mut args = slots(1);
        // SAFETY: one slot.
        unsafe {
            write(args.as_mut_ptr(), bytes.as_ptr() as usize);
            write(args.as_mut_ptr().add(8), bytes.len() as u32);
            assert!(read_str(slot(args.as_ptr(), 0)).is_err());
        }
    }

    #[test]
    fn an_object_round_trips_through_its_box_and_cannot_be_read_twice() {
        let raw = object_into_raw(Label("cube".to_string()));
        let mut args = slots(1);
        // SAFETY: one slot holding the box, read once; the null read after it
        // is refused rather than dereferenced.
        unsafe {
            write(args.as_mut_ptr(), raw as usize);
            assert_eq!(
                object_from_raw::<Label>(slot(args.as_ptr(), 0)),
                Ok(Label("cube".to_string()))
            );
            write(args.as_mut_ptr(), 0usize);
            assert!(object_from_raw::<Label>(slot(args.as_ptr(), 0)).is_err());
        }
    }

    #[test]
    fn handles_and_loaders_decode_their_encodings() {
        struct Mesh;
        impl Asset for Mesh {}
        trait_type_map::impl_trait_accessible!(dyn Asset; Mesh);

        let path = "models/cube.obj";
        let mut args = slots(2);
        // SAFETY: two slots, each written within its sixteen bytes.
        unsafe {
            write(args.as_mut_ptr(), 7u32);
            write(args.as_mut_ptr().add(4), 2u32);
            write(args.as_mut_ptr().add(16), path.as_ptr() as usize);
            write(args.as_mut_ptr().add(24), path.len() as u32);
            *args.as_mut_ptr().add(28) = 0;
            let handle = Handle::<Mesh>::read_slot(slot(args.as_ptr(), 0)).unwrap();
            assert_eq!((handle.index(), handle.generation()), (7, 2));
            let loader = AssetLoader::read_slot(slot(args.as_ptr(), 1)).unwrap();
            assert!(matches!(loader, AssetLoader::Path(found) if found == Path::new(path)));
        }
    }

    #[test]
    fn an_absent_option_skips_the_inner_read() {
        let mut args = slots(1);
        // SAFETY: one slot; the presence byte is inside it.
        unsafe {
            write(args.as_mut_ptr(), 9u32);
            assert_eq!(
                read_option(slot(args.as_ptr(), 0), |at| u32::read_slot(at)),
                Ok(None)
            );
            *args.as_mut_ptr().add(OPTION_ARGUMENT_FLAG_OFFSET) = 1;
            assert_eq!(
                read_option(slot(args.as_ptr(), 0), |at| u32::read_slot(at)),
                Ok(Some(9))
            );
        }
    }
}
