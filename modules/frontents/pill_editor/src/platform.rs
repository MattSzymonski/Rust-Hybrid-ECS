//! The executable's platform layer (D28): the Windows clipboard and
//! drag-and-drop today, the seam Linux and macOS fill in later.
//!
//! # Responsibilities
//! - Write and read Unicode text (CF_UNICODETEXT).
//! - Write and read one registered custom format alongside that text, so the
//!   shell's structured payloads ([`pill_editor_core::clipboard`]) survive a
//!   trip through the system clipboard and still paste as readable JSON into
//!   any other application.
//! - Drag a file out to other applications (CF_HDROP through OLE's
//!   `DoDragDrop`), so an asset row can be dropped into Explorer.
//!
//! The Win32 entry points are declared here by hand rather than pulled from a
//! crate with `windows-sys`, whose features would unify with the modules'
//! dependency graph and split the shared engine crates into differently
//! featured variants - the same reason the host's job objects are declared by
//! hand. The drag-source COM interfaces are hand-rolled for the same reason:
//! three interfaces over one object, as small as the drag machinery accepts.
//!
//! Every call is best-effort: a clipboard another process is holding fails
//! the operation and the caller reports it; nothing here blocks or panics
//! except `drag_out`, which runs OLE's modal drag loop by definition and says
//! so.

/// How a drag out of the window ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    /// A drop target took the file (or the effect was applied).
    Dropped,
    /// The user cancelled, or nothing under the cursor accepted the file.
    Cancelled,
    /// OLE ended the drag another way; the raw result code.
    Other(i32),
}

#[cfg(windows)]
mod win32 {
    // Standard library
    use std::ffi::c_void;

    /// `CF_UNICODETEXT`: a zero-terminated UTF-16 buffer in an `HGLOBAL`.
    const CF_UNICODETEXT: u32 = 13;
    /// `GMEM_MOVEABLE`: the allocation model every clipboard format uses.
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn GetClipboardData(format: u32) -> isize;
        fn SetClipboardData(format: u32, memory: isize) -> isize;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(handle: isize) -> *mut c_void;
        fn GlobalUnlock(handle: isize) -> i32;
        fn GlobalFree(handle: isize) -> isize;
        fn GlobalSize(handle: isize) -> usize;
    }

    /// Put `text` on the clipboard as Unicode text.
    pub fn set_text(text: &str) -> Result<(), String> {
        // SAFETY: a null owner handle is the documented contract; the call
        // touches no Rust memory.
        let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
        if opened == 0 {
            return Err("the clipboard is held by another application".to_string());
        }
        // SAFETY: the clipboard is open on this thread and owned here.
        let mut outcome = unsafe { EmptyClipboard() } != 0;
        if outcome {
            // Step 1: the UTF-16 buffer with its terminating NUL.
            let mut units: Vec<u16> = text.encode_utf16().collect();
            units.push(0);
            outcome = store_bytes(CF_UNICODETEXT, bytemuck_bytes(&units));
        }
        // SAFETY: closes the clipboard this function opened.
        unsafe { CloseClipboard() };
        if outcome {
            Ok(())
        } else {
            Err("SetClipboardData refused the text".to_string())
        }
    }

    /// Read the clipboard's Unicode text, when it holds any.
    pub fn text() -> Option<String> {
        // SAFETY: every call is the clipboard API on the open clipboard; the
        // slice is built from the locked global's own pointer and length and
        // is copied out before the global is unlocked.
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                return None;
            }
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            let mut result = None;
            if handle != 0 {
                let pointer = GlobalLock(handle);
                if !pointer.is_null() {
                    let units = GlobalSize(handle) / 2;
                    let buffer = std::slice::from_raw_parts(pointer as *const u16, units);
                    let length = buffer
                        .iter()
                        .position(|unit| *unit == 0)
                        .unwrap_or(buffer.len());
                    result = Some(String::from_utf16_lossy(&buffer[..length]));
                    GlobalUnlock(handle);
                }
            }
            CloseClipboard();
            result
        }
    }

    /// Put `bytes` on the clipboard under the registered `format`, with
    /// `text` offered alongside so plain editors paste something readable.
    pub fn set_object(format: &str, bytes: &[u8], text: &str) -> Result<(), String> {
        let format_id = register_format(format);
        if format_id == 0 {
            return Err(format!("the clipboard refused to register `{format}`"));
        }
        // SAFETY: a null owner handle is the documented contract; the call
        // touches no Rust memory.
        let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
        if opened == 0 {
            return Err("the clipboard is held by another application".to_string());
        }
        // SAFETY: the clipboard is open on this thread and owned here.
        let mut outcome = unsafe { EmptyClipboard() } != 0;
        if outcome {
            let mut units: Vec<u16> = text.encode_utf16().collect();
            units.push(0);
            outcome = store_bytes(CF_UNICODETEXT, bytemuck_bytes(&units));
        }
        if outcome {
            outcome = store_bytes(format_id, bytes);
        }
        // SAFETY: closes the clipboard this function opened.
        unsafe { CloseClipboard() };
        if outcome {
            Ok(())
        } else {
            Err("SetClipboardData refused the payload".to_string())
        }
    }

    /// Read the payload under the registered `format`, when the owner
    /// offered one.
    pub fn object(format: &str) -> Option<Vec<u8>> {
        let format_id = register_format(format);
        if format_id == 0 {
            return None;
        }
        // SAFETY: every call is the clipboard API on the open clipboard; the
        // slice is built from the locked global's own pointer and length and
        // is copied out before the global is unlocked.
        unsafe {
            if IsClipboardFormatAvailable(format_id) == 0 {
                return None;
            }
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let handle = GetClipboardData(format_id);
            let mut result = None;
            if handle != 0 {
                let pointer = GlobalLock(handle);
                if !pointer.is_null() {
                    let length = GlobalSize(handle);
                    result =
                        Some(std::slice::from_raw_parts(pointer as *const u8, length).to_vec());
                    GlobalUnlock(handle);
                }
            }
            CloseClipboard();
            result
        }
    }

    /// The clipboard format id for `name` (stable per session, shared across
    /// processes).
    fn register_format(name: &str) -> u32 {
        let mut units: Vec<u16> = name.encode_utf16().collect();
        units.push(0);
        // SAFETY: the buffer is NUL-terminated and outlives the call.
        unsafe { RegisterClipboardFormatW(units.as_ptr()) }
    }

    /// Allocate `bytes` as a moveable global; 0 on failure. The caller owns
    /// the handle: the clipboard takes it in [`store_bytes`], a drag's target
    /// takes it out of the `STGMEDIUM`.
    fn global_from(bytes: &[u8]) -> isize {
        // SAFETY: a byte count only; the returned handle is checked before
        // any use.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) };
        if handle == 0 {
            return 0;
        }
        // SAFETY: a handle this function just allocated and still owns.
        let target = unsafe { GlobalLock(handle) };
        if target.is_null() {
            // SAFETY: the lock failed; the handle is still this function's.
            unsafe { GlobalFree(handle) };
            return 0;
        }
        // SAFETY: `target` is the locked allocation of exactly
        // `bytes.len()` bytes and cannot alias `bytes` (a fresh allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target as *mut u8, bytes.len());
            GlobalUnlock(handle);
        }
        handle
    }

    /// Allocate `bytes` as a moveable global and hand it to the system's
    /// clipboard under `format`. On success the system owns the handle; on
    /// failure the handle is freed, so nothing leaks either way.
    fn store_bytes(format: u32, bytes: &[u8]) -> bool {
        let handle = global_from(bytes);
        if handle == 0 {
            return false;
        }
        // SAFETY: `format` is a clipboard format and `handle` a moveable
        // global allocated for it; the system takes ownership on success.
        let stored = unsafe { SetClipboardData(format, handle) };
        if stored == 0 {
            // SAFETY: the system refused the handle, so it is still this
            // function's to free.
            unsafe { GlobalFree(handle) };
            return false;
        }
        true
    }

    /// The zero-copy view of a `u16` slice as bytes (the clipboard's own
    /// little-endian UTF-16 layout on every supported Windows target).
    fn bytemuck_bytes(units: &[u16]) -> &[u8] {
        // SAFETY: a `u16` slice's pointer is 2-byte aligned, so viewing it as
        // bytes is valid for `units.len() * 2` bytes for the borrow's life.
        unsafe { std::slice::from_raw_parts(units.as_ptr() as *const u8, units.len() * 2) }
    }

    // =====================================================================
    // Drag out: CF_HDROP through OLE
    // =====================================================================

    /// `CF_HDROP`: the shell's list of dropped files.
    const CF_HDROP: u16 = 15;
    /// `TYMED_HGLOBAL`: the storage `CF_HDROP` travels in.
    const TYMED_HGLOBAL: u32 = 1;
    /// `DVASPECT_CONTENT`: the only aspect a file drag has.
    const DVASPECT_CONTENT: u32 = 1;
    /// `DATADIR_GET`: the enumeration direction `EnumFormatEtc` answers.
    const DATADIR_GET: u32 = 1;
    /// `MK_LBUTTON`: the gesture is over when the button is up.
    const MK_LBUTTON: u32 = 0x0001;
    /// The effects the drag offers: a copy only. A move would let Explorer
    /// delete the project's asset, and a link to a project file is nobody's
    /// intent either.
    const DROPEFFECTS: u32 = 0x0001;

    /// `S_OK`, `S_FALSE` and the drag loop's result codes.
    const S_OK: i32 = 0;
    const S_FALSE: i32 = 1;
    const DRAGDROP_S_DROP: i32 = 0x0004_0100;
    const DRAGDROP_S_CANCEL: i32 = 0x0004_0101;
    const DRAGDROP_S_USEDEFAULTCURSORS: i32 = 0x0004_0102;

    /// `E_NOTIMPL`, `E_NOINTERFACE`, `E_OUTOFMEMORY`, `DV_E_FORMATETC` and
    /// `OLE_E_ADVISENOTSUPPORTED`: the refusals a read-only, one-format drag
    /// gives for everything but `GetData`, `QueryGetData` and
    /// `EnumFormatEtc`.
    const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
    const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
    const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;
    const DV_E_FORMATETC: i32 = 0x8004_0064u32 as i32;
    const OLE_E_ADVISENOTSUPPORTED: i32 = 0x8004_0003u32 as i32;

    #[link(name = "ole32")]
    extern "system" {
        fn OleInitialize(apartment: *mut c_void) -> i32;
        fn DoDragDrop(
            data_object: *mut c_void,
            drop_source: *mut c_void,
            allowed_effects: u32,
            effect: *mut u32,
        ) -> i32;
    }

    /// An `IID`, as the COM calls take it.
    #[repr(C)]
    #[derive(Copy, Clone, PartialEq, Eq)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    /// The tail every OLE interface id shares.
    const IID_TAIL: [u8; 8] = [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];
    const IID_IUNKNOWN: Guid = Guid {
        data1: 0,
        data2: 0,
        data3: 0,
        data4: IID_TAIL,
    };
    const IID_IDATA_OBJECT: Guid = Guid {
        data1: 0x0000_010E,
        data2: 0,
        data3: 0,
        data4: IID_TAIL,
    };
    const IID_IDROP_SOURCE: Guid = Guid {
        data1: 0x0000_0121,
        data2: 0,
        data3: 0,
        data4: IID_TAIL,
    };
    const IID_IENUM_FORMATETC: Guid = Guid {
        data1: 0x0000_0103,
        data2: 0,
        data3: 0,
        data4: IID_TAIL,
    };

    /// `FORMATETC`: one clipboard format and where the drag keeps it.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct FormatEtc {
        format: u16,
        device: *mut c_void,
        aspect: u32,
        index: i32,
        storage: u32,
    }

    /// `STGMEDIUM`: the storage a `GetData` hands the drop target.
    ///
    /// Only the `HGLOBAL` arm of the union is ever used, so the struct
    /// spells that arm out; its offset matches the real union's.
    #[repr(C)]
    struct StorageMedium {
        storage: u32,
        global: isize,
        for_release: *mut c_void,
    }

    /// The `DROPFILES` header a `CF_HDROP` payload starts with.
    #[repr(C)]
    struct DropFilesHeader {
        offset: u32,
        x: i32,
        y: i32,
        not_client: i32,
        wide: i32,
    }

    /// The `CF_HDROP` payload for `paths`: the header, then the UTF-16 file
    /// list, double-NUL terminated. Pure data - unit tested without COM.
    fn drop_files_blob(paths: &[std::path::PathBuf]) -> Vec<u8> {
        let mut units: Vec<u16> = Vec::new();
        for path in paths {
            units.extend(path.to_string_lossy().encode_utf16());
            units.push(0);
        }
        units.push(0);
        let mut blob = Vec::with_capacity(std::mem::size_of::<DropFilesHeader>() + units.len() * 2);
        blob.extend_from_slice(&(std::mem::size_of::<DropFilesHeader>() as u32).to_ne_bytes());
        blob.extend_from_slice(&0i32.to_ne_bytes());
        blob.extend_from_slice(&0i32.to_ne_bytes());
        blob.extend_from_slice(&0i32.to_ne_bytes());
        blob.extend_from_slice(&1i32.to_ne_bytes());
        for unit in units {
            blob.extend_from_slice(&unit.to_ne_bytes());
        }
        blob
    }

    /// The three vtable slots every COM interface starts with.
    #[repr(C)]
    struct UnknownVtable {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    /// `IDataObject`'s slots, in COM declaration order.
    #[repr(C)]
    struct DataObjectVtable {
        unknown: UnknownVtable,
        get_data:
            unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StorageMedium) -> i32,
        query_get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc) -> i32,
        get_data_here:
            unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StorageMedium) -> i32,
        get_canonical_format_etc:
            unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut FormatEtc) -> i32,
        set_data: unsafe extern "system" fn(
            *mut c_void,
            *const FormatEtc,
            *const StorageMedium,
            i32,
        ) -> i32,
        enum_format_etc: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
        d_advise: unsafe extern "system" fn(
            *mut c_void,
            *const FormatEtc,
            u32,
            *mut c_void,
            *mut u32,
        ) -> i32,
        d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> i32,
        enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    }

    /// `IDropSource`'s slots, in COM declaration order.
    #[repr(C)]
    struct DropSourceVtable {
        unknown: UnknownVtable,
        query_continue_drag: unsafe extern "system" fn(*mut c_void, i32, u32) -> i32,
        give_feedback: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    }

    /// `IEnumFORMATETC`'s slots, in COM declaration order.
    #[repr(C)]
    struct EnumFormatVtable {
        unknown: UnknownVtable,
        next: unsafe extern "system" fn(*mut c_void, u32, *mut FormatEtc, *mut u32) -> i32,
        skip: unsafe extern "system" fn(*mut c_void, u32) -> i32,
        reset: unsafe extern "system" fn(*mut c_void) -> i32,
        clone: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    }

    /// One interface a [`FileDrop`] presents: its vtable and the object it
    /// forwards to. Its address is the interface pointer.
    #[repr(C)]
    struct SubInterface<V> {
        vtable: *const V,
        owner: *mut FileDrop,
    }

    /// The `IEnumFORMATETC` state: the one format, offered once per reset.
    #[repr(C)]
    struct FormatEnumerator {
        vtable: *const EnumFormatVtable,
        owner: *mut FileDrop,
        delivered: std::cell::Cell<bool>,
    }

    /// The single COM object behind one drag, carrying all three interfaces
    /// the drag machinery asks for.
    ///
    /// The first field is the `IDataObject` vtable pointer, so the object's
    /// own address is the `IDataObject*`; the two sub-interfaces are separate
    /// vtable-and-owner pairs whose addresses are the `IDropSource*` and
    /// `IEnumFORMATETC*`. Every `AddRef`/`Release` on any of them lands on
    /// this one count, so no interface can outlive the object.
    #[repr(C)]
    struct FileDrop {
        data_vtable: *const DataObjectVtable,
        drop_source: SubInterface<DropSourceVtable>,
        formats: FormatEnumerator,
        references: std::cell::Cell<u32>,
        paths: Vec<std::path::PathBuf>,
    }

    impl FileDrop {
        /// A source object with one reference, held by the caller; it frees
        /// itself when that reference is the last released.
        fn new(paths: Vec<std::path::PathBuf>) -> *mut FileDrop {
            let object = Box::into_raw(Box::new(FileDrop {
                data_vtable: &DATA_OBJECT_VTABLE,
                drop_source: SubInterface {
                    vtable: &DROP_SOURCE_VTABLE,
                    owner: std::ptr::null_mut(),
                },
                formats: FormatEnumerator {
                    vtable: &ENUM_FORMAT_VTABLE,
                    owner: std::ptr::null_mut(),
                    delivered: std::cell::Cell::new(false),
                },
                references: std::cell::Cell::new(1),
                paths,
            }));
            // The owner back-pointers need the box's final address.
            // SAFETY: `object` came out of `Box::into_raw` this instant and
            // is not aliased; the writes just fill the fields in.
            unsafe {
                (*object).drop_source.owner = object;
                (*object).formats.owner = object;
            }
            object
        }
    }

    /// One more reference on `object`.
    ///
    /// # Safety
    ///
    /// `object` must be a live [`FileDrop`] from [`FileDrop::new`].
    unsafe fn add_ref_object(object: *mut FileDrop) -> u32 {
        // SAFETY: the caller's contract makes `object` a live [`FileDrop`].
        unsafe {
            let count = (*object).references.get() + 1;
            (*object).references.set(count);
            count
        }
    }

    /// One reference fewer on `object`; at zero the object deallocates.
    ///
    /// # Safety
    ///
    /// `object` must be a live [`FileDrop`] with a reference to release, and
    /// the caller must not touch it afterwards when this returns zero.
    unsafe fn release_object(object: *mut FileDrop) -> u32 {
        // SAFETY: the caller's contract makes `object` a live [`FileDrop`]
        // with a reference to release; nothing below touches it after the
        // deallocation.
        unsafe {
            let count = (*object).references.get().saturating_sub(1);
            (*object).references.set(count);
            if count == 0 {
                drop(Box::from_raw(object));
            }
            count
        }
    }

    /// The shared `QueryInterface` body: the wanted interface comes off
    /// `object`'s own sub-interface addresses.
    ///
    /// # Safety
    ///
    /// `object` must be a live [`FileDrop`], and `out` must be writable when
    /// `iid` is non-null.
    unsafe fn query_interface_for(
        object: *mut FileDrop,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if iid.is_null() || out.is_null() {
            return E_NOINTERFACE;
        }
        // SAFETY: the caller's contract makes `object` live and `out`
        // writable; `iid` was checked non-null above.
        unsafe {
            let wanted = *iid;
            let answer = if wanted == IID_IDATA_OBJECT || wanted == IID_IUNKNOWN {
                object as *mut c_void
            } else if wanted == IID_IDROP_SOURCE {
                std::ptr::addr_of_mut!((*object).drop_source) as *mut c_void
            } else if wanted == IID_IENUM_FORMATETC {
                std::ptr::addr_of_mut!((*object).formats) as *mut c_void
            } else {
                *out = std::ptr::null_mut();
                return E_NOINTERFACE;
            };
            *out = answer;
            add_ref_object(object);
            S_OK
        }
    }

    // --- IDataObject -------------------------------------------------------

    unsafe extern "system" fn data_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        // SAFETY: OLE calls this through the vtable it holds a reference to,
        // so `this` is the live data object.
        unsafe { query_interface_for(this as *mut FileDrop, iid, out) }
    }

    unsafe extern "system" fn data_add_ref(this: *mut c_void) -> u32 {
        // SAFETY: as above; `this` is the live data object.
        unsafe { add_ref_object(this as *mut FileDrop) }
    }

    unsafe extern "system" fn data_release(this: *mut c_void) -> u32 {
        // SAFETY: as above; OLE releases the reference it took, exactly once.
        unsafe { release_object(this as *mut FileDrop) }
    }

    /// Whether `format` asks for exactly what the drag offers.
    ///
    /// # Safety
    ///
    /// `format` must be null or point at a live [`FormatEtc`].
    unsafe fn format_is_wanted(format: *const FormatEtc) -> bool {
        if format.is_null() {
            return false;
        }
        // SAFETY: the caller's contract (and the null check above) makes
        // `format` point at a live [`FormatEtc`].
        unsafe {
            let wanted = &*format;
            wanted.format == CF_HDROP && wanted.storage & TYMED_HGLOBAL != 0
        }
    }

    unsafe extern "system" fn data_get_data(
        this: *mut c_void,
        format: *const FormatEtc,
        medium: *mut StorageMedium,
    ) -> i32 {
        // SAFETY: `format` comes through the vtable; the helper checks null
        // and reads only the one struct.
        let wanted = unsafe { format_is_wanted(format) };
        if medium.is_null() || !wanted {
            return DV_E_FORMATETC;
        }
        // SAFETY: `this` is the live data object OLE holds a reference to,
        // and `medium` was checked non-null above.
        unsafe {
            let object = &*(this as *mut FileDrop);
            // Step 1: a fresh global per call; the target owns it from here
            // (the drag loop may take the data more than once).
            let handle = global_from(&drop_files_blob(&object.paths));
            if handle == 0 {
                return E_OUTOFMEMORY;
            }
            (*medium).storage = TYMED_HGLOBAL;
            (*medium).global = handle;
            (*medium).for_release = std::ptr::null_mut();
            S_OK
        }
    }

    unsafe extern "system" fn data_query_get_data(
        this: *mut c_void,
        format: *const FormatEtc,
    ) -> i32 {
        let _ = this;
        // SAFETY: `format` is whatever OLE passed through the vtable; the
        // helper handles null and reads only the one struct.
        if unsafe { format_is_wanted(format) } {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    unsafe extern "system" fn data_get_data_here(
        this: *mut c_void,
        format: *const FormatEtc,
        medium: *mut StorageMedium,
    ) -> i32 {
        let (_, _, _) = (this, format, medium);
        // The drag offers its data through `GetData`; a caller-provided
        // medium is never filled (E_NOTIMPL is the documented answer for a
        // format served that way only).
        E_NOTIMPL
    }

    unsafe extern "system" fn data_get_canonical_format_etc(
        this: *mut c_void,
        format: *const FormatEtc,
        out: *mut FormatEtc,
    ) -> i32 {
        let (_, _) = (this, format);
        if !out.is_null() {
            // SAFETY: `out` was checked non-null above and OLE supplies a
            // writable `FORMATETC` there.
            unsafe { (*out).device = std::ptr::null_mut() };
        }
        // No second format describes the first one better.
        E_NOTIMPL
    }

    unsafe extern "system" fn data_set_data(
        this: *mut c_void,
        format: *const FormatEtc,
        medium: *const StorageMedium,
        release: i32,
    ) -> i32 {
        let (_, _, _, _) = (this, format, medium, release);
        // A drag's data object is read-only.
        E_NOTIMPL
    }

    unsafe extern "system" fn data_enum_format_etc(
        this: *mut c_void,
        direction: u32,
        out: *mut *mut c_void,
    ) -> i32 {
        if direction != DATADIR_GET || out.is_null() {
            return E_NOTIMPL;
        }
        // SAFETY: `this` is the live data object; `out` was checked non-null
        // above, and the enumerator lives inside that same object.
        unsafe {
            let object = this as *mut FileDrop;
            let enumerator = std::ptr::addr_of_mut!((*object).formats);
            (*enumerator).delivered.set(false);
            add_ref_object(object);
            *out = enumerator as *mut c_void;
            S_OK
        }
    }

    unsafe extern "system" fn data_d_advise(
        this: *mut c_void,
        format: *const FormatEtc,
        aspect: u32,
        sink: *mut c_void,
        connection: *mut u32,
    ) -> i32 {
        let (_, _, _, _, _) = (this, format, aspect, sink, connection);
        OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn data_d_unadvise(this: *mut c_void, connection: u32) -> i32 {
        let _ = (this, connection);
        OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn data_enum_d_advise(this: *mut c_void, out: *mut *mut c_void) -> i32 {
        let _ = (this, out);
        OLE_E_ADVISENOTSUPPORTED
    }

    // --- IDropSource -------------------------------------------------------

    unsafe extern "system" fn source_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        // SAFETY: OLE calls this through the drop source's vtable, so `this`
        // is the live sub-interface and its owner pointer is the object.
        unsafe {
            let owner = (*(this as *mut SubInterface<DropSourceVtable>)).owner;
            query_interface_for(owner, iid, out)
        }
    }

    unsafe extern "system" fn source_add_ref(this: *mut c_void) -> u32 {
        // SAFETY: as above; OLE holds the reference it takes here.
        unsafe { add_ref_object((*(this as *mut SubInterface<DropSourceVtable>)).owner) }
    }

    unsafe extern "system" fn source_release(this: *mut c_void) -> u32 {
        // SAFETY: as above; OLE releases the reference it took, exactly once.
        unsafe { release_object((*(this as *mut SubInterface<DropSourceVtable>)).owner) }
    }

    unsafe extern "system" fn source_query_continue_drag(
        this: *mut c_void,
        escape_pressed: i32,
        key_state: u32,
    ) -> i32 {
        let _ = this;
        if escape_pressed != 0 {
            return DRAGDROP_S_CANCEL;
        }
        if key_state & MK_LBUTTON == 0 {
            // The button came up: the window under the cursor takes the
            // drop, or nothing does.
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    unsafe extern "system" fn source_give_feedback(this: *mut c_void, effect: u32) -> i32 {
        let _ = (this, effect);
        // OLE's own cursors: copy, move, link and the "no" cursor.
        DRAGDROP_S_USEDEFAULTCURSORS
    }

    // --- IEnumFORMATETC ----------------------------------------------------

    unsafe extern "system" fn formats_query_interface(
        this: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        // SAFETY: OLE calls this through the enumerator's vtable, so `this`
        // is the live enumerator and its owner pointer is the object.
        unsafe {
            let owner = (*(this as *mut FormatEnumerator)).owner;
            query_interface_for(owner, iid, out)
        }
    }

    unsafe extern "system" fn formats_add_ref(this: *mut c_void) -> u32 {
        // SAFETY: as above; OLE holds the reference it takes here.
        unsafe { add_ref_object((*(this as *mut FormatEnumerator)).owner) }
    }

    unsafe extern "system" fn formats_release(this: *mut c_void) -> u32 {
        // SAFETY: as above; OLE releases the reference it took, exactly once.
        unsafe { release_object((*(this as *mut FormatEnumerator)).owner) }
    }

    unsafe extern "system" fn formats_next(
        this: *mut c_void,
        count: u32,
        items: *mut FormatEtc,
        fetched: *mut u32,
    ) -> i32 {
        if count == 0 {
            if !fetched.is_null() {
                // SAFETY: `fetched` was checked non-null above.
                unsafe { *fetched = 0 };
            }
            return S_OK;
        }
        // SAFETY: `this` is the live enumerator; the caller (OLE) supplies
        // `items`/`fetched` per the `Next` contract, checked before writes.
        unsafe {
            let enumerator = &*(this as *mut FormatEnumerator);
            if enumerator.delivered.get() || items.is_null() || fetched.is_null() {
                if !fetched.is_null() {
                    *fetched = 0;
                }
                return S_FALSE;
            }
            // Step 1: the one format: a file list in an HGLOBAL.
            *items = FormatEtc {
                format: CF_HDROP,
                device: std::ptr::null_mut(),
                aspect: DVASPECT_CONTENT,
                index: -1,
                storage: TYMED_HGLOBAL,
            };
            *fetched = 1;
            enumerator.delivered.set(true);
            if count > 1 {
                // Fewer than asked for: `S_FALSE` is the enumeration's
                // answer.
                return S_FALSE;
            }
            S_OK
        }
    }

    unsafe extern "system" fn formats_skip(this: *mut c_void, count: u32) -> i32 {
        if count == 0 {
            return S_OK;
        }
        // SAFETY: `this` is the live enumerator OLE holds a reference to.
        unsafe {
            let enumerator = &*(this as *mut FormatEnumerator);
            if enumerator.delivered.get() {
                return S_FALSE;
            }
            enumerator.delivered.set(true);
            if count > 1 {
                return S_FALSE;
            }
            S_OK
        }
    }

    unsafe extern "system" fn formats_reset(this: *mut c_void) -> i32 {
        // SAFETY: `this` is the live enumerator OLE holds a reference to.
        unsafe { (*(this as *mut FormatEnumerator)).delivered.set(false) };
        S_OK
    }

    unsafe extern "system" fn formats_clone(this: *mut c_void, out: *mut *mut c_void) -> i32 {
        let _ = (this, out);
        // The shell never clones a one-item enumerator; E_NOTIMPL is the
        // documented answer when it cannot be copied.
        E_NOTIMPL
    }

    /// The three vtables, built once. Slot order is COM declaration order;
    /// nothing may be inserted or reordered.
    static DATA_OBJECT_VTABLE: DataObjectVtable = DataObjectVtable {
        unknown: UnknownVtable {
            query_interface: data_query_interface,
            add_ref: data_add_ref,
            release: data_release,
        },
        get_data: data_get_data,
        query_get_data: data_query_get_data,
        get_data_here: data_get_data_here,
        get_canonical_format_etc: data_get_canonical_format_etc,
        set_data: data_set_data,
        enum_format_etc: data_enum_format_etc,
        d_advise: data_d_advise,
        d_unadvise: data_d_unadvise,
        enum_d_advise: data_enum_d_advise,
    };
    static DROP_SOURCE_VTABLE: DropSourceVtable = DropSourceVtable {
        unknown: UnknownVtable {
            query_interface: source_query_interface,
            add_ref: source_add_ref,
            release: source_release,
        },
        query_continue_drag: source_query_continue_drag,
        give_feedback: source_give_feedback,
    };
    static ENUM_FORMAT_VTABLE: EnumFormatVtable = EnumFormatVtable {
        unknown: UnknownVtable {
            query_interface: formats_query_interface,
            add_ref: formats_add_ref,
            release: formats_release,
        },
        next: formats_next,
        skip: formats_skip,
        reset: formats_reset,
        clone: formats_clone,
    };

    /// Start an OS drag carrying `path` as the dragged file, and return how
    /// it ended.
    ///
    /// Runs OLE's modal drag loop: it blocks the calling thread until the
    /// user drops or cancels, and no frame runs while it does. Call it from
    /// the event loop thread only, which is also the thread OLE requires.
    ///
    /// # Errors
    ///
    /// A message when `DoDragDrop` itself failed; a drop or a cancellation
    /// is an `Ok` outcome.
    pub fn drag_out(path: &std::path::Path) -> Result<super::DragOutcome, String> {
        // Step 1: OLE on this thread. winit already initialised it for its
        // drop target, so this is normally a refcounted no-op. The matching
        // uninitialise is never called: the executable keeps OLE for its
        // lifetime, and winit owns its own count.
        // SAFETY: a null apartment pointer asks for the default; the call is
        // refcounted and safe to repeat on this thread.
        unsafe { OleInitialize(std::ptr::null_mut()) };
        // Step 2: the source object; this scope holds its one reference.
        let object = FileDrop::new(vec![path.to_path_buf()]);
        let data_object = object as *mut c_void;
        // SAFETY: `object` is live and the sub-interface's address is a valid
        // `IDropSource*` for the whole call.
        let drop_source = unsafe { std::ptr::addr_of_mut!((*object).drop_source) } as *mut c_void;
        // Step 3: the drag; OLE takes its own references while it runs.
        let mut effect: u32 = 0;
        // SAFETY: both interface pointers belong to `object`, and `effect` is
        // writable for the call.
        let result = unsafe { DoDragDrop(data_object, drop_source, DROPEFFECTS, &mut effect) };
        // Step 4: this scope's reference goes last; the object frees at zero.
        // SAFETY: the reference was taken by construction and is released
        // exactly once; nothing below touches `object`.
        unsafe { release_object(object) };
        match result {
            DRAGDROP_S_DROP => Ok(super::DragOutcome::Dropped),
            DRAGDROP_S_CANCEL => Ok(super::DragOutcome::Cancelled),
            code if code >= 0 => Ok(super::DragOutcome::Other(code)),
            code => Err(format!("DoDragDrop failed: {:#010x}", code as u32)),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_drop_files_blob_has_the_header_and_a_double_nul_list() {
            let blob = drop_files_blob(&[std::path::PathBuf::from("C:\\a.png")]);
            let header = std::mem::size_of::<DropFilesHeader>();
            assert_eq!(
                u32::from_ne_bytes(blob[0..4].try_into().unwrap()),
                header as u32,
                "pFiles points just past the header"
            );
            assert_eq!(
                i32::from_ne_bytes(blob[12..16].try_into().unwrap()),
                0,
                "fNC"
            );
            assert_eq!(
                i32::from_ne_bytes(blob[16..20].try_into().unwrap()),
                1,
                "fWide"
            );
            let units: Vec<u16> = blob[header..]
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes(pair.try_into().unwrap()))
                .collect();
            let expected: Vec<u16> = "C:\\a.png\0\0".encode_utf16().collect();
            assert_eq!(units, expected);
        }

        #[test]
        fn the_drop_files_blob_joins_several_paths_in_one_list() {
            let blob =
                drop_files_blob(&[std::path::PathBuf::from("a"), std::path::PathBuf::from("b")]);
            let header = std::mem::size_of::<DropFilesHeader>();
            let units: Vec<u16> = blob[header..]
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes(pair.try_into().unwrap()))
                .collect();
            let expected: Vec<u16> = "a\0b\0\0".encode_utf16().collect();
            assert_eq!(units, expected);
        }
    }
}

#[cfg(windows)]
pub use win32::{drag_out, object, set_object, set_text, text};

/// No platform clipboard yet on this target (Linux and macOS land in
/// Stage 24); callers report the `None`/`Err` rather than failing.
#[cfg(not(windows))]
pub fn set_text(_text: &str) -> Result<(), String> {
    Err("the platform clipboard is not implemented on this target yet".to_string())
}

/// No platform clipboard yet on this target.
#[cfg(not(windows))]
pub fn text() -> Option<String> {
    None
}

/// No platform clipboard yet on this target.
#[cfg(not(windows))]
pub fn set_object(_format: &str, _bytes: &[u8], _text: &str) -> Result<(), String> {
    Err("the platform clipboard is not implemented on this target yet".to_string())
}

/// No platform clipboard yet on this target.
#[cfg(not(windows))]
pub fn object(_format: &str) -> Option<Vec<u8>> {
    None
}

/// No platform drag-and-drop yet on this target (Stage 24); the caller
/// reports the `Err` rather than failing.
#[cfg(not(windows))]
pub fn drag_out(_path: &std::path::Path) -> Result<DragOutcome, String> {
    Err("the platform drag-and-drop is not implemented on this target yet".to_string())
}
