# Component Serialization & Schema Migration

How the ECS engine preserves entity state across hot-reloads, including when
component struct definitions change.

---

## 1. Serialization 101

### What is serialization?

Serialization is the process of converting an in-memory data structure into a
format that can be stored or transmitted, then later reconstructed. The
reverse process is **deserialization**.

```
In-memory struct                Serialized form               In-memory struct
┌─────────────────┐            ┌──────────────┐            ┌─────────────────┐
│ Position {       │  ──→      │ {"x":1.0,    │  ──→      │ Position {       │
│   x: 1.0,        │  serialize │  "y":2.5}    │ deserialize│   x: 1.0,        │
│   y: 2.5,        │            └──────────────┘            │   y: 2.5,        │
│ }                │                                        │ }                │
└─────────────────┘                                        └─────────────────┘
```

### Why does hot-reload need it?

When the game DLL is recompiled and reloaded:

1. The old DLL is unloaded — all its heap allocations, vtables, and function
   pointers become invalid.
2. The entity component data lives in the engine's memory (the standalone host),
   but the _types_ that describe that data (struct layouts, field offsets) were
   defined in the old DLL.
3. After the new DLL loads, the old byte layouts may not match the new struct
   definitions.

Serialization bridges this gap: before the old DLL disappears, we convert every
component into a type-name-keyed byte blob. After the new DLL registers its
types, we reconstruct the components from those blobs — matching old data to
new types by **name**, not by memory layout.

### Serialization formats

| Format | Self-describing? | Schema changes | Speed |
|--------|:-----------------:|----------------|-------|
| **bincode** (binary) | No | ❌ Adding a field shifts all bytes, old data unreadable | Fast |
| **JSON** | Yes | ✅ Fields matched by name, missing→default, unknown→ignored | Slower |
| **MessagePack** | Yes | ✅ Same as JSON, more compact | Medium |

We use **JSON** because it is self-describing: field names are embedded in the
payload. Adding `z: f32` to a struct does not break reading old `{"x":1,"y":2}`
data — serde fills the missing `z` from `Default::default()`.

---

## 2. Serde 101

[Serde](https://serde.rs) is Rust's standard serialization framework. It
separates _data structure definitions_ from _data format implementations_.

### The two key traits

```rust
pub trait Serialize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error>;
}

pub trait Deserialize<'de>: Sized {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error>;
}
```

You implement neither — you **derive** them:

```rust
#[derive(Serialize, Deserialize)]
struct Position {
    x: f32,
    y: f32,
}
```

Serde's derive macro generates code that iterates over every field, calls the
format-specific `Serializer`/`Deserializer` methods, and handles the
name-based matching automatically.

### Format crates

Serde itself is format-agnostic. You pick a format crate:

| Crate | Format | `to_vec` / `from_slice` |
|-------|--------|--------------------------|
| `serde_json` | JSON | `serde_json::to_vec(&val)` / `serde_json::from_slice(&bytes)` |
| `bincode` | Binary | `bincode::serialize(&val)` / `bincode::deserialize(&bytes)` |
| `rmp-serde` | MessagePack | `rmp_serde::to_vec(&val)` / `rmp_serde::from_slice(&bytes)` |

### Schema evolution with serde

Serde's derive macro supports attributes that control schema evolution:

```rust
#[derive(Serialize, Deserialize)]
struct FrameCounter {
    count: u64,
    #[serde(default)]              // New field → fills from Default::default()
    label: String,
    #[serde(skip)]                 // Old field → skipped during serialization
    _deprecated: bool,
}
```

By default (without `#[serde(deny_unknown_fields)]`), serde **ignores**
unknown JSON keys. This means old data with removed fields deserializes cleanly.

### The limitation

`serde_json::from_slice::<T>()` fails if the JSON is missing a field that `T`
requires and that field lacks `#[serde(default)]`. Our engine works around this
by pre-merging `T::default()` into the JSON before deserializing (see §3).

---

## 3. Implementation in This Project

### Overview

The persistence system lives in `engine/src/persistence.rs`. It has three
phases:

```
┌─ BEFORE HOT-RELOAD ──────────────────────────────────────┐
│  snapshot = world.snapshot_components()                  │
│  → walks every archetype                                 │
│  → for every entity, for every persistable component:    │
│    calls serialize_fn(storage, index) → JSON bytes       │
│  → stores (type_name, bytes) pairs in ComponentSnapshot  │
│                                                          │
│  engine.clear_systems()  // old DLL still loaded ✅       │
├──────────────────────────────────────────────────────────┤
│  new_library.call_game_init(&engine_api)                 │
│  → register_persistable_component::<FrameCounter>()      │
│  → replaces storage_factories, persist function pointers │
│                                                          │
│  world.restore_from_snapshot(&snapshot)                  │
│  → destroys all entities (stale TypeIds)                 │
│  → for each snapshot entry:                              │
│    1. match type_name → deserialize_fn (by name)         │
│    2. deserialize_fn(bytes) → merge with Default JSON    │
│    3. insert_boxed into new archetype storage            │
└──────────────────────────────────────────────────────────┘
```

### Component registration

Game components opt in to persistence by calling
`register_persistable_component` instead of `register_component`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FrameCounter {
    count: u64,
}
impl Component for FrameCounter {}

// In game_init:
engine.world_mut().register_persistable_component::<FrameCounter>();
```

This stores three monomorphized function pointers in the engine's `World`:

| Function | Stored in | Key | What it does |
|----------|-----------|-----|-------------|
| `serialize_component::<T>` | `persist_serializers` | `ComponentId` | Reads `T` from archetype column, calls `serde_json::to_vec` |
| `deserialize_component::<T>` | `persist_deserializers` | type **name** string | Parses JSON, merges with default, calls `serde_json::from_value` |
| `insert_boxed_component::<T>` | `persist_inserters` | `ComponentId` | Downcasts `Box<dyn Component>` → `Box<T>`, pushes into `VecStorage<T>` |

> **Why type name, not TypeId?**
>
> `TypeId` is a compiler-generated hash of the type's identity. Adding a field
> to `FrameCounter` produces a different `TypeId`. If we keyed by `TypeId`, old
> data could never match new types. By keying by type _name_ (the string
> `"game::FrameCounter"`), we match old data to the new type definition
> regardless of `TypeId` changes.

### JSON default-merge

The core of schema migration is `deserialize_component` and `merge_json`:

```
Old snapshot JSON:    {"count": 42}
New struct default:   {"count": 0, "migrated": false}
                        │
                        ▼  merge_json(default, snapshot)
                        │  → snapshot values override defaults
                        │  → defaults fill missing keys
                        ▼
Merged:               {"count": 42, "migrated": false}
                        │
                        ▼  serde_json::from_value::<T>(merged)
                        │
Result:               FrameCounter { count: 42, migrated: false }  ✅
```

The `merge_json` function recursively merges nested objects. For flat structs
(which most components are), it simply fills missing keys from the default.

### Handling schema change scenarios

| Change | What happens | Result |
|--------|-------------|--------|
| **Add a field** | Snapshot is missing the key → default fills it | `migrated: false` (from `Default`) |
| **Remove a field** | Snapshot has the key, new struct doesn't → serde ignores unknown keys | Old field silently dropped |
| **Change field type** | e.g. `bool` → `u32` — JSON `true` cannot deserialize into `u32` | `deserialize_component` returns `None`, component skipped with warning |
| **No change** | JSON matches struct exactly | Direct deserialization, no merge needed |

### Stale-entry cleanup

When a component struct is changed and then changed back:

```
Cycle 1:  FrameCounter { count: u64 }            → TypeId = 0xA
Cycle 2:  FrameCounter { count: u64, migrated }  → TypeId = 0xB
Cycle 3:  FrameCounter { count: u64 }            → TypeId = 0xA  (same as cycle 1!)
```

The compiler may assign the **same TypeId** to cycle 1 and cycle 3 because the
struct is identical. Without cleanup, `persist_inserters` would still hold the
cycle 2 entry (TypeId 0xB), and the query system could resolve components to
the wrong TypeId.

`register_persistable_component` handles this by removing stale entries for
the same type name before inserting new ones.

### Why function pointers, not trait objects?

The serialize/deserialize/insert functions are stored as plain `fn` pointers:

```rust
pub(crate) type SerializeComponentFn =
    fn(storage: &TraitTypeMap<dyn Component, VecFamily>, index: usize) -> Vec<u8>;
```

They are **not** `Box<dyn Fn(...)>` trait objects. This matters because:

1. **Function pointers have no destructor.** Overwriting an entry in the
   `HashMap` replaces the pointer — no vtable call, no DLL-unload issue.
2. **They are monomorphized in the game DLL.** `serialize_component::<FrameCounter>`
   is compiled into the game DLL's code section. The engine only stores the
   address.
3. **The old DLL stays loaded** (graveyard pattern). Even after a hot-reload,
   old function pointers are never called, but if they were, the old DLL is
   still mapped.

### The five-step reload sequence

In `standalone/src/main.rs`, the hot-reload follows a strict order:

| Step | Action | Why this order |
|:----:|--------|---------------|
| 1 | `engine.world().snapshot_components()` | Serialize data using old DLL's persist functions (old DLL loaded ✅) |
| 2 | `engine.clear_systems()` | Drop old `Box<dyn System>` objects (old DLL loaded ✅, vtables valid) |
| 3 | `new_library.call_game_init()` | New DLL registers components + persist functions; old storage factories/copiers replaced (old DLL loaded ✅ during drop) |
| 4 | `engine.world_mut().restore_from_snapshot()` | Destroy stale-TypeId entities, recreate from snapshot using new persist functions |
| 5 | `old_libraries.push(old_library)` | Archive old DLL handle (never unloaded — its code may still be referenced) |

---

## 4. How to Add a New Persistable Component

1. Derive the required traits on your component:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MyComponent {
    value: f32,
}
impl Component for MyComponent {}
```

2. Add it to `impl_trait_accessible!`:

```rust
impl_trait_accessible!(dyn Component; MyComponent, /* other types */);
```

3. Register it in `game_init`:

```rust
engine.world_mut().register_persistable_component::<MyComponent>();
```

That's it. The component will automatically survive hot-reloads, and field
additions/removals will be handled by the JSON default-merge.

### Requirements

| Trait | Why |
|-------|-----|
| `Serialize` | To convert the component to JSON during snapshot |
| `DeserializeOwned` | To reconstruct the component from JSON during restore |
| `Default` | To fill missing fields when the schema expands |
| `Clone` | Required by the ECS archetype migration (component copiers) |

---

## 5. Limitations

- **Field type changes are not migrated.** Changing `bool` to `u32` causes
  deserialization to fail; the component is skipped. The entity is still
  restored with its other components intact.
- **Tuple struct fields are matched by position, not name.** `Health(f32)` and
  `Health(f64)` cannot be distinguished by serde — both serialize as a bare
  number. Prefer named-field structs for persistable components.
- **The old DLL is never unloaded** (graveyard pattern). Each hot-reload leaks
  ~2 MB of virtual address space. For development use this is negligible; for
  production, consider a periodic full restart.
- **`Default` is required.** Components without a sensible default (e.g., those
  requiring initialization with specific values) should use manual
  `Default` impl or `#[serde(default = "fn_name")]`.
