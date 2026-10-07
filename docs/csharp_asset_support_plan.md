# C# Asset Types: Implementation Plan

Let a C# project declare its own asset types and have them behave exactly like
Rust-declared assets:
- many values per type in the engine's `AssetManager`, addressed by handle,
  name and GUID;
- imported from source files with `.meta` sidecars, or stored as standalone
  files in `res`;
- able to reference other assets by GUID, survive hot reload, and appear in
  the editor.

Along the way, components, resources and assets end up on one storage
type, and C#-declared types of all three kinds gain variable-length fields.

> **Already in place** from [csharp_rust_asset_api_plan.md](csharp_rust_asset_api_plan.md):
> the C# `Handle<T>`, the `AssetManager` marker with `Res<AssetManager>` /
> `ResMut<AssetManager>` access (ordered by the scheduler against Rust
> users), `AssetManagerExtensions` (`Add`, `AddNamed`, `Find`, `Remove`,
> `Import`, `ImportStandalone`, ...), startup methods taking `Res`/`ResMut`
> parameters, and `Resource::shared_name` on `AssetManager`
> (`pill_engine::asset::AssetManager`). Those extension methods currently
> require a `RustObject` asset type; Stage 6 below adds C#-declared asset
> types beside them. The first two open questions below are settled.

## Decisions

| Topic | Decision |
| --- | --- |
| Storage for every asset column, Rust or C# | `ComponentColumn`, renamed `ErasedColumn` |
| Column key | `AssetTypeKey`: `Native(TypeId)` for unshared Rust types, `Identity(u128)` for shared Rust types and C# types |
| `TraitAccessible<dyn Asset>` bounds and the `trait_type_map` dependency | Kept for now; removed in a separate cleanup after this plan |
| Foreign resources | Move onto `ErasedColumn` (one-row column), right after the asset column swap |
| Bridge binding record | One `ForeignBinding` / `ForeignFieldLayout` shared by resources and assets |
| Scheduler access | Whole `AssetManager` resource; no per-asset-type access |
| C# access API | `Res<AssetManager>` / `ResMut<AssetManager>` with typed methods (`Get<T>`, `Add<T>`, `Remove<T>`, `Find<T>`) |
| Variable-length data | Engine-owned `DynamicBuffer`-backed fields, for C# components, resources and assets alike |
| Imported assets | A C# asset type declares source extensions, a settings struct and an import method; the engine calls it, writing settings into `.meta` like Rust |
| References | C# `AssetReference<T>` field type: a GUID that resolves to a handle |
| Later scope | Standalone files, `.meta` imports, variable-length data, sharing a type with Rust, and editor inspection are all scheduled |

## Background

- **Components:** C# declares them; they are stored as raw bytes in a
  `ComponentColumn` built from a `ColumnLayout`
  (`StorageFactory::Descriptor`).
- **Resources:** C# declares them (`[EcsResource]`, `Res<T>` / `ResMut<T>`);
  they are stored as raw bytes in an `ErasedResource::new_foreign` box.
- **Assets:** C# cannot declare them. Only fixed renderer and audio entry
  points exist (`Assets.Import<T>`, `LoadMeshObj`, ...). `AssetManager`
  stores each type in a `trait_type_map::ErasedVecStorage<dyn Asset>`, keyed
  by the Rust `TypeId`, and every method is generic over a Rust `T`.

`ComponentColumn` (`pill_engine_core/src/archetype.rs`) already serves both
Rust and byte rows:

| Need | API |
| --- | --- |
| Rust element types, including ones that need dropping | `new_native::<T>`, `push<T>`, `get<T>`, `get_mut<T>`, `swap_remove<T>`, `iter<T>`, `refresh_ops(ColumnOps::of::<T>())` |
| Byte rows | `new(ColumnLayout)`, `push_bytes`, `set_bytes`, `bytes`, `row_ptr`, `swap_remove_discard` |
| Hot-reload layout change | `relayout(ColumnLayout, &FieldPlan)` |
| Types compiled into several binaries | `ColumnIdentity::Shared`, checked by layout |

`AssetManager` only calls `push`, `get`, `get_mut`, `swap_remove`, `len` and
`iter` on its columns and never uses the `dyn Asset` conversion. Component
storage made the same move from `ErasedVecStorage` to `ComponentColumn`
earlier (see the comment at `archetype.rs:696`).

On the bridge side there is already:
- one manifest array tagged by `kind`;
- one generic reload pipeline (`manifest_apply::ManifestSubject`) that handles
  aliases, renames, layout changes, retirement and rollback.

## Stage overview

| Phase | Stage | Outcome |
| --- | --- | --- |
| A. One storage type | 1. Asset columns on `ComponentColumn` | No behaviour change |
| | 2. Rename to `ErasedColumn` | Mechanical |
| | 3. Foreign resources on `ErasedColumn` | `ErasedResource::new_foreign` retired |
| B. C# assets in memory | 4. Foreign asset types in `AssetManager` | Untyped core API |
| | 5. Bridge support | Manifest kind, access, native calls |
| | 6. C# API | `[EcsAsset]`, `Res<AssetManager>` methods |
| | 7. Example and hot-reload coverage | First usable release |
| C. Richer data | 8. Variable-length fields | `DynamicBuffer` fields in C# types |
| | 9. Field-layout serialization and `AssetReference<T>` | Bytes to JSON and back |
| D. Files | 10. Standalone C# asset files | Like `.material` |
| | 11. Imported C# assets with `.meta` | C# importer callback |
| E. Interop and tools | 12. Share an asset type with Rust | One column, two languages |
| | 13. Editor inspection | Asset rows visible and editable |

Each stage lands separately and leaves the tree working.

---

## Phase A: One storage type

### Stage 1: `AssetManager` on `ComponentColumn`

**Goal:** replace the column type and nothing else.

**Changes in `pill_engine_core/src/asset.rs`:**
- Replace `columns: TraitTypeMap<dyn Asset, ErasedVecFamily>` with
  `columns: HashMap<AssetTypeKey, ComponentColumn>`.
- Add `AssetTypeKey { Native(TypeId), Identity(u128) }`.
  - Shared Rust types (`Asset::shared_identity()` is `Some`) are keyed by
    `Identity` directly.
  - Remove `shared_keys`.
  - Key `metadata` and `ops_refreshers` by `AssetTypeKey` too.
- `register::<T>`: build with
  `ComponentColumn::new_native::<T>(0, T::shared_identity().is_some())`;
  keep the layout check against `AssetColumn::element_layout`.
- `refresh_column_ops::<T>`: `column.refresh_ops(ColumnOps::of::<T>())`.
- Rewrite `add`, `get`, `get_mut`, `remove`, `len` and `iter` against the
  typed `ComponentColumn` methods. `remove` already relies on swap-remove
  semantics through `row_slots`.
- Per-row change ticks in `ComponentColumn` stay unused for now.
- Check that `ComponentColumn`'s panic messages read sensibly from the asset
  path.

**Tests:**
- Every existing test in `asset*.rs` and
  `world/tests.rs::world_registers_and_rehomes_asset_types` passes unchanged.
- `devops/tests/test_hot_reload_assets.py` and `test_renderer_assets.py`
  pass; they exercise rehoming across a real module reload.
- Run `circus_demo` and `master_renderer_test` once.

**Done when:** no public signature changed and every asset test passes.

### Stage 2: Rename `ComponentColumn` to `ErasedColumn`

- Do this as one mechanical commit: the type, its docs, its panic messages,
  and `ComponentColumns` (keep that name if it still only means "an
  archetype's columns").
- Nothing else changes in this commit.

### Stage 3: Foreign resources on `ErasedColumn`

**Goal:** components, resources and assets share one byte-storage
implementation.

**Changes:**
- Store a foreign resource as a one-row `ErasedColumn::new(ColumnLayout)` in
  place of `ErasedResource::new_foreign`. Its change ticks can come from the
  column's row tick, replacing the separate `resource_ticks` entry for
  foreign resources (decide while implementing; keep the separate entry if
  merging complicates `get_resource_mut_tracked`).
- Route `register_foreign_resource`, `insert_foreign_resource_bytes`,
  `foreign_resource_bytes(_mut)`, `foreign_resource_layout` and
  `relayout_foreign_resource` (`world/resources.rs`) through it.
  `relayout_foreign_resource` uses `ErasedColumn::relayout`.
- Remove `ErasedResource::new_foreign`, `ErasedResourceOps::foreign` and the
  `foreign` flag once nothing uses them.
- **Optional:** if it simplifies code, move native resources onto one-row
  columns too, so `ErasedResource` disappears. Decide after the foreign
  move; it is not needed for any later stage.

**Tests:**
- All foreign-resource tests in `world/tests.rs` (e.g.
  `foreign_bytes_round_trip_and_stamp_the_change_tick`) and
  `pill_engine/tests/shared_resource_identity.rs`.
- Bridge resource tests in `pill_csharp_bridge/src/tests.rs`.
- `project_cs` hot reload with `SimulationTime`.

---

## Phase B: C# assets in memory

### Stage 4: Foreign asset types in `AssetManager`

**Goal:** an untyped API keyed by a 128-bit identity, on the same columns
and bookkeeping as Rust types.

**Changes in `pill_engine_core/src/asset.rs`:**
- `register_foreign(identity, name, layout: ColumnLayout) -> Result<(), WorldError>`
  - Idempotent for an identical layout. Refuses a different one; that goes
    through `relayout_foreign`.
  - Refuses an identity a Rust type already holds. Stage 12 relaxes this.
  - Creates `ErasedColumn::new(layout)` and an `AssetColumn`, with no ops
    refresher, since a byte column has no code to re-point.
- `add_bytes`, `add_named_bytes` and `add_named_bytes_with_guid`.
  - Factor the slot, name and GUID logic into private helpers taking an
    `AssetTypeKey` and a "push one row" closure, so typed and untyped
    paths share it.
- `bytes` / `bytes_mut` and `row_ptr(identity, handle, write)`.
  - Mutable access bumps `revision` and the slot's `content_version`, as
    `get_mut` does.
- `remove_bytes` (via `swap_remove_discard`), `handle_by_name_raw`,
  `handle_by_guid_raw`, `len_raw` and `iter_handles_raw`.
- `relayout_foreign(identity, layout, &FieldPlan)`: rows keep their order, so
  slots, names and GUIDs are untouched.
- `rename_foreign_type(old, new)` and `retire_foreign(identity)`.
- `RawHandle { index, generation }`, or reuse `Handle::from_raw` /
  `index` / `generation`.
- The typed API refuses a foreign column (the `ColumnIdentity` check), and
  the raw API refuses a native column whose rows are not plain data.

**Changes in `world/resources.rs`:** `World::register_foreign_asset`,
`relayout_foreign_asset`, `rename_foreign_asset_type` and
`retire_foreign_asset`. Each inserts `AssetManager` when it is missing, as
`register_asset` does.

**Tests (`asset.rs`):**
- Round trip: add, read, mutate and remove; a stale handle resolves to `None`.
- Name and GUID lookup on a foreign type.
- Layout mismatch is refused; one identity cannot be claimed by both a Rust
  type and a foreign registration.
- `relayout_foreign` keeps handles and moves field values per the plan.
- `revision` and `content_version` move on mutable access only.
- `rehome` leaves foreign columns untouched.

### Stage 5: Bridge support

**Manifest (`pill_csharp_bridge/src/manifest.rs`):**
- Add `ManifestEntryKind::Asset` (serde tag `"asset"`).
- Give `split_manifest_kinds` a third output.
- Refuse `shared: true` on asset entries until Stage 12.

**Shared binding (`resources.rs` into a new `foreign.rs`):**
- Generalise `ResourceBinding` and `ResourceFieldLayout` into
  `ForeignBinding` / `ForeignFieldLayout`, with a kind-specific id
  (`ResourceId` or the asset identity).
- Rename `resource_field_plan` to `foreign_field_plan`.
- Resources move onto the shared record in the same commit; no behaviour
  change for them.

**Asset subject (new `csharp_assets.rs` in the bridge; the existing `assets.rs` stays the renderer forwarding layer):**
- `ASSET_BINDINGS`, published like `RESOURCE_BINDINGS` and thread-local under
  `cfg(test)`.
- `ASSET_SUBJECT: ManifestSubject<...>`, modelled on `RESOURCE_SUBJECT`.
  - register: `register_foreign_asset`
  - reshape: `relayout_foreign_asset`
  - rename: `rename_foreign_asset_type`
  - retire: `retire_foreign_asset`
  - undo: like `undo_resource_entry`; a relayout journals no undo
- Run from cold-start registration and from the reload path, beside the
  resource subject, in the same transaction.

**Scheduler access via `AssetManager`:**
- The managed runtime gives `AssetManager` one well-known stable identity.
  It is not in the manifest, because the host already owns the resource.
- `resolve_resource_access` (`resources.rs`, called from
  `backend.rs:1531`) maps that identity to the native
  `ResourceId` of `AssetManager`. Rust systems taking `Res/ResMut<AssetManager>`
  then conflict with C# ones automatically.
- `ffi_get_resource_view` refuses the `AssetManager` identity with a
  dedicated status ("not a byte resource"), so `.Value` cannot expose
  Rust memory.

**Native calls (`abi.rs`, appended to the end of `CsEngineApi`):**
- `asset_add(id_lo, id_hi, bytes, len, name, name_len, out_index, out_generation) -> u8`
- `asset_get_view(id_lo, id_hi, index, generation, mode, out: *mut NativeAssetView) -> u8`
  - `NativeAssetView { data, length, scope_token }`.
- `asset_remove(id_lo, id_hi, index, generation) -> u8`
- `asset_find(id_lo, id_hi, by /* name | guid */, key, key_len, out_index, out_generation) -> u8`
- `asset_count(id_lo, id_hi, out: *mut u32) -> u8`
- Every call checks that the running system declared access to
  `AssetManager`: read for `get_view` (read mode), `find` and `count`;
  write for the rest.
- Status codes:
  - 0: ok
  - 1: unregistered type
  - 2: access not declared
  - 3: no active scope
  - 4: stale handle
  - 5: null output
  - 6: name in use
- Update the table-size or ABI check in `LoaderInterop.cs`.

**Pointer lifetime:** a removal swaps rows, so a view is valid only until the
next add or remove of that type. Adds and removes require
`ResMut<AssetManager>`, which the scheduler serialises against every reader.

**Tests (`pill_csharp_bridge/src/tests.rs`):**
- Registration is idempotent.
- On reload: relayout migrates rows, a dropped entry retires the column, and
  an alias moves it.
- A failing component entry in the same manifest leaves assets untouched.
- Each status code, including undeclared access and stale handles.

### Stage 6: C# API

**Declaration (`pill_csharp_runtime/src/Assets.cs`):**
- `[EcsAsset]` (optional explicit name) and `[EcsAssetAlias(oldName)]`.
- `ManifestKinds.Asset = "asset"`. `ProjectManifest.cs` discovers assets by
  attribute and excludes them from component discovery. `Describe` and
  `DeclaredAliases` take the kind.

**Shared metadata (the C# "base"):**
- One `ForeignTypeMetadata<T>`, keyed by kind, that resolves the name, stable
  id and size checked against `NativeLayout`. It replaces
  `ResourceTypeMetadata<T>` and the identity part of
  `ComponentTypeMetadata<T>`; hot-path fields stay put if moving them costs
  anything in the query loop.
- `ResourceNames.Of` becomes `ForeignNames.Of(type, kind)`.

**Access:**
- `public readonly struct AssetManager` marker type with the well-known
  identity from Stage 5.
- Extension methods on `Res<AssetManager>`:
  - `ref readonly T Get<T>(AssetHandle<T>)`
  - `bool TryGet<T>(AssetHandle<T>, out ...)`
  - `AssetHandle<T>? Find<T>(string name)`
  - `int Count<T>()`
- Extension methods on `ResMut<AssetManager>`: everything above, plus
  - `ref T GetMut<T>(AssetHandle<T>)`
  - `AssetHandle<T> Add<T>(in T)` and `Add<T>(string name, in T)`
  - `bool Remove<T>(AssetHandle<T>)`
- `T` is constrained to `unmanaged` and checked against `[EcsAsset]`. Every
  call re-fetches the view, checks the scope token, and checks
  `length == size`, like `ResourceAccess.Borrow`.
- `AssetHandle<T>`: typed and blittable `(uint Index, uint Generation)`,
  storable in components, resources and assets, and convertible to the
  untyped `AssetHandle`.

**Startups:**
- Startups take `ResMut<AssetManager>` as a parameter, like systems.
  Extend startup parameter binding if it only accepts `Commands` today.
- Move `Assets.Import<T>` onto `ResMut<AssetManager>` as well, keeping the
  static as an obsolete forwarder for one release.

**Discovery, AOT and analyzer:**
- `ProjectHost.cs` already turns `IResourceParameter` into accesses; check
  that `Res<AssetManager>` flows through unchanged.
- `macros/EcsAotRegistryGenerator.cs` emits the same access for the
  NativeAOT build.
- `analyzers/PillSystemAnalyzer.cs`: flag `.Value` on `Res<AssetManager>`
  and `Get<T>` / `Add<T>` over a type without `[EcsAsset]`.

**Tests:**
- `pill_csharp_runtime/tests/Program.cs`: manifest output and aliases.
- `test_csharp_bridge.py` and `test_csharp_analyzer.py`.

### Stage 7: Example and hot-reload coverage

- Add an `[EcsAsset]` type to `examples/project_cs` (e.g. ball profiles):
  - a startup adds named instances;
  - one system reads them by handle each frame;
  - another mutates one.
- Hot-reload tests:
  - Add a field while running: other values survive and the new field gets
    its default.
  - Rename the type with `[EcsAssetAlias]`: handles held in components still
    resolve.
- `test_shipping_smoke.py` for the NativeAOT posture.

---

## Phase C: Richer data

### Stage 8: Variable-length fields

**Goal:** C# components, resources and assets can hold strings and lists in
engine-owned memory.

**Basis:** `pill_core::dynamic_buffer::DynamicBuffer<T>` is already
engine-owned, reference-counted, address-stable and reload-safe, and C# reads
it through `Engine.OverDynamicBuffer<T>`.

**Changes:**
- **Field types:** add C# field types `NativeList<T>` (where `T` is
  `unmanaged`) and `NativeString` (UTF-8), laid out as the
  `DynamicBuffer` `#[repr(C)]` handle. Add matching manifest field tags.
- **Manifest validation:** accept these tags. A type containing one is no
  longer `Blittability::from_manifest_fields()`; give it a "has buffers"
  layout instead.
- **Column ops:** a `ColumnOps` built from the field layout, whose
  `drop_range` releases each buffer field. Assets and resources get it
  through `ErasedColumn`; components through `StorageFactory::Descriptor`.
- **Copies:** row copies that duplicate a value (e.g. `add_bytes` from a
  managed struct, `Commands.Add`) must retain each buffer. Moves stay
  bitwise.
- **Hot reload:** `FieldPlan` treats a buffer field as one opaque handle; a
  retyped buffer field is released and reset.
- **C# API:** `NativeList<T>` exposes `Span<T>`, `Add`, `Clear` and
  `Count`, calling into the engine's allocation service. `NativeString`
  converts to and from `string`.

**Tests:**
- Buffers are released when a row is removed, an entity despawned, an asset
  removed, or a type retired.
- No leak across a reload (`pill_engine/tests/dynamic_buffers.rs` and
  `heap_component_fields.rs` are the patterns to follow).
- A component, a resource and an asset each round-trip a list and a string.

### Stage 9: Field-layout serialization and `AssetReference<T>`

**Goal:** turn any C#-declared value into JSON and back from its registered
field layout. This is the foundation for files and the editor.

**Changes:**
- In the engine (`pill_engine_core`, beside `world/reflection.rs`), add
  `bytes_to_json(layout, &[u8])` and `json_to_bytes(layout, &Value)`.
  - Primitives, nested value types, `NativeList`, `NativeString` and asset
    references.
  - Missing fields take their declared default; unknown fields are ignored.
- Add C# `AssetReference<T>`: a blittable 16-byte GUID field
  - serialized as 32 hex digits, matching Rust's `AssetReference`;
  - resolved with `Res<AssetManager>.Resolve(reference)`, which returns the
    live handle or invalid, and reports a missing GUID once, as Rust does.
- Register the `AssetReference` field tag in the manifest.

**Tests:**
- Round trip for every field type.
- A file written before a field existed still reads.
- A C# reference and a Rust reference to the same GUID serialize
  identically.

---

## Phase D: Files

### Stage 10: Standalone C# asset files

**Goal:** an `[EcsAsset(Extension = "ballprofile")]` type loads from and
saves to `res/**/*.ballprofile` exactly like `.material`. The file has the
same header (format version, asset type, GUID), with the body under `asset`.

**Changes:**
- **Engine:** factor the generic parts of `AssetManager::import_standalone`,
  its reload and move-following, and `render_standalone`
  (`asset_standalone.rs`) out of `T: StandaloneAsset`, so a foreign entry
  can supply:
  - the type name;
  - the extension;
  - a "document to bytes" step, which is Stage 9's `json_to_bytes`;
  - a "default document" step, which is the declared field defaults.
- **Import registry:** add an erased `ImportFunctions` variant for foreign
  standalone types, keyed by asset identity. Its liveness is tied to the
  managed generation (a `Weak<()>` owned by the bridge per generation),
  so a reload that drops the type unregisters its extension.
- **Bridge:** register the extension when the manifest declares one; the
  manifest entry gains `extension`.
- **C#:** `[EcsAsset(Extension = ...)]`. The existing scan, watcher, move
  following and `test_asset_metadata.py` paths then apply unchanged.

**Tests:**
- Load, edit on disk (watcher reload keeps the handle), move (GUID
  follows), create a new file with defaults.
- Packed shipping build reads the file.

### Stage 11: Imported C# assets with `.meta`

**Goal:** a C# type built from a source file (e.g. `.csv`, `.json`, a custom
binary) with import settings in a `.meta` sidecar, identical to a Rust
`ImportedAsset`.

**C# declaration:**
```csharp
[EcsAsset(SourceExtensions = new[] { "csv" })]
public struct WaypointTable : IImportedAsset<WaypointTable, WaypointTableSettings>
{
    public NativeList<Waypoint> Points;
    public static WaypointTable Import(string name, ReadOnlySpan<byte> source, in WaypointTableSettings settings) { ... }
}
```
- The settings struct is a C# value type described in the manifest like any
  other, and serialized into `.meta` with Stage 9.

**Changes:**
- **Engine:** factor `AssetManager::import`, `reimport` and the metadata
  read/write in `asset_metadata.rs` out of `T: ImportedAsset`, so a foreign
  entry supplies:
  - the type name and extensions;
  - the settings layout, with defaults for a missing `.meta`;
  - an "import" callback: name + source bytes + settings bytes, returning
    asset bytes or an error string.
- **Import registry:** a foreign `ImportFunctions` variant, as in Stage 10.
- **Bridge:** an exported managed entry point the host calls to run a
  type's `Import`; it is generated per type by `EcsAotRegistryGenerator`
  for NativeAOT. It runs in an exclusive-world scope like a startup.
- **Hot reload:** a reload that changes `Import` code does not re-import
  automatically; editing the source file or `.meta` does, through the
  watcher, as for Rust.

**Tests:**
- First import writes `.meta` under `CreateIfMissing`.
- A settings edit in `.meta` reimports.
- An import error is reported with the C# exception message.
- A shipping build imports from the pack.

---

## Phase E: Interop and tools

### Stage 12: Share an asset type with Rust

**Goal:** a C# `[EcsAsset("my_mod::Waypoint")]` and a Rust `Waypoint` with
`shared_name() = "my_mod::Waypoint"` land in one column.

**Changes:**
- **Engine:** `register_foreign` accepts an identity a Rust type holds when
  the Rust column is plain data and the field layout matches.
  `register::<T>` accepts an identity a foreign registration holds under
  the same check. This needs the Rust type's field layout, which the
  component derive already produces for shared components; extend it to
  assets.
- **Bridge:** allow `shared: true` on asset entries and check them against
  the Rust field signature, as `ComponentBinding::ModuleNative` does.
- **Codegen:** optionally generate C# mirrors of shared Rust asset types
  into the extension's `generated/*.g.cs`, as for components.

**Tests:** both sides add and read the same assets; a layout mismatch is
refused with a clear message.

### Stage 13: Editor inspection

**Goal:** the editor lists C# asset types and shows and edits their rows,
like Rust assets.

**Changes:**
- Expose `AssetManager` type listing, rows and names/GUIDs for foreign
  columns through the same reflection the inspector uses for descriptor
  components (`world/reflection.rs`).
- Edit through Stage 9's JSON path. A write bumps `content_version`.
- For standalone files, "save" writes the file back with
  `render_standalone`.

---

## Cleanup after the plan

- Remove the `TraitAccessible<dyn Asset>` bounds from the public asset API
  (`asset.rs`, `asset_reference.rs`, `asset_metadata.rs`,
  `asset_standalone.rs`, `asset_import_registry.rs`) and the
  `impl_trait_accessible!` calls in extension crates. Then drop the
  `trait_type_map` dependency from `pill_engine_core`.

## Open questions

- `AssetManager` implements `Resource` with no `shared_name`, so its
  `ResourceId` is `Native(TypeId)`. That is only stable if every binary
  reaches the same compiled `pill_engine_core` (the `dylib_engine` setup).
  Consider giving `AssetManager` a `Resource::shared_name` in Stage 5, so
  the C# well-known identity and the native id are derived from one name.
- Startups carry no resource accesses today (`ManagedStartup(Name, Action Run)`
  in `ProjectHost.cs`, and the AOT generator's `AotStartupRegistration`).
  Stage 6 must add parameter binding for startups, or give startups a
  scope-checked static `Assets` that requires the exclusive startup scope.
  Pick one at the start of Stage 6.
- Stage 3: merge foreign resources' change ticks into the column's row tick,
  or keep `resource_ticks`?
- Stage 8: should `NativeList<T>` element types allow nested buffers, or only
  plain data?

## Risks

| Risk | Mitigation |
| --- | --- |
| Stage 1 or 3 changes drop or rehome behaviour across a reload | Each lands alone; hot-reload asset and resource tests run before and after |
| A pointer to an asset row outlives a swap-remove | Views re-fetched per call; adds and removes need `ResMut<AssetManager>` |
| Coarse scheduling: every `ResMut<AssetManager>` serialises against every asset user | Same as Rust today; accepted |
| Buffer fields leak or double-free on copy, move, migration or reload | Stage 8 tests every path; copies retain, moves stay bitwise |
| ABI table drift between host and managed runtime | Append-only slots plus the size check in `LoaderInterop.cs` |
| Development and shipping postures declare different access or importers | AOT generator updated in the same stage; shipping smoke test |
| A C# importer throws or hangs during a scan | Exceptions become import errors; imports run in an exclusive scope with the existing failure reporting |
