//! Many-per-type asset storage, addressed by generational handle.
//!
//! # Responsibilities
//!
//! - Defines the [`Asset`] marker trait for types stored many-per-type.
//! - Provides [`Handle<T>`], a typed generational reference to one asset.
//! - Implements [`AssetManager`], the store holding every asset column.
//!
//! # Design
//!
//! Resources are singletons: one `SimulationTime` per world, fetched by type
//! through [`Res`](crate::query::Res). Meshes and textures are the opposite
//! shape - a world holds hundreds, and the type alone does not name one. That
//! is a different storage problem, so it gets different storage rather than a
//! reinterpretation of [`Resource`](crate::resource::Resource).
//!
//! [`AssetManager`] is itself a plain resource, so the whole store arrives
//! through the existing `Res<AssetManager>` / `ResMut<AssetManager>` parameters
//! and the scheduler's existing resource-conflict analysis already covers it -
//! two systems writing assets serialise, a reader and a writer serialise, and
//! an asset writer still runs beside a system touching only components. No
//! scheduler change was needed to add this.
//!
//! Storage mirrors how components are held - a type-erased column per type in a
//! `TraitTypeMap` - minus archetypes, which exist to group *entities* by their
//! component set and have no meaning for an asset. The family is
//! [`VecOptionFamily`] rather than `VecFamily` because unloading must free one
//! slot without moving the others: every live handle keeps its index.
//!
//! # Handles are generational
//!
//! A [`Handle<T>`] carries an index and a generation. Freeing a slot bumps that
//! slot's generation, so a handle kept across an unload resolves to `None`
//! rather than silently addressing whatever was loaded into the slot next. That
//! is the difference between a missing model and a wrong one, and the second is
//! far harder to recognise.

// Standard library
use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

// External crates
use trait_type_map::{TraitAccessible, TraitTypeMap, VecOptionFamily};

// Current crate
use crate::resource::Resource;

// =============================================================================
// Asset
// =============================================================================

/// Marker for data stored many-per-type in the [`AssetManager`].
///
/// Implement this for meshes, textures, materials, shaders and the like - any
/// type a world holds several of, where the type alone does not identify one
/// value. Use [`Resource`](crate::resource::Resource) instead for genuine
/// singletons such as a clock or an input snapshot.
///
/// `Send + Sync` for the same reason resources are: systems are dispatched
/// across threads, and the store is reachable from any of them.
///
/// # Examples
///
/// ```
/// use pill_engine::asset::Asset;
/// use trait_type_map::impl_trait_accessible;
///
/// struct Mesh {
///     vertices: Vec<[f32; 3]>,
/// }
///
/// impl Asset for Mesh {}
/// impl_trait_accessible!(dyn Asset; Mesh);
/// ```
pub trait Asset: Send + Sync + 'static {}

// =============================================================================
// AssetLoader
// =============================================================================

/// Source used to initialize an asset from a project file or embedded bytes.
///
/// Relative paths resolve below the configured asset root. A project normally
/// sets that root to its `res` directory once during initialization.
#[derive(Debug, Clone)]
pub enum AssetLoader {
    Path(PathBuf),
    Bytes(Box<[u8]>),
}

/// Failure to resolve or read an [`AssetLoader`].
#[derive(Debug, thiserror::Error)]
pub enum AssetLoadError {
    #[error("asset path was not found: {path}")]
    PathNotFound { path: PathBuf },
    #[error("failed to read asset {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("asset {label} is not valid UTF-8: {source}")]
    Utf8 {
        label: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("failed to decode asset {label}: {detail}")]
    Decode { label: String, detail: String },
}

pub type AssetLoadResult<T> = Result<T, AssetLoadError>;

static ASSET_ROOT: OnceLock<RwLock<PathBuf>> = OnceLock::new();

impl AssetLoader {
    /// Set the directory relative [`Self::Path`] values resolve beneath.
    pub fn set_root(path: impl Into<PathBuf>) {
        let root = ASSET_ROOT.get_or_init(|| RwLock::new(PathBuf::new()));
        *root.write().expect("asset root lock poisoned") = path.into();
    }

    /// Return the currently configured asset root, if one was set.
    pub fn root() -> Option<PathBuf> {
        let root = ASSET_ROOT.get()?.read().ok()?.clone();
        (!root.as_os_str().is_empty()).then_some(root)
    }

    /// Read this source into owned bytes.
    pub fn load(&self) -> AssetLoadResult<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.to_vec()),
            Self::Path(path) => {
                let path = resolve_asset_path(path)
                    .ok_or_else(|| AssetLoadError::PathNotFound { path: path.clone() })?;
                std::fs::read(&path).map_err(|source| AssetLoadError::Read { path, source })
            }
        }
    }

    /// Read this source as UTF-8 text.
    pub fn load_string(&self) -> AssetLoadResult<String> {
        let label = match self {
            Self::Path(path) => path.display().to_string(),
            Self::Bytes(_) => "embedded bytes".to_owned(),
        };
        String::from_utf8(self.load()?).map_err(|source| AssetLoadError::Utf8 { label, source })
    }
}

fn resolve_asset_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() && path.is_file() {
        return Some(path.to_owned());
    }
    if let Some(root) = AssetLoader::root() {
        let candidate = root.join(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if path.is_file() {
        return Some(path.to_owned());
    }
    let candidate = Path::new("res").join(path);
    if candidate.is_file() {
        return Some(candidate);
    }
    if let Some(project) = std::env::var_os("PROJECT_PATH") {
        let candidate = PathBuf::from(project).join("res").join(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|executable| {
            executable
                .parent()
                .map(|parent| parent.join("res").join(path))
        })
        .filter(|candidate| candidate.is_file())
}

// =============================================================================
// Handle
// =============================================================================

/// A typed, generational reference to one asset in the [`AssetManager`].
///
/// Cheap to copy and free of any borrow, so handles live inside components,
/// inside other assets, and inside project state without tying anything to the
/// lifetime of the store.
///
/// A handle is only meaningful to the [`AssetManager`] that issued it. Handing
/// one to a different manager is a programming error; it resolves to `None`
/// or - if that manager happens to have a live slot at the same index and
/// generation - to an unrelated asset. Worlds hold one manager, so this does
/// not arise in ordinary use.
#[repr(C)]
pub struct Handle<T: Asset> {
    /// Slot index within this type's column.
    index: u32,
    /// Generation of the slot at the time this handle was issued.
    ///
    /// Compared on every lookup: a mismatch means the slot was freed and
    /// possibly refilled, so this handle no longer refers to a live asset.
    generation: u32,
    /// Marks `T` without storing one; the handle owns no asset data.
    _marker: PhantomData<fn() -> T>,
}

impl<T: Asset> Handle<T> {
    /// Sentinel for an optional asset reference that has not been assigned.
    pub const INVALID: Self = Self {
        index: u32::MAX,
        generation: u32::MAX,
        _marker: PhantomData,
    };

    /// Slot index this handle addresses.
    #[inline]
    pub fn index(self) -> u32 {
        self.index
    }

    /// Generation this handle was issued against.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

impl<T: Asset> Default for Handle<T> {
    fn default() -> Self {
        Self::INVALID
    }
}

impl<T: Asset> serde::Serialize for Handle<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.index, self.generation).serialize(serializer)
    }
}

impl<'de, T: Asset> serde::Deserialize<'de> for Handle<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (index, generation) = <(u32, u32)>::deserialize(deserializer)?;
        Ok(Self {
            index,
            generation,
            _marker: PhantomData,
        })
    }
}

// The derives are written out by hand because `#[derive]` would add a `T: Clone`
// style bound on the asset type, which a handle never needs - it stores no `T`.
impl<T: Asset> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Asset> Copy for Handle<T> {}

impl<T: Asset> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation
    }
}

impl<T: Asset> Eq for Handle<T> {}

impl<T: Asset> std::hash::Hash for Handle<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
        self.generation.hash(state);
    }
}

impl<T: Asset> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Handle<{}>({}, gen {})",
            std::any::type_name::<T>(),
            self.index,
            self.generation
        )
    }
}

// =============================================================================
// AssetColumn
// =============================================================================

/// Per-type slot bookkeeping that sits alongside one asset column.
///
/// The column in the [`TraitTypeMap`] holds the values; this holds everything
/// needed to hand out and validate handles for them. Kept separate because the
/// map stores type-erased columns and cannot carry per-type metadata of its own.
#[derive(Default)]
struct AssetColumn {
    /// Generation of each slot, parallel to the column's slots by index.
    ///
    /// Starts at 0 for a fresh slot and increments on every free, so a handle
    /// issued before the free never matches after it.
    generations: Vec<u32>,
    /// Slots freed by `remove`, refilled before the column grows.
    free_slots: Vec<u32>,
    /// Name lookup for assets added with one.
    ///
    /// Holds the index only: the generation is read from `generations` at
    /// lookup time, so a name rebound to a new asset resolves to the live one.
    by_name: HashMap<String, u32>,
    /// Reverse of `by_name`, so freeing a slot can drop its name without
    /// scanning the whole map.
    names: HashMap<u32, String>,
    /// Guid lookup, holding the index exactly as `by_name` does.
    ///
    /// A separate map rather than hashing the name at lookup time: a guid may
    /// be assigned without a name (an asset cooked to an id), and the two
    /// namespaces must not alias.
    by_guid: HashMap<AssetGuid, u32>,
    /// Reverse of `by_guid`, so freeing a slot can drop its guid without
    /// scanning the whole map.
    guids: HashMap<u32, AssetGuid>,
}

// =============================================================================
// AssetGuid
// =============================================================================

/// Stable 128-bit identity of one asset.
///
/// A name is what a human writes and a scene file carries; a guid is what
/// survives a rename. Both address the same slot - [`AssetManager`] keeps an
/// index for each - and an asset may carry either, both, or neither.
///
/// [`AssetGuid::from_name`] derives one from a string with the same FNV-1a
/// construction a shared component identity uses, so a cooking step and the
/// runtime compute the same value from the same canonical name without
/// exchanging anything. A guid assigned by a pipeline, from a file or a
/// database, is passed to [`AssetGuid::new`] instead and is not required to
/// relate to any name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetGuid(u128);

impl AssetGuid {
    /// Wrap a guid a cooking pipeline or content database already assigned.
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Derive a guid from a name, reusing the shared-identity hash.
    ///
    /// The same function components use for their stable names, so one
    /// canonical string yields one identity everywhere: in a build step, in
    /// the host, and in a module compiled separately from either.
    pub const fn from_name(name: &str) -> Self {
        Self(crate::component::shared_component_identity(name))
    }

    /// The raw 128-bit value.
    pub const fn value(self) -> u128 {
        self.0
    }
}

impl std::fmt::Display for AssetGuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

// =============================================================================
// AssetManager
// =============================================================================

/// Stores many assets per type, each addressed by a [`Handle`].
///
/// Inserted into a world like any other resource, and reached through
/// `Res<AssetManager>` / `ResMut<AssetManager>`:
///
/// ```
/// use pill_engine::asset::{Asset, AssetManager};
/// use trait_type_map::impl_trait_accessible;
///
/// #[derive(Debug, PartialEq)]
/// struct Mesh(&'static str);
/// impl Asset for Mesh {}
/// impl_trait_accessible!(dyn Asset; Mesh);
///
/// let mut assets = AssetManager::new();
/// let rock = assets.add(Mesh("rock"));
/// let tree = assets.add_named("tree", Mesh("tree"));
///
/// assert_eq!(assets.get(rock), Some(&Mesh("rock")));
/// assert_eq!(assets.handle_by_name::<Mesh>("tree"), Some(tree));
/// ```
#[derive(Default)]
pub struct AssetManager {
    /// One type-erased column per asset type, holding the values themselves.
    columns: TraitTypeMap<dyn Asset, VecOptionFamily>,
    /// Slot bookkeeping for each column, keyed by the same type.
    metadata: HashMap<TypeId, AssetColumn>,
    /// Changes whenever stored asset data may have changed.
    revision: u64,
}

// SAFETY: Every value the map holds is an `Asset`, and `Asset` requires
// `Send + Sync`, so each stored asset is genuinely safe to move and share
// across threads. What is not `Send + Sync` is the erased
// `Box<dyn TraitVecOptionStorage<dyn Asset>>` the map stores its columns in:
// that trait object carries no auto-trait bounds, so the compiler cannot see
// the guarantee the `Asset` bound already enforces at every insertion point.
// `add` is the only way a value enters a column and it requires `T: Asset`, so
// no non-`Send` or non-`Sync` value can be reached through this type.
//
// Aliasing is handled separately by the scheduler: `AssetManager` is a
// resource, so concurrent access is serialised by the existing
// `Res` / `ResMut` conflict analysis exactly as for any other resource.
unsafe impl Send for AssetManager {}
// SAFETY: As for `Send` directly above - shared access reaches only `Asset`
// values, which are `Sync` by the trait bound, and the scheduler serialises a
// writer against every other accessor.
unsafe impl Sync for AssetManager {}

impl Resource for AssetManager {}

impl AssetManager {
    /// Create an empty manager holding no asset types.
    ///
    /// Types register themselves on first use, so nothing needs declaring up
    /// front - a project that loads no meshes carries no mesh column.
    pub fn new() -> Self {
        Self::default()
    }

    /// Monotonic change counter used by renderer upload caches.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Store `asset` and return a handle to it.
    ///
    /// Reuses a slot freed by [`Self::remove`] when one is available, so a
    /// project that repeatedly loads and unloads does not grow the column
    /// without bound.
    pub fn add<T>(&mut self, asset: T) -> Handle<T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.ensure_column::<T>();

        let storage = self.columns.get_storage_mut::<T>();
        let metadata = self
            .metadata
            .get_mut(&TypeId::of::<T>())
            .expect("column metadata is created alongside the column");

        // Refill a freed slot when one exists; otherwise append and give the
        // new slot generation 0.
        let index = match metadata.free_slots.pop() {
            Some(index) => {
                storage.data[index as usize] = Some(asset);
                index
            }
            None => {
                let index = storage.push(asset) as u32;
                metadata.generations.push(0);
                index
            }
        };

        self.revision = self.revision.wrapping_add(1);

        Handle {
            index,
            generation: metadata.generations[index as usize],
            _marker: PhantomData,
        }
    }

    /// Store `asset` under `name` and return a handle to it.
    ///
    /// The name is a lookup key for code that resolves assets by string - a
    /// scene file naming a mesh, say. Rebinding a name that is already in use
    /// points it at the new asset and leaves the old one reachable by its
    /// handle; it does not unload anything.
    pub fn add_named<T>(&mut self, name: impl Into<String>, asset: T) -> Handle<T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let handle = self.add(asset);
        let metadata = self
            .metadata
            .get_mut(&TypeId::of::<T>())
            .expect("add created the column metadata");
        let name = name.into();
        // Drop any previous reverse entry for this name so `names` cannot
        // accumulate stale index -> name pairs when a name is rebound.
        if let Some(previous_index) = metadata.by_name.insert(name.clone(), handle.index) {
            metadata.names.remove(&previous_index);
        }
        metadata.names.insert(handle.index, name);
        handle
    }

    /// Store `asset` under `guid` and return a handle to it.
    ///
    /// The guid twin of [`Self::add_named`], and it rebinds the same way: a
    /// guid already in use points at the new asset and leaves the old one
    /// reachable by its handle. Nothing is unloaded.
    pub fn add_with_guid<T>(&mut self, guid: AssetGuid, asset: T) -> Handle<T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let handle = self.add(asset);
        self.bind_guid::<T>(guid, handle.index);
        handle
    }

    /// Store `asset` under both a name and a guid.
    ///
    /// The usual shape for a cooked asset: the name is what a scene file
    /// writes, the guid is what survives the name changing.
    pub fn add_named_with_guid<T>(
        &mut self,
        name: impl Into<String>,
        guid: AssetGuid,
        asset: T,
    ) -> Handle<T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let handle = self.add_named(name, asset);
        self.bind_guid::<T>(guid, handle.index);
        handle
    }

    /// Point `guid` at the asset already living in `index`.
    ///
    /// Shared by the two guid-assigning entry points. Drops any previous
    /// reverse entry so `guids` cannot accumulate stale index -> guid pairs
    /// when a guid is rebound, exactly as `add_named` does for names.
    fn bind_guid<T>(&mut self, guid: AssetGuid, index: u32)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self
            .metadata
            .get_mut(&TypeId::of::<T>())
            .expect("add created the column metadata");
        if let Some(previous_index) = metadata.by_guid.insert(guid, index) {
            metadata.guids.remove(&previous_index);
        }
        metadata.guids.insert(index, guid);
    }

    /// Borrow the asset `handle` refers to, or `None` when it is stale.
    pub fn get<T>(&self, handle: Handle<T>) -> Option<&T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }
        self.columns.get_storage::<T>().get(handle.index as usize)
    }

    /// Mutably borrow the asset `handle` refers to, or `None` when it is stale.
    pub fn get_mut<T>(&mut self, handle: Handle<T>) -> Option<&mut T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }
        self.revision = self.revision.wrapping_add(1);
        self.columns
            .get_storage_mut::<T>()
            .get_mut(handle.index as usize)
    }

    /// Resolve a name to a live handle, or `None` when nothing holds it.
    pub fn handle_by_name<T>(&self, name: &str) -> Option<Handle<T>>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self.metadata.get(&TypeId::of::<T>())?;
        let index = *metadata.by_name.get(name)?;
        Some(Handle {
            index,
            generation: *metadata.generations.get(index as usize)?,
            _marker: PhantomData,
        })
    }

    /// Borrow an asset by name, or `None` when nothing holds it.
    pub fn get_by_name<T>(&self, name: &str) -> Option<&T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.get(self.handle_by_name::<T>(name)?)
    }

    /// Resolve a guid to a live handle, or `None` when nothing holds it.
    pub fn handle_by_guid<T>(&self, guid: AssetGuid) -> Option<Handle<T>>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self.metadata.get(&TypeId::of::<T>())?;
        let index = *metadata.by_guid.get(&guid)?;
        Some(Handle {
            index,
            generation: *metadata.generations.get(index as usize)?,
            _marker: PhantomData,
        })
    }

    /// Borrow an asset by guid, or `None` when nothing holds it.
    pub fn get_by_guid<T>(&self, guid: AssetGuid) -> Option<&T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.get(self.handle_by_guid::<T>(guid)?)
    }

    /// Mutably borrow an asset by name, or `None` when nothing holds it.
    pub fn get_by_name_mut<T>(&mut self, name: &str) -> Option<&mut T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.get_mut(self.handle_by_name::<T>(name)?)
    }

    /// Mutably borrow an asset by guid, or `None` when nothing holds it.
    pub fn get_by_guid_mut<T>(&mut self, guid: AssetGuid) -> Option<&mut T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.get_mut(self.handle_by_guid::<T>(guid)?)
    }

    /// The name `handle` was stored under, or `None` when it has none.
    pub fn name_of<T>(&self, handle: Handle<T>) -> Option<&str>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }
        self.metadata
            .get(&TypeId::of::<T>())?
            .names
            .get(&handle.index)
            .map(String::as_str)
    }

    /// The guid `handle` was stored under, or `None` when it has none.
    pub fn guid_of<T>(&self, handle: Handle<T>) -> Option<AssetGuid>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }
        self.metadata
            .get(&TypeId::of::<T>())?
            .guids
            .get(&handle.index)
            .copied()
    }

    /// Remove and return the asset `handle` refers to.
    ///
    /// The slot is freed and its generation bumped, so every outstanding handle
    /// to it - including this one - now resolves to `None`. Returns `None` when
    /// the handle was already stale, which makes a double removal a no-op
    /// rather than an error.
    pub fn remove<T>(&mut self, handle: Handle<T>) -> Option<T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }

        let asset = self
            .columns
            .get_storage_mut::<T>()
            .take(handle.index as usize)?;

        let metadata = self
            .metadata
            .get_mut(&TypeId::of::<T>())
            .expect("a live handle implies existing metadata");

        // Bump the generation so this handle, and every copy of it, stops
        // resolving. Saturating rather than wrapping: at u32::MAX the slot is
        // retired instead of silently validating ancient handles again.
        let generation = &mut metadata.generations[handle.index as usize];
        *generation = generation.saturating_add(1);
        if *generation < u32::MAX {
            metadata.free_slots.push(handle.index);
        }

        if let Some(name) = metadata.names.remove(&handle.index) {
            metadata.by_name.remove(&name);
        }
        if let Some(guid) = metadata.guids.remove(&handle.index) {
            metadata.by_guid.remove(&guid);
        }

        self.revision = self.revision.wrapping_add(1);

        Some(asset)
    }

    /// Whether `handle` still refers to a live asset.
    pub fn contains<T>(&self, handle: Handle<T>) -> bool
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.is_live(handle)
            && self
                .columns
                .get_storage::<T>()
                .get(handle.index as usize)
                .is_some()
    }

    /// Number of live assets of type `T`.
    pub fn len<T>(&self) -> usize
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.metadata.contains_key(&TypeId::of::<T>()) {
            return 0;
        }
        self.columns.get_storage::<T>().iter().count()
    }

    /// Whether no asset of type `T` is stored.
    pub fn is_empty<T>(&self) -> bool
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.len::<T>() == 0
    }

    /// Iterate every live asset of type `T`.
    ///
    /// Order is slot order, which is insertion order until the first removal
    /// and arbitrary afterwards; callers needing a stable order must impose
    /// one themselves.
    pub fn iter<T>(&self) -> impl Iterator<Item = &T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        // `Option` is iterable, so an unregistered type yields nothing rather
        // than needing the column to exist.
        self.metadata
            .contains_key(&TypeId::of::<T>())
            .then(|| self.columns.get_storage::<T>().iter())
            .into_iter()
            .flatten()
    }

    /// Iterate every live asset of type `T` with its handle.
    pub fn iter_handles<T>(&self) -> impl Iterator<Item = (Handle<T>, &T)>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self.metadata.get(&TypeId::of::<T>());
        metadata
            .map(|metadata| {
                self.columns
                    .get_storage::<T>()
                    .data
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, slot)| {
                        let asset = slot.as_ref()?;
                        Some((
                            Handle {
                                index: index as u32,
                                generation: metadata.generations[index],
                                _marker: PhantomData,
                            },
                            asset,
                        ))
                    })
            })
            .into_iter()
            .flatten()
    }

    /// Whether `handle`'s generation still matches its slot.
    ///
    /// The single place a handle is validated, so every accessor agrees on what
    /// "stale" means.
    fn is_live<T: Asset>(&self, handle: Handle<T>) -> bool {
        self.metadata
            .get(&TypeId::of::<T>())
            .and_then(|metadata| metadata.generations.get(handle.index as usize))
            .is_some_and(|generation| *generation == handle.generation)
    }

    /// Create the column and metadata for `T` if this is its first use.
    ///
    /// Registration is implicit rather than an explicit call a caller could
    /// forget: `register_type_storage` panics on a second registration, so the
    /// check belongs here where it can be made idempotent.
    fn ensure_column<T>(&mut self)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if self.metadata.contains_key(&TypeId::of::<T>()) {
            return;
        }
        self.columns.register_type_storage::<T>();
        self.metadata
            .insert(TypeId::of::<T>(), AssetColumn::default());
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use trait_type_map::impl_trait_accessible;

    #[derive(Debug, PartialEq)]
    struct Mesh(&'static str);
    impl Asset for Mesh {}
    impl_trait_accessible!(dyn Asset; Mesh);

    #[derive(Debug, PartialEq)]
    struct Texture(u32);
    impl Asset for Texture {}
    impl_trait_accessible!(dyn Asset; Texture);

    /// A guid resolves to the asset stored under it, and the two namespaces
    /// are independent: a name lookup does not answer a guid and vice versa.
    #[test]
    fn resolves_an_asset_by_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::from_name("textures/grass");
        let grass = assets.add_with_guid(guid, Texture(7));

        assert_eq!(assets.get_by_guid::<Texture>(guid), Some(&Texture(7)));
        assert_eq!(assets.handle_by_guid::<Texture>(guid), Some(grass));
        // Stored with a guid alone, so no name resolves to it.
        assert_eq!(assets.get_by_name::<Texture>("textures/grass"), None);
    }

    /// The usual cooked-asset shape: both keys address the same slot.
    #[test]
    fn name_and_guid_address_one_asset() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(0xdead_beef);
        let handle = assets.add_named_with_guid("grass", guid, Texture(1));

        assert_eq!(assets.handle_by_name::<Texture>("grass"), Some(handle));
        assert_eq!(assets.handle_by_guid::<Texture>(guid), Some(handle));
        assert_eq!(assets.name_of(handle), Some("grass"));
        assert_eq!(assets.guid_of(handle), Some(guid));
    }

    /// Freeing a slot must drop its guid entry, or the next asset to land in
    /// that slot would answer the dead guid - the wrong-asset failure the
    /// generational handle exists to prevent, reintroduced through the index.
    #[test]
    fn removing_an_asset_releases_its_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::from_name("textures/grass");
        let grass = assets.add_with_guid(guid, Texture(7));

        assets.remove(grass);
        assert_eq!(assets.get_by_guid::<Texture>(guid), None);

        // The freed slot is reused; the stale guid must not reach the new value.
        let stone = assets.add(Texture(9));
        assert_eq!(stone.index(), grass.index());
        assert_eq!(assets.get_by_guid::<Texture>(guid), None);
        assert_eq!(assets.guid_of(stone), None);
    }

    /// Rebinding points the guid at the new asset and leaves the old one
    /// reachable by handle, matching how `add_named` treats a name.
    #[test]
    fn rebinding_a_guid_moves_it_to_the_new_asset() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(42);
        let first = assets.add_with_guid(guid, Texture(1));
        let second = assets.add_with_guid(guid, Texture(2));

        assert_eq!(assets.get_by_guid::<Texture>(guid), Some(&Texture(2)));
        assert_eq!(assets.get(first), Some(&Texture(1)));
        // The displaced asset keeps its slot but no longer claims the guid.
        assert_eq!(assets.guid_of(first), None);
        assert_eq!(assets.guid_of(second), Some(guid));
    }

    /// Guids are per-type, like every other column key: one value does not
    /// collide across two asset types.
    #[test]
    fn guids_do_not_collide_across_types() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(1);
        assets.add_with_guid(guid, Texture(5));
        assets.add_with_guid(guid, Mesh("rock"));

        assert_eq!(assets.get_by_guid::<Texture>(guid), Some(&Texture(5)));
        assert_eq!(assets.get_by_guid::<Mesh>(guid), Some(&Mesh("rock")));
    }

    /// The name hash is a fixed function of the bytes, which is what lets a
    /// cooking step and the runtime derive one identity from one name.
    #[test]
    fn guid_from_name_is_stable_and_distinguishing() {
        assert_eq!(
            AssetGuid::from_name("textures/grass"),
            AssetGuid::from_name("textures/grass")
        );
        assert_ne!(
            AssetGuid::from_name("textures/grass"),
            AssetGuid::from_name("textures/stone")
        );
    }

    /// A stale handle answers nothing, on the guid accessors as on the rest.
    #[test]
    fn a_stale_handle_has_no_name_or_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(3);
        let handle = assets.add_named_with_guid("grass", guid, Texture(1));
        assets.remove(handle);

        assert_eq!(assets.name_of(handle), None);
        assert_eq!(assets.guid_of(handle), None);
    }

    /// Mutable lookup by either key reaches the same value.
    #[test]
    fn mutable_lookup_by_name_and_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(11);
        let handle = assets.add_named_with_guid("grass", guid, Texture(1));

        assets.get_by_name_mut::<Texture>("grass").unwrap().0 = 2;
        assert_eq!(assets.get(handle), Some(&Texture(2)));

        assets.get_by_guid_mut::<Texture>(guid).unwrap().0 = 3;
        assert_eq!(assets.get(handle), Some(&Texture(3)));
    }

    /// The point of the module: one type, many live values.
    #[test]
    fn stores_many_assets_of_one_type() {
        let mut assets = AssetManager::new();
        let rock = assets.add(Mesh("rock"));
        let tree = assets.add(Mesh("tree"));

        assert_ne!(rock, tree);
        assert_eq!(assets.get(rock), Some(&Mesh("rock")));
        assert_eq!(assets.get(tree), Some(&Mesh("tree")));
        assert_eq!(assets.len::<Mesh>(), 2);
    }

    /// Columns are per type and do not interfere.
    #[test]
    fn asset_types_are_stored_independently() {
        let mut assets = AssetManager::new();
        let mesh = assets.add(Mesh("rock"));
        let texture = assets.add(Texture(7));

        assert_eq!(assets.get(mesh), Some(&Mesh("rock")));
        assert_eq!(assets.get(texture), Some(&Texture(7)));
        assert_eq!(assets.len::<Mesh>(), 1);
        assert_eq!(assets.len::<Texture>(), 1);
    }

    /// An unused type reads as empty rather than panicking on a missing column.
    #[test]
    fn unregistered_types_read_as_empty() {
        let assets = AssetManager::new();

        assert_eq!(assets.len::<Mesh>(), 0);
        assert!(assets.is_empty::<Mesh>());
        assert_eq!(assets.iter::<Mesh>().count(), 0);
        assert_eq!(assets.handle_by_name::<Mesh>("rock"), None);
        assert_eq!(assets.get_by_name::<Mesh>("rock"), None);
    }

    /// Assets are mutable in place through their handle.
    #[test]
    fn assets_can_be_mutated_through_a_handle() {
        let mut assets = AssetManager::new();
        let handle = assets.add(Texture(1));

        *assets.get_mut(handle).expect("live handle") = Texture(2);

        assert_eq!(assets.get(handle), Some(&Texture(2)));
    }

    /// Render upload caches can cheaply observe every data-changing operation.
    #[test]
    fn revision_changes_when_asset_data_can_change() {
        let mut assets = AssetManager::new();
        let initial = assets.revision();
        let handle = assets.add(Texture(1));
        let after_add = assets.revision();
        assert_ne!(after_add, initial);

        assets.get_mut(handle).expect("live handle").0 = 2;
        let after_mutation = assets.revision();
        assert_ne!(after_mutation, after_add);

        assets.remove(handle);
        assert_ne!(assets.revision(), after_mutation);
    }

    /// Removing yields the asset and invalidates the handle.
    #[test]
    fn removing_returns_the_asset_and_invalidates_the_handle() {
        let mut assets = AssetManager::new();
        let handle = assets.add(Mesh("rock"));

        assert_eq!(assets.remove(handle), Some(Mesh("rock")));
        assert_eq!(assets.get(handle), None);
        assert!(!assets.contains(handle));
        // Removing twice is a no-op, not a panic.
        assert_eq!(assets.remove(handle), None);
    }

    /// The reason handles carry a generation: a refilled slot must not
    /// resurrect an old handle as a pointer to the new asset.
    #[test]
    fn a_stale_handle_does_not_alias_the_asset_that_reuses_its_slot() {
        let mut assets = AssetManager::new();
        let rock = assets.add(Mesh("rock"));
        assets.remove(rock);

        let tree = assets.add(Mesh("tree"));

        // The slot is reused, which is exactly the dangerous case.
        assert_eq!(rock.index(), tree.index());
        assert_ne!(rock.generation(), tree.generation());

        assert_eq!(assets.get(rock), None, "the stale handle must not resolve");
        assert_eq!(assets.get(tree), Some(&Mesh("tree")));
    }

    /// Freed slots are refilled rather than leaked.
    #[test]
    fn freed_slots_are_reused_before_the_column_grows() {
        let mut assets = AssetManager::new();
        let first = assets.add(Mesh("a"));
        let second = assets.add(Mesh("b"));
        assets.remove(first);

        let third = assets.add(Mesh("c"));

        assert_eq!(third.index(), first.index());
        assert_eq!(assets.len::<Mesh>(), 2);
        assert_eq!(assets.get(second), Some(&Mesh("b")));
    }

    /// Names resolve to handles and follow removal.
    #[test]
    fn names_resolve_to_live_handles() {
        let mut assets = AssetManager::new();
        let handle = assets.add_named("rock", Mesh("rock"));

        assert_eq!(assets.handle_by_name::<Mesh>("rock"), Some(handle));
        assert_eq!(assets.get_by_name::<Mesh>("rock"), Some(&Mesh("rock")));

        assets.remove(handle);
        assert_eq!(assets.handle_by_name::<Mesh>("rock"), None);
        assert_eq!(assets.get_by_name::<Mesh>("rock"), None);
    }

    /// Rebinding a name points it at the new asset and leaves the old one
    /// reachable by handle - it is a lookup change, not an unload.
    #[test]
    fn rebinding_a_name_leaves_the_previous_asset_alive() {
        let mut assets = AssetManager::new();
        let original = assets.add_named("mesh", Mesh("first"));
        let replacement = assets.add_named("mesh", Mesh("second"));

        assert_eq!(assets.handle_by_name::<Mesh>("mesh"), Some(replacement));
        assert_eq!(assets.get(original), Some(&Mesh("first")));
        assert_eq!(assets.len::<Mesh>(), 2);
    }

    /// A rebound name must not leave the displaced slot mapped, or removing
    /// that slot would drop the name the live asset still answers to.
    #[test]
    fn removing_a_displaced_asset_keeps_the_rebound_name() {
        let mut assets = AssetManager::new();
        let original = assets.add_named("mesh", Mesh("first"));
        let replacement = assets.add_named("mesh", Mesh("second"));

        assets.remove(original);

        assert_eq!(assets.handle_by_name::<Mesh>("mesh"), Some(replacement));
        assert_eq!(assets.get_by_name::<Mesh>("mesh"), Some(&Mesh("second")));
    }

    /// Iteration visits live assets only.
    #[test]
    fn iteration_skips_removed_slots() {
        let mut assets = AssetManager::new();
        let first = assets.add(Mesh("a"));
        assets.add(Mesh("b"));
        assets.add(Mesh("c"));
        assets.remove(first);

        let mut names: Vec<&str> = assets.iter::<Mesh>().map(|mesh| mesh.0).collect();
        names.sort_unstable();
        assert_eq!(names, ["b", "c"]);
    }

    /// Handle iteration returns handles that actually resolve.
    #[test]
    fn iter_handles_returns_resolvable_handles() {
        let mut assets = AssetManager::new();
        let first = assets.add(Mesh("a"));
        assets.add(Mesh("b"));
        assets.remove(first);
        assets.add(Mesh("c"));

        let pairs: Vec<(Handle<Mesh>, &Mesh)> = assets.iter_handles::<Mesh>().collect();
        assert_eq!(pairs.len(), 2);
        for (handle, asset) in pairs {
            assert_eq!(assets.get(handle), Some(asset));
        }
    }

    /// Handles are inert values: copyable, comparable, and hashable, so they
    /// can sit inside components and map keys.
    #[test]
    fn handles_are_copyable_and_hashable() {
        use std::collections::HashSet;

        let mut assets = AssetManager::new();
        let handle = assets.add(Mesh("rock"));
        let copy = handle;

        assert_eq!(handle, copy);
        let mut set = HashSet::new();
        set.insert(handle);
        assert!(set.contains(&copy));
    }

    /// The manager is a resource, so it reaches systems through the existing
    /// `Res` / `ResMut` parameters with no scheduler change.
    #[test]
    fn manager_round_trips_through_the_world_as_a_resource() {
        use crate::world::World;

        let mut world = World::new();
        let mut assets = AssetManager::new();
        let handle = assets.add(Mesh("rock"));
        world.insert_resource(assets);

        let stored = world
            .get_resource::<AssetManager>()
            .expect("manager was inserted");
        assert_eq!(stored.get(handle), Some(&Mesh("rock")));
    }

    #[test]
    fn asset_loader_returns_embedded_bytes() {
        let loader = AssetLoader::Bytes(vec![1, 2, 3].into_boxed_slice());
        assert_eq!(loader.load().unwrap(), [1, 2, 3]);
    }

    #[test]
    fn asset_loader_resolves_paths_below_its_root() {
        let root = std::env::temp_dir().join(format!(
            "pill-asset-loader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sample.wgsl"), b"shader").unwrap();
        AssetLoader::set_root(&root);

        let bytes = AssetLoader::Path("sample.wgsl".into()).load().unwrap();

        assert_eq!(bytes, b"shader");
        std::fs::remove_dir_all(root).unwrap();
    }
}
