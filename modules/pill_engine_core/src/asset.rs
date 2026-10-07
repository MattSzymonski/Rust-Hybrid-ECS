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
//! Storage mirrors how components are held - an erased column per type in a
//! `TraitTypeMap` - minus archetypes, which exist to group *entities* by their
//! component set and have no meaning for an asset. The column is the same
//! `ErasedVecStorage` the component side uses, so its per-type behaviour is a
//! replaceable data table rather than a trait-object vtable: a handle keeps its
//! slot index across an unload, and a column can be re-pointed at whichever
//! generation is still mapped (see [`AssetManager::rehome`]).
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
use std::path::PathBuf;

// External crates
use trait_type_map::{
    ErasedVecFamily, ErasedVecStorage, ErasedVecStorageInfo, ErasedVecStorageOps, TraitAccessible,
    TraitTypeMap,
};

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
pub trait Asset: Send + Sync + 'static {
    /// A stable name that identifies this asset type across binaries, or
    /// `None` to be identified by its `TypeId` alone.
    ///
    /// The asset counterpart of [`Component::shared_name`](crate::Component::shared_name).
    /// Every binary that compiles its own copy of the defining crate - with
    /// different cargo features, say, as a module and the crates that link it
    /// do - gets a different `TypeId` for the same type, so a `TypeId`-keyed
    /// store would hand each binary its own empty column. A shared name makes
    /// them one column: the store keys it by the name's identity, and checks
    /// each binary's type against the column by layout.
    ///
    /// Pin a literal rather than deriving it from the module path, so moving
    /// the type does not change its identity. Every copy must be compiled from
    /// the same source; the column checks size and alignment, as it does for
    /// shared components.
    fn shared_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// The stable identity [`Self::shared_name`] hashes to, or `None`.
    ///
    /// The same hash shared components use. Override it only with that
    /// function of `Self::shared_name()`.
    fn shared_identity() -> Option<u128>
    where
        Self: Sized,
    {
        Self::shared_name().map(crate::component::shared_component_identity)
    }
}

// =============================================================================
// AssetLoader
// =============================================================================

/// Source used to initialize an asset from a project file or embedded bytes.
///
/// A relative path names a file of the project's `res` directory and resolves
/// through the asset store's mount points ([`crate::asset_store`]): a packed
/// copy in a shipping or web build, the file itself in development. Loading
/// code is the same on every target.
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
    /// An asset's metadata file exists but cannot be used: it does not parse,
    /// names another asset type, or was written by a newer format version.
    #[error("asset metadata {path} cannot be used: {detail}")]
    Metadata {
        /// The metadata file, relative to the project's asset directory.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
    },
    /// Writing a file into the project's asset directory failed.
    #[error("failed to write asset file {path}: {source}")]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Result of loading an asset's bytes from wherever its [`AssetLoader`] points.
pub type AssetLoadResult<T> = Result<T, AssetLoadError>;

/// Refusal to store an asset under a key another asset already answers to.
///
/// Kept apart from [`AssetLoadError`]: nothing failed to load, the store simply
/// would not take a second asset under a name that is already live.
#[derive(Debug, thiserror::Error)]
pub enum AssetBindingError {
    /// The name already addresses a live asset.
    #[error("an asset named `{name}` already exists")]
    NameInUse {
        /// The name that is already taken.
        name: String,
    },
    /// The handle no longer addresses a live asset.
    #[error("the asset handle is stale: its asset was removed")]
    StaleHandle,
}

/// Result of a binding operation that can hit an occupied name.
pub type AssetBindingResult<T> = Result<T, AssetBindingError>;

impl AssetLoader {
    /// Mount the project's asset directory, which relative [`Self::Path`]
    /// values resolve beneath; see [`crate::asset_store::mount_directory`].
    pub fn set_root(path: impl Into<PathBuf>) {
        crate::asset_store::mount_directory(path);
    }

    /// Return the currently mounted asset directory, if one was set.
    pub fn root() -> Option<PathBuf> {
        crate::asset_store::mounted_directory()
    }

    /// Read this source into owned bytes.
    ///
    /// A path is read through the asset store's mount points: packs first,
    /// then the filesystem - see [`crate::asset_store::read`].
    pub fn load(&self) -> AssetLoadResult<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.to_vec()),
            Self::Path(path) => crate::asset_store::read(path),
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

    /// Reconstructs a handle from a raw index/generation pair.
    ///
    /// The same trust model [`Deserialize`](serde::Deserialize) already
    /// applies: the caller vouches that the pair came from a handle this
    /// asset manager issued, most often across an FFI boundary that carries
    /// only the two integers rather than a live handle value.
    #[inline]
    pub fn from_raw(index: u32, generation: u32) -> Self {
        Self {
            index,
            generation,
            _marker: PhantomData,
        }
    }

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
    /// Size and alignment of the column's element type, as the first
    /// registration declared it; what a later shared registrant must match.
    element_layout: (usize, usize),
    /// Generation of each slot, one entry per slot that has ever existed.
    ///
    /// Starts at 0 for a fresh slot and increments on every free, so a handle
    /// issued before the free never matches after it.
    generations: Vec<u32>,
    /// Content version of each slot, one entry per slot that has ever existed.
    ///
    /// Bumped whenever the stored value is mutably borrowed, so a consumer
    /// that mirrors assets - the renderer's GPU uploads - can tell an edited
    /// asset from an untouched one instead of rebuilding everything when any
    /// asset changes. A freed slot keeps its last entry; the handle that
    /// refills it carries a new generation, so a version only ever compares
    /// within one handle.
    content_versions: Vec<u64>,
    /// Slots freed by `remove`, refilled before the column grows.
    free_slots: Vec<u32>,
    /// Row the live slot at this index occupies in the packed column.
    ///
    /// Rows are packed, so removing one swaps the last row into the hole. This
    /// map and `row_slots` are what keep the *slot* index stable through that
    /// swap, and the slot index is what a handle addresses. An entry for a free
    /// slot holds whatever the last occupant left.
    slot_rows: Vec<u32>,
    /// Slot owning each row; the reverse of `slot_rows`.
    row_slots: Vec<u32>,
    /// Name lookup for assets added with one.
    ///
    /// Holds the index only: the generation is read from `generations` at
    /// lookup time, so a name that outlived a free cannot resolve to whatever
    /// refilled the slot.
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

    /// A new guid from the operating system's random source.
    ///
    /// What an asset's metadata file is given when it is first written. It is
    /// deliberately unrelated to the asset's path: a guid derived from the path
    /// would be handed to whatever file later took that path, and a stale
    /// reference would then resolve to the wrong asset instead of to nothing.
    ///
    /// # Errors
    ///
    /// Returns the `getrandom` error when the platform has no usable random
    /// source.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(u128::from_le_bytes(bytes)))
    }

    /// Parse the 32-digit hexadecimal form [`Display`](std::fmt::Display)
    /// writes, or `None` for any other text.
    pub fn parse(text: &str) -> Option<Self> {
        // Exactly 32 hex digits: `from_str_radix` alone would also accept a
        // sign and a shorter string.
        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u128::from_str_radix(text, 16).ok().map(Self)
    }
}

impl std::fmt::Display for AssetGuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

// Serialized as the 32-digit hex string rather than a number: a 128-bit integer
// does not survive JSON number handling in most other tools, C# included.
impl serde::Serialize for AssetGuid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for AssetGuid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{text}` is not an asset guid (expected 32 hexadecimal digits)"
            ))
        })
    }
}

// =============================================================================
// AssetManager
// =============================================================================

/// Rebuild one asset type's function table from the calling generation's code.
///
/// The address of this function is what [`AssetManager::register`] records per
/// type. Calling it from a freshly loaded generation re-points that type's
/// column at that generation, which is what keeps the column usable after the
/// generation that filled it is evicted from the reload graveyard.
fn refresh_column_ops<T>(columns: &mut TraitTypeMap<dyn Asset, ErasedVecFamily>, key: TypeId)
where
    T: Asset + TraitAccessible<dyn Asset>,
{
    if let Some(column) = columns.get_trait_storage_mut(key) {
        column.refresh_ops(ErasedVecStorageOps::of::<T>());
    }
}

/// How to refresh one asset type's column, given its storage key; see
/// [`refresh_column_ops`].
type AssetOpsRefresher = fn(&mut TraitTypeMap<dyn Asset, ErasedVecFamily>, TypeId);

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
/// let tree = assets.add_named("tree", Mesh("tree")).expect("a fresh name");
///
/// assert_eq!(assets.get(rock), Some(&Mesh("rock")));
/// assert_eq!(assets.handle_by_name::<Mesh>("tree"), Some(tree));
/// ```
#[derive(Default)]
pub struct AssetManager {
    /// One erased column per asset type, holding the values themselves.
    columns: TraitTypeMap<dyn Asset, ErasedVecFamily>,
    /// Slot bookkeeping for each column, keyed by the same storage key.
    ///
    /// A storage key is a type's own `TypeId`, except for a shared asset type
    /// (see [`Asset::shared_name`]), whose key is the `TypeId` of the first
    /// binary that registered it; see [`Self::shared_keys`].
    metadata: HashMap<TypeId, AssetColumn>,
    /// The storage key of each shared asset type, by its shared identity.
    ///
    /// Recorded by the first registration and never replaced, so every binary's
    /// copy of the type reaches the one column. A `TypeId` is only a value
    /// here: it stays a valid key after the binary it came from is unloaded.
    shared_keys: HashMap<u128, TypeId>,
    /// How to rebuild each registered type's table from the generation that
    /// last registered it; see [`Self::rehome`].
    ops_refreshers: HashMap<TypeId, AssetOpsRefresher>,
    /// Changes whenever stored asset data may have changed.
    revision: u64,
}

/// The shared name C# reaches the store under, as
/// `ResMut<TracyLive.AssetManager>`.
///
/// Shared rather than `TypeId`-keyed so the identity is a name both languages
/// can compute: the managed marker type hashes the same string, and every
/// artifact that links the engine agrees on it without comparing `TypeId`s.
pub const ASSET_MANAGER_SHARED_NAME: &str = "pill_engine::asset::AssetManager";

impl Resource for AssetManager {
    fn shared_name() -> Option<&'static str> {
        Some(ASSET_MANAGER_SHARED_NAME)
    }
}

impl AssetManager {
    /// The storage key `T`'s column is kept under, or `None` when `T` is a
    /// shared type no binary has registered yet.
    #[inline]
    fn key_of<T: Asset>(&self) -> Option<TypeId> {
        match T::shared_identity() {
            Some(identity) => self.shared_keys.get(&identity).copied(),
            None => Some(TypeId::of::<T>()),
        }
    }

    /// `T`'s column bookkeeping, or `None` when `T` has no column.
    #[inline]
    fn metadata_of<T: Asset>(&self) -> Option<&AssetColumn> {
        self.metadata.get(&self.key_of::<T>()?)
    }

    /// `T`'s column bookkeeping, mutably, or `None` when `T` has no column.
    #[inline]
    fn metadata_of_mut<T: Asset>(&mut self) -> Option<&mut AssetColumn> {
        let key = self.key_of::<T>()?;
        self.metadata.get_mut(&key)
    }

    /// `T`'s column. Panics when `T` has none: callers check first.
    #[inline]
    fn column<T: Asset>(&self) -> &ErasedVecStorage<dyn Asset> {
        let key = self.key_of::<T>().expect("the asset type is registered");
        self.columns
            .get_trait_storage(key)
            .expect("the asset type's column exists")
    }

    /// `T`'s column, mutably. Panics when `T` has none: callers check first.
    #[inline]
    fn column_mut<T: Asset>(&mut self) -> &mut ErasedVecStorage<dyn Asset> {
        let key = self.key_of::<T>().expect("the asset type is registered");
        self.columns
            .get_trait_storage_mut(key)
            .expect("the asset type's column exists")
    }

    /// Create an empty manager holding no asset types.
    ///
    /// Types register themselves on first use, so nothing needs declaring up
    /// front - a project that loads no meshes carries no mesh column. Declaring
    /// a type with [`Self::register`] is for types whose owning artifact
    /// reloads; see that method for why it matters there.
    pub fn new() -> Self {
        Self::default()
    }

    /// Monotonic change counter used by renderer upload caches.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Content version of the slot `handle` names, or `None` when stale.
    ///
    /// Bumped whenever the value is mutably borrowed, so a consumer that
    /// mirrors assets can rebuild the one asset that changed instead of every
    /// asset. Meaningful only within one handle: a refilled slot is addressed
    /// by a new generation, whose version starts fresh.
    pub fn content_version<T>(&self, handle: Handle<T>) -> Option<u64>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if !self.is_live(handle) {
            return None;
        }
        self.metadata_of::<T>()
            .and_then(|metadata| metadata.content_versions.get(handle.index as usize))
            .copied()
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

        let key = self
            .key_of::<T>()
            .expect("ensure_column registered the type");
        let storage = self
            .columns
            .get_trait_storage_mut(key)
            .expect("the column is created alongside its metadata");
        let metadata = self
            .metadata
            .get_mut(&key)
            .expect("column metadata is created alongside the column");

        // Refill a freed slot when one exists; otherwise append one and give it
        // generation 0. The value always lands in a fresh row, so a refilled
        // slot never inherits the row its previous occupant occupied.
        let index = match metadata.free_slots.pop() {
            Some(index) => index,
            None => {
                let index = metadata.slot_rows.len() as u32;
                metadata.slot_rows.push(0);
                metadata.generations.push(0);
                metadata.content_versions.push(0);
                index
            }
        };
        let row = storage.len() as u32;
        storage.push(asset);
        metadata.slot_rows[index as usize] = row;
        metadata.row_slots.push(index);

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
    /// scene file naming a mesh, say. A name that already addresses a live
    /// asset is refused rather than rebound, so a second load of the same key
    /// is reported instead of quietly adding a copy that nothing looks up.
    /// Free the name with [`Self::remove`] first when a deliberate replacement
    /// is wanted.
    ///
    /// # Errors
    ///
    /// Returns [`AssetBindingError::NameInUse`] when `name` already addresses a
    /// live asset. Nothing is stored and no handle is issued.
    pub fn add_named<T>(
        &mut self,
        name: impl Into<String>,
        asset: T,
    ) -> AssetBindingResult<Handle<T>>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let name = name.into();
        // Checked before `add`, so a refused name cannot leave an unreachable
        // asset behind in the column.
        if self.handle_by_name::<T>(&name).is_some() {
            return Err(AssetBindingError::NameInUse { name });
        }

        let handle = self.add(asset);
        let metadata = self
            .metadata_of_mut::<T>()
            .expect("add created the column metadata");
        metadata.by_name.insert(name.clone(), handle.index);
        metadata.names.insert(handle.index, name);
        Ok(handle)
    }

    /// Give the live asset `handle` a new name, keeping its handle, guid and
    /// value.
    ///
    /// What following a moved source file needs: everything that holds the
    /// handle or the guid keeps working, and only lookups by the old name stop
    /// resolving. Renaming an asset to the name it already has is a no-op.
    ///
    /// # Errors
    ///
    /// [`AssetBindingError::NameInUse`] when another live asset has
    /// `new_name`, and [`AssetBindingError::StaleHandle`] when `handle` is
    /// stale. Nothing changes on an error.
    pub fn rename<T>(
        &mut self,
        handle: Handle<T>,
        new_name: impl Into<String>,
    ) -> AssetBindingResult<()>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let new_name = new_name.into();
        if !self.is_live(handle) {
            return Err(AssetBindingError::StaleHandle);
        }
        match self.handle_by_name::<T>(&new_name) {
            Some(existing) if existing == handle => return Ok(()),
            Some(_) => return Err(AssetBindingError::NameInUse { name: new_name }),
            None => {}
        }
        let metadata = self
            .metadata_of_mut::<T>()
            .expect("a live handle has column metadata");
        if let Some(old_name) = metadata.names.insert(handle.index, new_name.clone()) {
            metadata.by_name.remove(&old_name);
        }
        metadata.by_name.insert(new_name, handle.index);
        Ok(())
    }

    /// Store `asset` under `guid` and return a handle to it.
    ///
    /// Unlike [`Self::add_named`], a guid already in use is rebound: it points
    /// at the new asset and leaves the old one reachable by its handle. Nothing
    /// is unloaded.
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
    ///
    /// # Errors
    ///
    /// Returns [`AssetBindingError::NameInUse`] on the same terms as
    /// [`Self::add_named`].
    pub fn add_named_with_guid<T>(
        &mut self,
        name: impl Into<String>,
        guid: AssetGuid,
        asset: T,
    ) -> AssetBindingResult<Handle<T>>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let handle = self.add_named(name, asset)?;
        self.bind_guid::<T>(guid, handle.index);
        Ok(handle)
    }

    /// Point `guid` at the asset already living in `index`.
    ///
    /// Shared by the two guid-assigning entry points. Drops any previous
    /// reverse entry so `guids` cannot accumulate stale index -> guid pairs
    /// when a guid is rebound.
    fn bind_guid<T>(&mut self, guid: AssetGuid, index: u32)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self
            .metadata_of_mut::<T>()
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
        let row = self.slot_row::<T>(handle.index)?;
        Some(self.column::<T>().get::<T>(row as usize))
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
        // One counter per slot, not per column: it is what tells a mirrored
        // consumer which single asset moved, and borrowing one value must not
        // look like a change to every asset.
        let version = &mut self
            .metadata_of_mut::<T>()
            .expect("a live handle implies existing metadata")
            .content_versions[handle.index as usize];
        *version = version.wrapping_add(1);
        let row = self.slot_row::<T>(handle.index)?;
        Some(self.column_mut::<T>().get_mut::<T>(row as usize))
    }

    /// Resolve a name to a live handle, or `None` when nothing holds it.
    pub fn handle_by_name<T>(&self, name: &str) -> Option<Handle<T>>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self.metadata_of::<T>()?;
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
        let metadata = self.metadata_of::<T>()?;
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
        self.metadata_of::<T>()?
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
        self.metadata_of::<T>()?.guids.get(&handle.index).copied()
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

        let type_id = self
            .key_of::<T>()
            .expect("a live handle implies a registered type");
        let row = self
            .metadata
            .get(&type_id)
            .expect("a live handle implies existing metadata")
            .slot_rows[handle.index as usize];

        let storage = self
            .columns
            .get_trait_storage_mut(type_id)
            .expect("a live handle implies an existing column");
        let asset = storage.swap_remove::<T>(row as usize);
        // A packed column moves its last row into the hole; that row's slot
        // takes over the vacated index below, so handles follow the slot and
        // not the row.
        let moved_row = (row as usize) < storage.len();

        let metadata = self
            .metadata
            .get_mut(&type_id)
            .expect("a live handle implies existing metadata");

        // The popped entry is the slot of the row that moved in, or the removed
        // slot's own when the last row went - which needs no fix-up either way.
        let last_slot = metadata.row_slots.pop().expect("a live slot owns a row");
        if moved_row {
            metadata.slot_rows[last_slot as usize] = row;
            metadata.row_slots[row as usize] = last_slot;
        }

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
        self.is_live(handle) && self.slot_row::<T>(handle.index).is_some()
    }

    /// Number of live assets of type `T`.
    pub fn len<T>(&self) -> usize
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        if self.metadata_of::<T>().is_none() {
            return 0;
        }
        self.column::<T>().len()
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
    /// Order is row order, which is insertion order until the first removal
    /// and arbitrary afterwards; callers needing a stable order must impose
    /// one themselves.
    pub fn iter<T>(&self) -> impl Iterator<Item = &T>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        // An unregistered type yields nothing rather than needing the column
        // to exist.
        self.metadata_of::<T>()
            .is_some()
            .then(|| self.column::<T>().iter::<T>())
            .into_iter()
            .flatten()
    }

    /// Iterate every live asset of type `T` with its handle.
    pub fn iter_handles<T>(&self) -> impl Iterator<Item = (Handle<T>, &T)>
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let metadata = self.metadata_of::<T>();
        metadata
            .map(|metadata| {
                self.column::<T>()
                    .iter::<T>()
                    .enumerate()
                    .map(move |(row, asset)| {
                        // Rows are packed; the handle names the slot the row
                        // lives in, which is the index that survives a swap.
                        let slot = metadata.row_slots[row];
                        (
                            Handle {
                                index: slot,
                                generation: metadata.generations[slot as usize],
                                _marker: PhantomData,
                            },
                            asset,
                        )
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
        self.metadata_of::<T>()
            .and_then(|metadata| metadata.generations.get(handle.index as usize))
            .is_some_and(|generation| *generation == handle.generation)
    }

    /// Row the live slot at `index` occupies, or `None` when nothing lives there.
    ///
    /// The reverse lookup is what proves occupancy: a freed slot's stale row
    /// entry answers to a different slot (or to no row), so even a handle that
    /// guessed a freed slot's generation cannot reach another asset's row.
    fn slot_row<T: Asset>(&self, index: u32) -> Option<u32> {
        let metadata = self.metadata_of::<T>()?;
        let row = *metadata.slot_rows.get(index as usize)?;
        (metadata.row_slots.get(row as usize) == Some(&index)).then_some(row)
    }

    /// Declare `T` as an asset type and re-point its column at this generation.
    ///
    /// Creating the column is implicit in using the type - every `add` lands
    /// here first - but a *declaration* also matters across a reload: the
    /// column's per-type table is code from the artifact that filled it, so a
    /// generation that owns an asset type should declare it in registration
    /// even when it will not add an instance this run. Declaring twice is
    /// harmless; the second call only refreshes the table.
    ///
    /// `register_type_storage` panics on a second registration, so the
    /// existence check belongs here where it can be made idempotent.
    ///
    /// A shared asset type ([`Asset::shared_name`]) is keyed by the `TypeId`
    /// of the first binary that registers it, and its column is built to check
    /// element types by layout: every binary's copy of the type then reaches
    /// that one column. A later registrant's copy must match its size and
    /// alignment; one that does not is refused with a panic here, before it can
    /// read a row.
    pub fn register<T>(&mut self)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        let key = match T::shared_identity() {
            Some(identity) => *self
                .shared_keys
                .entry(identity)
                .or_insert_with(TypeId::of::<T>),
            None => TypeId::of::<T>(),
        };
        if let Some(metadata) = self.metadata.get(&key) {
            // Checked here, where a mismatch is a registration problem with
            // one clear cause, rather than at the first read of a row.
            assert!(
                metadata.element_layout == (std::mem::size_of::<T>(), std::mem::align_of::<T>()),
                "asset type {} does not match the layout of the shared column it is registered to",
                std::any::type_name::<T>()
            );
            self.ops_refreshers.insert(key, refresh_column_ops::<T>);
            if let Some(column) = self.columns.get_trait_storage_mut(key) {
                column.refresh_ops(ErasedVecStorageOps::of::<T>());
            }
            return;
        }
        self.ops_refreshers.insert(key, refresh_column_ops::<T>);
        if T::shared_identity().is_some() {
            self.columns
                .insert_erased(ErasedVecStorage::new(ErasedVecStorageInfo::of_shared::<T>()));
        } else {
            self.columns.register_type_storage::<T>();
        }
        self.metadata.insert(
            key,
            AssetColumn {
                element_layout: (std::mem::size_of::<T>(), std::mem::align_of::<T>()),
                ..AssetColumn::default()
            },
        );
    }

    /// Re-point every registered asset column at its newest function table.
    ///
    /// The reload transaction calls this beside the component and resource
    /// re-homing passes, after the arriving generation registered and before
    /// any retiring image can be evicted: a column keeps working only while its
    /// table points at code that is still mapped. A type the arriving
    /// generation neither declared nor used keeps the table it has - there is
    /// nothing newer to point it at.
    pub fn rehome(&mut self) {
        for (key, refresher) in &self.ops_refreshers {
            refresher(&mut self.columns, *key);
        }
    }

    /// Create the column and metadata for `T` if this is its first use.
    fn ensure_column<T>(&mut self)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        self.register::<T>();
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use trait_type_map::impl_trait_accessible;

    /// A rename keeps the handle and guid, moves the name, and refuses a name
    /// another asset has or a stale handle.
    #[test]
    fn rename_keeps_the_handle_and_guid_and_refuses_a_taken_name() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(7);
        let moved = assets
            .add_named_with_guid("old.png", guid, Mesh("moved"))
            .unwrap();
        let other = assets.add_named("taken.png", Mesh("other")).unwrap();

        assets.rename(moved, "new.png").unwrap();
        assert_eq!(assets.handle_by_name::<Mesh>("new.png"), Some(moved));
        assert_eq!(assets.handle_by_name::<Mesh>("old.png"), None);
        assert_eq!(assets.handle_by_guid::<Mesh>(guid), Some(moved));
        assert_eq!(assets.name_of(moved), Some("new.png"));
        assert_eq!(assets.get(moved), Some(&Mesh("moved")));

        // Renaming to its own name is a no-op; a taken name is refused.
        assets.rename(moved, "new.png").unwrap();
        assert!(matches!(
            assets.rename(moved, "taken.png"),
            Err(AssetBindingError::NameInUse { .. })
        ));
        assert_eq!(assets.handle_by_name::<Mesh>("taken.png"), Some(other));

        assets.remove(moved);
        assert!(matches!(
            assets.rename(moved, "again.png"),
            Err(AssetBindingError::StaleHandle)
        ));
    }

    #[derive(Debug, PartialEq)]
    struct Mesh(&'static str);
    impl Asset for Mesh {}
    impl_trait_accessible!(dyn Asset; Mesh);

    #[derive(Debug, PartialEq)]
    struct Texture(u32);
    impl Asset for Texture {}
    impl_trait_accessible!(dyn Asset; Texture);

    /// The shared name the two copies below register under, standing in for
    /// one asset type compiled into two binaries (two distinct `TypeId`s).
    const SHARED_MESH: &str = "asset_tests::SharedMesh";

    /// One binary's copy of the shared asset type.
    #[derive(Debug, PartialEq)]
    struct WriterMesh {
        vertices: Vec<u32>,
    }
    impl Asset for WriterMesh {
        fn shared_name() -> Option<&'static str> {
            Some(SHARED_MESH)
        }
    }
    impl_trait_accessible!(dyn Asset; WriterMesh);

    /// Another binary's copy of the same type.
    #[derive(Debug, PartialEq)]
    struct ReaderMesh {
        vertices: Vec<u32>,
    }
    impl Asset for ReaderMesh {
        fn shared_name() -> Option<&'static str> {
            Some(SHARED_MESH)
        }
    }
    impl_trait_accessible!(dyn Asset; ReaderMesh);

    /// A type claiming the same shared name with a different layout.
    #[derive(Debug, PartialEq)]
    struct MismatchedMesh(u8);
    impl Asset for MismatchedMesh {
        fn shared_name() -> Option<&'static str> {
            Some(SHARED_MESH)
        }
    }
    impl_trait_accessible!(dyn Asset; MismatchedMesh);

    /// Two copies of one shared asset type reach one column: what one stores,
    /// the other finds by handle, name and iteration, and can add to and
    /// remove from.
    #[test]
    fn copies_of_a_shared_asset_type_share_one_column() {
        let mut assets = AssetManager::new();
        let written = assets
            .add_named(
                "cube",
                WriterMesh {
                    vertices: vec![1, 2, 3],
                },
            )
            .expect("a fresh name");

        let as_reader = Handle::<ReaderMesh>::from_raw(written.index(), written.generation());
        assert_eq!(
            assets.get(as_reader).map(|mesh| mesh.vertices.clone()),
            Some(vec![1, 2, 3])
        );
        assert_eq!(assets.handle_by_name::<ReaderMesh>("cube"), Some(as_reader));
        assert_eq!(assets.len::<ReaderMesh>(), 1);

        let added = assets.add(ReaderMesh { vertices: vec![4] });
        assert_eq!(assets.len::<WriterMesh>(), 2);
        assert_eq!(
            assets
                .iter::<WriterMesh>()
                .map(|mesh| mesh.vertices.len())
                .sum::<usize>(),
            4
        );

        assert_eq!(
            assets.remove(added).map(|mesh| mesh.vertices),
            Some(vec![4])
        );
        assert_eq!(assets.len::<WriterMesh>(), 1);
    }

    /// A reader that looks before any copy registered sees nothing, rather
    /// than a column of its own.
    #[test]
    fn a_shared_type_nobody_registered_has_no_assets() {
        let assets = AssetManager::new();
        assert_eq!(assets.len::<ReaderMesh>(), 0);
        assert_eq!(assets.handle_by_name::<ReaderMesh>("cube"), None);
    }

    /// Registering a second copy re-points the column's function table and
    /// keeps its rows, as a reloaded module's registration does.
    #[test]
    fn a_later_copy_registers_onto_the_existing_column() {
        let mut assets = AssetManager::new();
        assets.add(WriterMesh { vertices: vec![7] });
        assets.register::<ReaderMesh>();
        assets.rehome();
        assert_eq!(assets.len::<ReaderMesh>(), 1);
    }

    /// A copy whose layout differs is refused when it registers.
    #[test]
    #[should_panic(expected = "does not match the layout of the shared column")]
    fn a_shared_type_with_a_different_layout_is_refused() {
        let mut assets = AssetManager::new();
        assets.add(WriterMesh { vertices: vec![1] });
        assets.register::<MismatchedMesh>();
    }

    /// Unshared types keep their own columns even when they look alike.
    #[test]
    fn unshared_types_keep_separate_columns() {
        let mut assets = AssetManager::new();
        assets.add(Mesh("rock"));
        assert_eq!(assets.len::<Mesh>(), 1);
        assert_eq!(assets.len::<Texture>(), 0);
    }

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
        let handle = assets
            .add_named_with_guid("grass", guid, Texture(1))
            .expect("a fresh name");

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
    /// reachable by handle. Guids rebind where names refuse, because a guid is
    /// the identity a cook step pins an asset to rather than a lookup key a
    /// second load can collide on.
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
        let handle = assets
            .add_named_with_guid("grass", guid, Texture(1))
            .expect("a fresh name");
        assets.remove(handle);

        assert_eq!(assets.name_of(handle), None);
        assert_eq!(assets.guid_of(handle), None);
    }

    /// Mutable lookup by either key reaches the same value.
    #[test]
    fn mutable_lookup_by_name_and_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::new(11);
        let handle = assets
            .add_named_with_guid("grass", guid, Texture(1))
            .expect("a fresh name");

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

    /// The per-slot content counter is what lets a GPU uploader rebuild one
    /// edited asset instead of every asset: only a mutable borrow moves it.
    #[test]
    fn content_version_moves_only_on_a_mutable_borrow() {
        let mut assets = AssetManager::new();
        let handle = assets.add(Texture(1));

        let created = assets.content_version(handle).expect("live handle");
        assert_eq!(
            assets.content_version(handle),
            Some(created),
            "reading leaves the version alone"
        );

        assets.get_mut(handle).expect("live handle").0 = 2;
        assert_eq!(
            assets.content_version(handle),
            Some(created + 1),
            "editing moves it"
        );

        assets.remove(handle);
        assert_eq!(
            assets.content_version(handle),
            None,
            "a stale handle answers nothing"
        );
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
        let handle = assets
            .add_named("rock", Mesh("rock"))
            .expect("a fresh name");

        assert_eq!(assets.handle_by_name::<Mesh>("rock"), Some(handle));
        assert_eq!(assets.get_by_name::<Mesh>("rock"), Some(&Mesh("rock")));

        assets.remove(handle);
        assert_eq!(assets.handle_by_name::<Mesh>("rock"), None);
        assert_eq!(assets.get_by_name::<Mesh>("rock"), None);
    }

    /// A name that is already bound is refused, and the asset holding it is
    /// left exactly as it was - the collision is reported, not swallowed.
    #[test]
    fn a_taken_name_is_refused() {
        let mut assets = AssetManager::new();
        let original = assets
            .add_named("mesh", Mesh("first"))
            .expect("a fresh name");

        let refused = assets.add_named("mesh", Mesh("second"));

        assert!(matches!(
            refused,
            Err(AssetBindingError::NameInUse { ref name }) if name == "mesh"
        ));
        assert_eq!(assets.handle_by_name::<Mesh>("mesh"), Some(original));
        assert_eq!(assets.get(original), Some(&Mesh("first")));
        // The refused asset never entered the column, so nothing leaked either.
        assert_eq!(assets.len::<Mesh>(), 1);
    }

    /// Freeing a slot frees the name with it, so a later load can take it.
    #[test]
    fn a_removed_name_can_be_bound_again() {
        let mut assets = AssetManager::new();
        let first = assets
            .add_named("mesh", Mesh("first"))
            .expect("a fresh name");
        assets.remove(first);

        let second = assets
            .add_named("mesh", Mesh("second"))
            .expect("removal freed the name");

        assert_eq!(assets.handle_by_name::<Mesh>("mesh"), Some(second));
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

    /// The packed column swaps rows on removal; a handle follows its slot, not
    /// the row that used to hold its value.
    #[test]
    fn removing_the_middle_asset_keeps_the_others_addressable() {
        let mut assets = AssetManager::new();
        let a = assets.add(Mesh("a"));
        let b = assets.add(Mesh("b"));
        let c = assets.add(Mesh("c"));

        assert_eq!(assets.remove(b), Some(Mesh("b")));
        assert_eq!(assets.get(a), Some(&Mesh("a")));
        assert_eq!(assets.get(c), Some(&Mesh("c")));

        let d = assets.add(Mesh("d"));
        assert_eq!(d.index(), b.index(), "the freed slot is refilled");
        assert_eq!(assets.get(a), Some(&Mesh("a")));
        assert_eq!(assets.get(c), Some(&Mesh("c")));
        assert_eq!(assets.get(d), Some(&Mesh("d")));
        assert_eq!(assets.len::<Mesh>(), 3);
    }

    /// Declaring a type re-points its column without disturbing its contents.
    #[test]
    fn declaring_a_type_twice_keeps_its_assets() {
        let mut assets = AssetManager::new();
        let handle = assets
            .add_named("rock", Mesh("rock"))
            .expect("a fresh name");

        assets.register::<Mesh>();

        assert_eq!(assets.get(handle), Some(&Mesh("rock")));
        assert_eq!(assets.handle_by_name::<Mesh>("rock"), Some(handle));
        assert_eq!(assets.len::<Mesh>(), 1);
    }

    /// The re-home pass swaps every column's table; values, handles and names
    /// must survive it untouched.
    #[test]
    fn rehoming_leaves_assets_and_handles_intact() {
        let mut assets = AssetManager::new();
        let shared = assets.add(Mesh("shared"));
        let doomed = assets.add(Mesh("doomed"));
        assets.remove(doomed);
        let named = assets
            .add_named("tree", Mesh("tree"))
            .expect("a fresh name");

        assets.rehome();

        assert_eq!(assets.get(shared), Some(&Mesh("shared")));
        assert_eq!(assets.get(named), Some(&Mesh("tree")));
        assert_eq!(assets.handle_by_name::<Mesh>("tree"), Some(named));
        let mut names: Vec<&str> = assets.iter::<Mesh>().map(|mesh| mesh.0).collect();
        names.sort_unstable();
        assert_eq!(names, ["shared", "tree"]);
    }

    /// The column still drops its values through its function table after the
    /// table has been swapped - the property the whole design exists to keep.
    #[test]
    fn dropping_the_manager_drops_its_assets() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Asset for DropProbe {}
        impl_trait_accessible!(dyn Asset; DropProbe);

        let drops = Arc::new(AtomicUsize::new(0));
        {
            let mut assets = AssetManager::new();
            assets.add(DropProbe(Arc::clone(&drops)));
            assets.rehome();
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
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

    /// The world-side API the reload transaction uses: declaring a type
    /// creates the manager on first use, and the re-home pass runs through it.
    #[test]
    fn world_registers_and_rehomes_asset_types() {
        use crate::world::World;

        let mut world = World::new();
        world.register_asset::<Mesh>();
        let handle = world
            .get_resource_mut::<AssetManager>()
            .expect("register_asset creates the manager")
            .add(Mesh("rock"));

        world.rehome_assets();

        assert_eq!(
            world
                .get_resource::<AssetManager>()
                .and_then(|assets| assets.get(handle)),
            Some(&Mesh("rock"))
        );
    }

    #[test]
    fn asset_loader_returns_embedded_bytes() {
        let loader = AssetLoader::Bytes(vec![1, 2, 3].into_boxed_slice());
        assert_eq!(loader.load().unwrap(), [1, 2, 3]);
    }

    #[test]
    fn asset_loader_resolves_paths_below_its_root() {
        let _mounted = crate::asset_store::mounted_directory_test_lock();
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

    /// A guid is written as the 32-digit hex string `Display` gives, and reads
    /// back to the same value.
    #[test]
    fn a_guid_serializes_as_32_hex_digits() {
        let guid = AssetGuid::new(0x0123_4567_89ab_cdef_0011_2233_4455_6677);
        let json = serde_json::to_string(&guid).unwrap();
        assert_eq!(json, "\"0123456789abcdef0011223344556677\"");
        assert_eq!(serde_json::from_str::<AssetGuid>(&json).unwrap(), guid);
        // Leading zeros are kept, so every guid has the same width.
        assert_eq!(
            serde_json::to_string(&AssetGuid::new(1)).unwrap(),
            "\"00000000000000000000000000000001\""
        );
    }

    /// Anything but exactly 32 hex digits is refused rather than read as some
    /// other guid.
    #[test]
    fn malformed_guid_text_is_refused() {
        for text in [
            "",
            "abc",
            "+0123456789abcdef0011223344556677",
            "0123456789abcdef001122334455667g",
        ] {
            assert_eq!(AssetGuid::parse(text), None, "{text:?}");
        }
        assert!(serde_json::from_str::<AssetGuid>("\"xyz\"").is_err());
        assert!(serde_json::from_str::<AssetGuid>("42").is_err());
        assert_eq!(
            AssetGuid::parse("0123456789ABCDEF0011223344556677"),
            Some(AssetGuid::new(0x0123_4567_89ab_cdef_0011_2233_4455_6677))
        );
    }

    /// Random guids differ from each other and survive a round trip.
    #[test]
    fn random_guids_are_distinct_and_round_trip() {
        let first = AssetGuid::random().expect("a random source");
        let second = AssetGuid::random().expect("a random source");
        assert_ne!(first, second);
        let json = serde_json::to_string(&first).unwrap();
        assert_eq!(serde_json::from_str::<AssetGuid>(&json).unwrap(), first);
    }
}
