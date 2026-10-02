//! Where an asset path's bytes come from: mounted packs and the filesystem.
//!
//! # Responsibilities
//!
//! - Parse an asset pack ([`AssetPack`]): the files of a project's `res`
//!   directory in one blob, written at build time by `pill_assets`.
//! - Keep the mount points an [`AssetLoader::Path`](crate::AssetLoader::Path)
//!   resolves through: packs ([`mount_pack`]) and the project's asset
//!   directory ([`mount_directory`]).
//! - Read a path's bytes from the first mount that has it ([`read`]).
//! - Write a file beside an asset that lives on the filesystem
//!   ([`write_beside`]), which is how a development build records an asset's
//!   metadata, and write a new file that must not exist yet ([`write_new`]),
//!   which is how a tool creates a standalone asset. Packs are never written.
//!
//! # Design
//!
//! Loading code names a path relative to the project's `res` directory and
//! never the target it runs on; what differs per target is only what is
//! mounted:
//!
//! - **development:** nothing but the filesystem - the project's asset
//!   directory, then the working directory's `res`, `$PROJECT_PATH/res` and
//!   the executable's `res`;
//! - **native shipping:** the pack the shipping bundle embeds, mounted by the
//!   runtime before any module initializes;
//! - **web:** a pack the frontend fetched, mounted the same way.
//!
//! Packs are searched before the filesystem, newest mount first: a shipped
//! game reads the assets it was built with, not whatever happens to sit at a
//! development path on the machine it runs on.
//!
//! The store is a process-wide static of this crate. In a development host
//! each loaded image has its own copy, which is harmless there: only the
//! filesystem is mounted, and every copy reads the same files.
//!
//! # Pack format (version 1)
//!
//! Little-endian throughout. `PILLPACK`, a `u32` version, a `u32` entry count,
//! then per entry a `u32` path length, the path (UTF-8, `/`-separated,
//! relative), and a `u64` offset and `u64` length of its bytes counted from the
//! start of the pack. The bytes follow the index. `pill_assets::write_asset_pack`
//! is the writer; the two change together.

// Standard library
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

// Current crate
use crate::asset::{AssetLoadError, AssetLoadResult};

/// The first bytes of every asset pack.
pub const ASSET_PACK_MAGIC: &[u8; 8] = b"PILLPACK";

/// The pack format version this engine reads.
pub const ASSET_PACK_VERSION: u32 = 1;

// =============================================================================
// AssetPack
// =============================================================================

/// A parsed asset pack: the files of a `res` directory, by relative path.
#[derive(Debug)]
pub struct AssetPack {
    /// The whole pack, borrowed when embedded in the binary, owned when fetched.
    bytes: Cow<'static, [u8]>,
    /// Each file's byte range within `bytes`, by `/`-separated relative path.
    entries: HashMap<String, (usize, usize)>,
}

/// Why bytes could not be read as an asset pack.
#[derive(Debug, thiserror::Error)]
pub enum AssetPackError {
    /// The bytes do not start with [`ASSET_PACK_MAGIC`].
    #[error("not an asset pack: the header is missing")]
    NotAPack,
    /// The pack was written in a format version this engine does not read.
    #[error("asset pack version {found} is not supported (expected {ASSET_PACK_VERSION})")]
    UnsupportedVersion {
        /// The version the pack declares.
        found: u32,
    },
    /// The index or a file's bytes run past the end of the pack.
    #[error("asset pack is truncated")]
    Truncated,
    /// An entry's path is not UTF-8.
    #[error("asset pack entry {index} has a path that is not UTF-8")]
    InvalidPath {
        /// Position of the entry in the index.
        index: u32,
    },
}

impl AssetPack {
    /// Parse a pack, validating its index against its length.
    ///
    /// Takes `&'static [u8]` for a pack embedded with `include_bytes!` without
    /// copying it, or `Vec<u8>` for one fetched at runtime.
    ///
    /// # Errors
    ///
    /// Returns an [`AssetPackError`] when the bytes are not a pack of this
    /// version, or its index does not fit in it.
    pub fn parse(bytes: impl Into<Cow<'static, [u8]>>) -> Result<Self, AssetPackError> {
        let bytes = bytes.into();
        let mut reader = PackReader {
            bytes: &bytes,
            position: 0,
        };
        if reader.take(ASSET_PACK_MAGIC.len())? != ASSET_PACK_MAGIC {
            return Err(AssetPackError::NotAPack);
        }
        let version = reader.u32()?;
        if version != ASSET_PACK_VERSION {
            return Err(AssetPackError::UnsupportedVersion { found: version });
        }
        let count = reader.u32()?;

        // Each entry's range is checked here, once, so a lookup can slice
        // without checking again.
        let mut entries = HashMap::with_capacity(count as usize);
        for index in 0..count {
            let path_length = reader.u32()? as usize;
            let path = std::str::from_utf8(reader.take(path_length)?)
                .map_err(|_| AssetPackError::InvalidPath { index })?
                .to_owned();
            let offset = usize::try_from(reader.u64()?).map_err(|_| AssetPackError::Truncated)?;
            let length = usize::try_from(reader.u64()?).map_err(|_| AssetPackError::Truncated)?;
            let end = offset
                .checked_add(length)
                .ok_or(AssetPackError::Truncated)?;
            if end > bytes.len() {
                return Err(AssetPackError::Truncated);
            }
            entries.insert(path, (offset, length));
        }
        Ok(Self { bytes, entries })
    }

    /// The bytes of the file at `path` (`/`-separated, relative), if packed.
    pub fn get(&self, path: &str) -> Option<&[u8]> {
        let &(offset, length) = self.entries.get(path)?;
        Some(&self.bytes[offset..offset + length])
    }

    /// Number of files in the pack.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pack holds no files.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A cursor over a pack's index.
struct PackReader<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
}

impl<'bytes> PackReader<'bytes> {
    /// The next `length` bytes.
    fn take(&mut self, length: usize) -> Result<&'bytes [u8], AssetPackError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(AssetPackError::Truncated)?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    /// The next little-endian `u32`.
    fn u32(&mut self) -> Result<u32, AssetPackError> {
        let mut value = [0; 4];
        value.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(value))
    }

    /// The next little-endian `u64`.
    fn u64(&mut self) -> Result<u64, AssetPackError> {
        let mut value = [0; 8];
        value.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(value))
    }
}

// =============================================================================
// Mount points
// =============================================================================

/// What is mounted: packs, newest first, and the project's asset directory.
#[derive(Default)]
struct Mounts {
    packs: Vec<Arc<AssetPack>>,
    directory: Option<PathBuf>,
}

/// The process-wide mounts of this image.
fn mounts() -> &'static RwLock<Mounts> {
    static MOUNTS: OnceLock<RwLock<Mounts>> = OnceLock::new();
    MOUNTS.get_or_init(Default::default)
}

/// Mount `pack`: its files are found before every earlier mount's.
pub fn mount_pack(pack: AssetPack) {
    let mut mounts = mounts()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mounts.packs.insert(0, Arc::new(pack));
}

/// Mount `directory` as the project's asset directory, replacing the previous
/// one. Searched after every pack.
pub fn mount_directory(directory: impl Into<PathBuf>) {
    let mut mounts = mounts()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mounts.directory = Some(directory.into());
}

/// The project's asset directory, when one is mounted.
pub fn mounted_directory() -> Option<PathBuf> {
    let mounts = mounts()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    mounts
        .directory
        .clone()
        .filter(|directory| !directory.as_os_str().is_empty())
}

/// Read the bytes of the asset at `path` from the first mount that has it.
///
/// An absolute path is read from the filesystem as is. A relative one is
/// looked up in every pack, then below the project's asset directory, then
/// the development fallbacks: `res` in the working directory, in
/// `$PROJECT_PATH`, and beside the executable.
///
/// # Errors
///
/// Returns [`AssetLoadError::PathNotFound`] when no mount has the path, or
/// [`AssetLoadError::Read`] when the file was found but could not be read.
pub fn read(path: &Path) -> AssetLoadResult<Vec<u8>> {
    // Packs first: a shipped game reads what it was built with.
    if let Some(key) = pack_key(path) {
        let packs = mounts()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .packs
            .clone();
        if let Some(bytes) = packs.iter().find_map(|pack| pack.get(&key)) {
            return Ok(bytes.to_vec());
        }
    }
    let file = resolve_file(path).ok_or_else(|| AssetLoadError::PathNotFound {
        path: path.to_owned(),
    })?;
    std::fs::read(&file).map_err(|source| AssetLoadError::Read { path: file, source })
}

/// The key `path` is packed under: its components joined with `/`. `None` for
/// a path no pack can hold - absolute, climbing out with `..`, or not UTF-8.
///
/// Also the canonical name of an asset loaded from that path, so the name and
/// the pack key can never disagree. `\` separates components on every
/// target, not only on Windows: a path written on one machine must name the
/// same asset on another.
pub(crate) fn pack_key(path: &Path) -> Option<String> {
    let unified = path.to_str()?.replace('\\', "/");
    let mut parts = Vec::new();
    for component in Path::new(&unified).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Find `path` on the filesystem: as is when absolute, else below the
/// mounted directory and the development fallbacks.
fn resolve_file(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return path.is_file().then(|| path.to_owned());
    }
    let mut candidates = Vec::new();
    if let Some(directory) = mounted_directory() {
        candidates.push(directory.join(path));
    }
    candidates.push(path.to_owned());
    candidates.push(Path::new("res").join(path));
    if let Some(project) = std::env::var_os("PROJECT_PATH") {
        candidates.push(PathBuf::from(project).join("res").join(path));
    }
    if let Some(parent) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_owned))
    {
        candidates.push(parent.join("res").join(path));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// The filesystem path the asset at `path` resolves to, or `None` when it is
/// not on the filesystem - packed only, or missing.
///
/// Resolves exactly as [`read`] does after the packs: as is when absolute,
/// else below the mounted directory and the development fallbacks.
pub fn locate_file(path: &Path) -> Option<PathBuf> {
    resolve_file(path)
}

/// Write `bytes` to a file named like `target` in the directory of the asset
/// `source`, unless that file already exists.
///
/// Returns the written (or already present) file, or `None` when `source` is
/// not on the filesystem: a packed asset - every asset of a shipping or web
/// build - has no directory to write into, and that is not an error.
///
/// The bytes go to a temporary sibling first and are linked into place only
/// when complete, so a crash or a second writer never leaves a half-written
/// file. An existing file is never replaced, even one that appears while this
/// runs: it may hold a person's edits.
///
/// # Errors
///
/// Returns [`AssetLoadError::Write`] when the file cannot be written.
pub fn write_beside(
    source: &Path,
    target: &Path,
    bytes: &[u8],
) -> AssetLoadResult<Option<PathBuf>> {
    let Some(source_file) = resolve_file(source) else {
        return Ok(None);
    };
    let directory = source_file.parent().unwrap_or(Path::new("."));
    let file_name = target.file_name().ok_or_else(|| AssetLoadError::Write {
        path: target.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"),
    })?;
    let destination = directory.join(file_name);
    if destination.exists() {
        return Ok(Some(destination));
    }
    let write_error = |source| AssetLoadError::Write {
        path: destination.clone(),
        source,
    };

    let temporary = directory.join(temporary_name(file_name));
    std::fs::write(&temporary, bytes).map_err(write_error)?;
    let placed = place_without_replacing(&temporary, &destination);
    // The temporary is gone after a successful fallback rename; ignore that.
    let _ = std::fs::remove_file(&temporary);
    placed.map_err(write_error)?;
    Ok(Some(destination))
}

/// Write `bytes` as the new file `destination`, refusing when it exists.
///
/// The bytes go to a temporary sibling first and are linked into place only
/// when complete, so a crash never leaves a half-written file, and an existing
/// file - even one that appears while this runs - is never replaced. Missing
/// parent directories are not created: the caller names a folder that exists.
///
/// # Errors
///
/// [`AssetLoadError::Write`] with [`std::io::ErrorKind::AlreadyExists`] when
/// the file exists, or with the underlying error when it cannot be written.
pub fn write_new(destination: &Path, bytes: &[u8]) -> AssetLoadResult<()> {
    let write_error = |source| AssetLoadError::Write {
        path: destination.to_owned(),
        source,
    };
    let file_name = destination.file_name().ok_or_else(|| {
        write_error(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no file name",
        ))
    })?;
    let directory = destination.parent().unwrap_or(Path::new("."));
    if destination.exists() {
        return Err(write_error(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the file already exists",
        )));
    }
    let temporary = directory.join(temporary_name(file_name));
    std::fs::write(&temporary, bytes).map_err(write_error)?;
    // A hard link fails when the destination exists, which is the guarantee
    // wanted here; unlike `place_without_replacing`, an existing file is an
    // error rather than someone else's equally good copy.
    let linked = std::fs::hard_link(&temporary, destination);
    let _ = std::fs::remove_file(&temporary);
    linked.map_err(write_error)
}

/// A sibling name for `file_name` that no other writer in this or another
/// process picks at the same time.
fn temporary_name(file_name: &std::ffi::OsStr) -> std::ffi::OsString {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(format!(".{}-{sequence}.tmp", std::process::id()));
    name
}

/// Move the complete file `temporary` to `destination`, leaving an existing
/// `destination` untouched.
///
/// A hard link fails atomically when the destination exists, which no
/// rename on Windows does. A filesystem without hard links falls back to a
/// rename guarded by an existence check: the check and the rename are not one
/// operation, but only two writers racing on one metadata file could slip
/// between them.
fn place_without_replacing(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) if destination.exists() => Ok(()),
        Err(_) => std::fs::rename(temporary, destination),
    }
}

/// Serializes tests that change the process-wide mounted directory.
///
/// `mount_directory` replaces one global, and tests run in parallel threads,
/// so two tests mounting their own scratch directory would read each other's
/// files. Every test that mounts a directory holds this for its whole run.
#[cfg(test)]
pub(crate) fn mounted_directory_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that failed while holding the lock poisons it; the directory is
    // re-mounted by the next holder anyway.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_new_writes_once_and_never_replaces() {
        let directory = std::env::temp_dir().join(format!("pill-write-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("a.render_pass");

        write_new(&file, b"first").unwrap();
        let error = write_new(&file, b"second").unwrap_err();

        assert!(matches!(
            error,
            AssetLoadError::Write { ref source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read(&file).unwrap(), b"first");
        let leftovers = std::fs::read_dir(&directory).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file is left behind");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A version 1 pack of `files`, written the way `pill_assets` writes one.
    fn pack_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let index_length: usize = files.iter().map(|(path, _)| 4 + path.len() + 16).sum();
        let mut offset = (8 + 4 + 4 + index_length) as u64;
        let mut pack = Vec::new();
        pack.extend_from_slice(ASSET_PACK_MAGIC);
        pack.extend_from_slice(&ASSET_PACK_VERSION.to_le_bytes());
        pack.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, bytes) in files {
            pack.extend_from_slice(&(path.len() as u32).to_le_bytes());
            pack.extend_from_slice(path.as_bytes());
            pack.extend_from_slice(&offset.to_le_bytes());
            pack.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            offset += bytes.len() as u64;
        }
        for (_, bytes) in files {
            pack.extend_from_slice(bytes);
        }
        pack
    }

    #[test]
    fn a_pack_returns_each_file_by_its_relative_path() {
        let pack =
            AssetPack::parse(pack_of(&[("models/a.obj", b"mesh"), ("b.png", b"image")])).unwrap();
        assert_eq!(pack.len(), 2);
        assert_eq!(pack.get("models/a.obj"), Some(&b"mesh"[..]));
        assert_eq!(pack.get("b.png"), Some(&b"image"[..]));
        assert_eq!(pack.get("missing"), None);
    }

    #[test]
    fn a_damaged_pack_is_refused() {
        assert!(matches!(
            AssetPack::parse(b"NOTAPACK".to_vec()),
            Err(AssetPackError::NotAPack)
        ));
        let mut future = pack_of(&[]);
        future[8] = 9;
        assert!(matches!(
            AssetPack::parse(future),
            Err(AssetPackError::UnsupportedVersion { found: 9 })
        ));
        let mut cut = pack_of(&[("a", b"0123456789")]);
        cut.truncate(cut.len() - 1);
        assert!(matches!(
            AssetPack::parse(cut),
            Err(AssetPackError::Truncated)
        ));
    }

    #[test]
    fn pack_keys_are_slash_separated_and_stay_inside_the_pack() {
        assert_eq!(
            pack_key(Path::new("models/./a.obj")).as_deref(),
            Some("models/a.obj")
        );
        assert_eq!(pack_key(Path::new("../outside.txt")), None);
        assert_eq!(pack_key(Path::new("")), None);
    }

    /// The pack key doubles as an asset's name, so every spelling of one path
    /// must give one key, on every target.
    #[test]
    fn backslashes_and_dots_normalize_to_one_key() {
        let expected = Some("textures/a.jpg".to_owned());
        assert_eq!(pack_key(Path::new("textures\\a.jpg")), expected);
        assert_eq!(pack_key(Path::new("textures/./a.jpg")), expected);
        assert_eq!(pack_key(Path::new(".\\textures\\.\\a.jpg")), expected);
        assert_eq!(pack_key(Path::new("textures\\..\\a.jpg")), None);
        assert_eq!(pack_key(&std::env::temp_dir().join("a.jpg")), None);
    }

    /// A fresh directory under the system temp directory, removed on drop.
    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pill-asset-store-{label}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn locate_file_finds_files_on_disk_only() {
        let scratch = ScratchDirectory::new("locate");
        let source = scratch.0.join("present.txt");
        std::fs::write(&source, b"x").unwrap();
        assert_eq!(locate_file(&source), Some(source.clone()));
        assert_eq!(locate_file(&scratch.0.join("absent.txt")), None);
    }

    /// The metadata path: written into the source's own directory, under the
    /// target's file name, with no temporary left behind.
    #[test]
    fn write_beside_writes_next_to_the_source() {
        let scratch = ScratchDirectory::new("write");
        let source = scratch.0.join("a.jpg");
        std::fs::write(&source, b"image").unwrap();

        let written = write_beside(&source, Path::new("textures/a.jpg.meta"), b"{}")
            .unwrap()
            .expect("the source is on disk");

        assert_eq!(written, scratch.0.join("a.jpg.meta"));
        assert_eq!(std::fs::read(&written).unwrap(), b"{}");
        let leftovers: Vec<_> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    /// An existing file may hold a person's edits: it is kept as it is.
    #[test]
    fn write_beside_never_replaces_an_existing_file() {
        let scratch = ScratchDirectory::new("keep");
        let source = scratch.0.join("a.jpg");
        std::fs::write(&source, b"image").unwrap();
        let existing = scratch.0.join("a.jpg.meta");
        std::fs::write(&existing, b"edited by hand").unwrap();

        let written = write_beside(&source, Path::new("a.jpg.meta"), b"generated").unwrap();

        assert_eq!(written, Some(existing.clone()));
        assert_eq!(std::fs::read(&existing).unwrap(), b"edited by hand");
    }

    /// A source no filesystem mount has - packed only, as in every shipping
    /// and web build - has nowhere to write beside, and that is not an error.
    #[test]
    fn write_beside_a_source_not_on_disk_writes_nothing() {
        let key = "pill-store-test/packed-only-source.jpg";
        mount_pack(AssetPack::parse(pack_of(&[(key, b"image")])).unwrap());
        assert_eq!(read(Path::new(key)).unwrap(), b"image");

        let written = write_beside(
            Path::new(key),
            Path::new("packed-only-source.jpg.meta"),
            b"{}",
        )
        .unwrap();

        assert_eq!(written, None);
    }

    #[test]
    fn a_mounted_pack_answers_before_the_filesystem() {
        mount_pack(
            AssetPack::parse(pack_of(&[("pill-store-test/only-packed.txt", b"packed")])).unwrap(),
        );
        assert_eq!(
            read(Path::new("pill-store-test/only-packed.txt")).unwrap(),
            b"packed"
        );
        assert!(matches!(
            read(Path::new("pill-store-test/nowhere.txt")),
            Err(AssetLoadError::PathNotFound { .. })
        ));
    }
}
