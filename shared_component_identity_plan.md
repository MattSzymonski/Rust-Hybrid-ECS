# Shared component identity: one column across binaries, at typed speed

Status: plan, not implemented.
Related: [`old_renderer_implementation_plan.md`](old_renderer_implementation_plan.md) (Phase 4 depends on the outcome here).

---

## 1. The problem

Two binaries that both *name* a concrete component type each get their own
`TypeId` for it, because `TypeId` is a hash over crate name, crate
disambiguator (`-C metadata`) and type path, computed per compilation unit.

`ComponentId::of::<T>()` is `Self::Native(TypeId::of::<T>())`
([component.rs:172](modules/pill_engine/src/component.rs#L172)), so two
`TypeId`s mean two `ComponentId`s, which means
[`World::register_component`](modules/pill_engine/src/world.rs#L520) allocates
**two separate columns** for what the programmer thinks is one type.

Concrete case in this repo: `pill_spline::Spline` is linked by the project DLL
*and* by the `pill_spline` module DLL. Both register it. There are two columns.

### 1.1 What is already handled — reloads

Multiple `TypeId`s for one type name across *hot-reload generations* are real
but **correctly handled**, and it is worth being precise about the mechanism
because it is not the one the code comments suggest.

`register_persistable_component_inner`
([persistence.rs:220-234](modules/pill_engine/src/persistence.rs#L220-L234))
purges every same-name-different-`TypeId` entry from `persist_inserters` and
`persist_serializers` when a new generation registers:

```rust
let stale_ids: Vec<ComponentId> = self
    .component_registry
    .registered_components()
    .filter(|(_, _, name)| *name == type_name)
    .map(|(id, _, _)| id)
    .filter(|id| *id != component_id)
    .collect();
for stale_id in &stale_ids {
    self.persist_serializers.remove(stale_id);
    self.persist_inserters.remove(stale_id);
}
```

By the time
[`resolve_component_id_by_name`](modules/pill_engine/src/persistence.rs#L557)
runs, its `persist_inserters.contains_key` filter has already reduced the
candidate set to exactly one entry. The `max_by_key(bit)` tiebreak on line 562
is therefore **vestigial** — it selects the maximum of a one-element set and
never actually chooses anything.

This matters because the doc comment on line 550-556 claims the highest bit
identifies the most recent registration. That is not reliable:
[`allocate_bit`](modules/pill_engine/src/component.rs#L465-L475) pops from
`free_bits` before advancing `next_bit`, so a later registration can receive a
*lower* bit than an earlier one. Eviction is what guarantees correctness; the
bit index could not.

### 1.2 What is not handled — two binaries at once

When two binaries register the same type name in one session, the eviction in
§1.1 misfires. It cannot distinguish a *concurrent peer* from a *superseded
generation* — both present as "same type name, different `TypeId`" — so
whichever binary registers second evicts the first's inserter.

Consequences today:

- **Steady-state frames are correct.** Each binary queries its own column via
  its own `TypeId`. Typed, fast, correct. The two just cannot see each other's
  entities.
- **Hot reload silently loses data.** The evicted binary's column has no entry
  in `persist_inserters`, so `resolve_component_id_by_name` never returns it
  and its snapshots are dropped during restore. Entities come back missing the
  component, with no error.
- **`resolve_component_id_by_name_any`
  ([world.rs:872](modules/pill_engine/src/world.rs#L872)) is ambiguous.** It has
  no `persist_inserters` filter to collapse the candidate set, so it falls
  through to `max_by_key(bit)` — which, per §1.1, is not a recency ordering.
  This is the path the C# backend uses to bind module components by name.

---

## 2. Approach chosen

Resolve a component's column by **stable name**, not by `TypeId`.

Components opt in. `register_component` looks the name up first: if a column
already exists under that name and the layout is verified compatible, the new
registration **binds to the existing column** instead of allocating a second
one. The first registrant owns the concrete type; later registrants reach the
same rows through an alias.

Alternatives considered and rejected:

| Approach | Why not |
|---|---|
| Only one binary names the type (module-exported API) | Removes the duplicate at the source, but the project loses `Query<&Spline>` entirely. Rejected because hot per-frame access from both sides is a requirement. |
| Ship the type from one `prefer-dynamic` dylib | Both sides keep typed access, but every consumer must rebuild and reload together; a stale DLL is UB rather than an error. Directly hostile to hot reload. |
| Split into `SplineData` (`repr(C)`) + rich `Spline` | A layout fix, not an identity fix. Useful *with* this plan, not instead of it. |
| Rename to disambiguate | Documents the split rather than fixing it. |

### 2.1 Why this is zero cost per row

The concern that motivated this plan: does name-resolved access cost anything
in the inner loop? **No — done correctly it compiles to the same instructions
as typed access.**

The existing erased reader in the 2D renderer
([component.rs:439-443](modules/optional/rendering/pill_2d_renderer/src/component.rs#L439-L443))
is slow, but that is an artifact of its implementation, not of name resolution:

```rust
let position = unsafe { read_shared_component::<Position>(position_storage.get_dyn(row)) };
```

Per row this pays a non-inlinable virtual call, a fat-pointer construction that
is immediately discarded, a `read_unaligned`, and a full copy out. It was
written for a once-per-frame read-only sprite sweep and never needed better.

The underlying storage is a plain contiguous `Vec<T>`
([trait_type_map.rs:63](../Trait-Type-Map/src/trait_type_map.rs#L63)), so the
base pointer can be fetched **once per archetype** and indexed directly:

```rust
let (base, stride, len) = position_storage.raw_parts();   // one vcall per archetype
debug_assert_eq!(stride, size_of::<Position>());
let base = base as *const Position;
for row in 0..len {
    let position = unsafe { &*base.add(row) };            // compile-time stride
}
```

Typed access ([target.rs:141](modules/pill_engine/src/query/target.rs#L141))
resolves to `self.data.get_unchecked(i)` — base plus `i * size_of::<T>()`. The
loop above is the same addressing mode, vectorizes the same way, and is
identical machine code.

Even keeping `stride` as a runtime value, LLVM strength-reduces `row * stride`
into a pointer increment; the compile-time-size form above removes the question
entirely by verifying stride once per archetype instead of using it per row.

This pattern is not new to the codebase — `MutFetchState`
([target.rs:150-159](modules/pill_engine/src/query/target.rs#L150-L159))
already caches raw pointers to values and ticks and indexes them per row. The
only change is where the pointer came from.

### 2.2 What it actually costs

- **Per row:** zero. Same instructions.
- **Per archetype:** two or three vcalls plus one hash lookup for the alias.
  Negligible against dozens of archetypes.
- **A permanent `unsafe` boundary.** Soundness rests on the layout check in
  Phase 2, not on the compiler.
- **Layout mismatches become load-time errors, never compile-time errors.** A
  field reorder compiles cleanly on both sides and fails when the second binary
  binds. This is inherent to the approach.
- **Build effort**, concentrated in `Trait-Type-Map`, `ComponentRegistry`,
  `Archetype` and `QueryTarget`.

---

## 3. Sequencing constraint

Phase 1 modifies `Trait-Type-Map` (sibling repo, `Z:/OtherProjects/Other/Trait-Type-Map`).

That repo is **currently missing** `ErasedVecStorage`, `ErasedVecStorageInfo`,
`ErasedVecStorageOps` and `insert_erased`, which `pill_engine` imports — the
changes exist on another machine and were never pushed. `pill_engine` does not
compile until that lands.

Phase 0 is independent of this and should go in first regardless.

---

## 4. Phases

### Phase 0 — Guard against silent data loss (independent, do first)

Small, self-contained, and it stops shipping a silent snapshot-loss path while
the rest is built. Worth doing even if the remaining phases are deferred.

1. In `register_persistable_component_inner`
   ([persistence.rs:220](modules/pill_engine/src/persistence.rs#L220)), before
   evicting a same-name-different-`TypeId` entry, check whether its column
   still holds live rows. Live rows mean a concurrent peer, not a dead
   generation → return a typed error naming both `ComponentId`s and the shared
   type name, rather than evicting.
2. Give `resolve_component_id_by_name`
   ([persistence.rs:557](modules/pill_engine/src/persistence.rs#L557)) an
   explicit ambiguity result instead of `max_by_key(bit)`. Once Phase 0.1 holds,
   more than one surviving candidate is a bug, so it should be reported as one.
3. Fix the stale doc comment on lines 550-556: eviction provides uniqueness,
   the bit index is not a recency ordering. If a tiebreak is ever genuinely
   needed, `persist_registration_sequence`
   ([persistence.rs:587](modules/pill_engine/src/persistence.rs#L587)) is a
   true chronological ordering and is already maintained.
4. Same treatment for `resolve_component_id_by_name_any`
   ([world.rs:872](modules/pill_engine/src/world.rs#L872)) — it has no eviction
   filter, so it is ambiguous under duplicates today.

**Test:** two registrations of the same type name with different `TypeId`s and
live rows in the first → error, not eviction. Reload of a type name whose prior
generation has no live rows → still evicts cleanly, as today.

### Phase 1 — Raw parts accessor in `Trait-Type-Map`

Blocked on §3.

1. Add to `TraitVecStorage<Dyn>`
   ([trait_type_map.rs:125](../Trait-Type-Map/src/trait_type_map.rs#L125)):
   ```rust
   fn raw_parts(&self) -> (*const u8, usize, usize);       // (base, stride, len)
   fn raw_parts_mut(&mut self) -> (*mut u8, usize, usize);
   ```
   Implemented on `VecStorage<T, Dyn>` as `self.data.as_ptr() as *const u8`,
   `size_of::<T>()`, `self.data.len()`.
2. Also expose `align_of::<T>()`, needed by Phase 2.

**Test:** `raw_parts` base pointer plus `n * stride` yields the same address as
`get_unchecked(n)` for a populated storage.

### Phase 2 — Layout identity in `ComponentRegistry`

The check that makes the `unsafe` in Phase 4 sound.

1. Store **alignment** alongside `sizes`
   ([component.rs:298](modules/pill_engine/src/component.rs#L298)). Its absence
   is the sole reason the renderer uses `read_unaligned`.
2. Store a **structural schema hash** per registered name. Name and size
   together are *not* layout: `{f32, f32}` and `{u32, u32}` pass both checks and
   misread silently. The hash must cover field types and offsets.
   `calculate_schema_hash` ([persistence.rs:258](modules/pill_engine/src/persistence.rs#L258))
   and `ComponentFieldDescriptor` already provide the ingredients.
3. Add a `name -> ComponentId` alias table, populated at registration.
4. Opt-in marker so only components that declare shared identity participate —
   `repr(C)` plus a stable name, distinct from `std::any::type_name` which is
   not guaranteed stable. Most likely an attribute on `#[derive(PillComponent)]`.

**Test:** two `repr(C)` types with identical names and sizes but different field
types produce different schema hashes and are rejected.

### Phase 3 — Bind instead of allocate

1. In `register_component`
   ([world.rs:520](modules/pill_engine/src/world.rs#L520)), for a type with
   shared identity: look up the stable name. If a column exists, verify size,
   alignment and schema hash; on match, record `TypeId_new -> existing
   ComponentId` in the alias table and **do not allocate a new bit**. On
   mismatch, return a typed error naming both layouts.
2. First registrant behaves exactly as today.
3. Registering an alias must not disturb `persist_inserters` — Phase 0's guard
   must treat an aliased registration as a bind, not as a peer collision.

**Test:** two registrations of the same shared type with different `TypeId`s →
one bit allocated, one column, both `TypeId`s resolving to the same
`ComponentId`. Mismatched layout → error.

### Phase 4 — Alias-aware lookup (the bulk of the work)

`get_bit` ([component.rs:504](modules/pill_engine/src/component.rs#L504)) is a
genuine choke point: **16 call sites**, all routed through one accessor. Making
it consult the alias table on a direct-lookup miss covers every consumer at
once. The consumers are not all queries, and one of them is a soundness
requirement rather than a convenience.

#### 4.1 Alias-aware `get_bit` — covers four distinct consumers

1. **Archetype identity.** `get_or_create_archetype`
   ([world.rs:1614](modules/pill_engine/src/world.rs#L1614)) builds the
   `ComponentMask` that *defines* `ArchetypeId`. Once both `TypeId`s resolve to
   one bit, entities spawned by either binary land in the **same archetype**.
   This is the outcome the whole plan exists to produce, and it falls out of
   `get_bit` for free — but note it changes archetype topology, not merely
   lookup results.
2. **Scheduler safety — REQUIRED FOR SOUNDNESS, NOT CORRECTNESS.**
   `SystemAccess::build_component_masks`
   ([scheduler.rs:178-185](modules/pill_engine/src/scheduler.rs#L178-L185))
   folds each accessed `ComponentId` into the read/write masks that
   `conflicts_with` uses to decide parallel batching. If binary A's system
   writes `Spline` under `TypeId_A` and binary B's writes it under `TypeId_B`,
   and those resolve to different bits, the two masks do **not overlap** — the
   scheduler batches them in parallel and they race on the same rows.
   This means Phase 4.1 **must land together with Phase 3**, never after: the
   moment two `TypeId`s share a column but not a mask bit, there is a data
   race. See §6.
   Note `build_component_masks` already tracks a `complete` flag and falls back
   to full set comparison when a bit fails to resolve, so an *unresolved* alias
   degrades safely. An alias resolving to the *wrong* bit does not.
3. **Query masks.** `build_target_mask`
   ([query.rs:155-164](modules/pill_engine/src/query/query.rs#L155-L164)) and
   `build_filter_mask_pairs` ([query.rs:185-190](modules/pill_engine/src/query/query.rs#L185-L190)).
4. **Direct component access.** `get_component`
   ([world.rs:1451](modules/pill_engine/src/world.rs#L1451)),
   `get_component_mut` ([world.rs:1490](modules/pill_engine/src/world.rs#L1490)),
   `get_component_ptr_mut` ([world.rs:1527](modules/pill_engine/src/world.rs#L1527)),
   and script component registration
   ([world.rs:1133](modules/pill_engine/src/world.rs#L1133)).

`QueryTarget::component_ids` ([target.rs:50](modules/pill_engine/src/query/target.rs#L50))
needs **no change** — it is a static method with no world access and cannot
resolve aliases itself, but everything downstream of it goes through `get_bit`.

#### 4.2 `get_storage::<T>()` — the actual obstacle

```rust
pub fn get_storage<T>(&self) -> &F::Storage<T> {
    let e = self.entries.get(&TypeId::of::<T>()).expect("type not registered");
    F::storage_ref::<T>(&e)
}
```

[trait_type_map.rs:613](../Trait-Type-Map/src/trait_type_map.rs#L613) is keyed by
`TypeId::of::<T>()` and **panics** on miss. From the non-owning binary every
call panics. `component_storages` is a `TraitTypeMap` keyed the same way
([archetype.rs:424](modules/pill_engine/src/archetype.rs#L424)).

Call sites needing an alias fallback:

- `init_state` for `&T` ([target.rs:125](modules/pill_engine/src/query/target.rs#L125))
- `init_state` for `&mut T` (`MutFetchState`, values **and** ticks)
- `get_component` ([world.rs:1467](modules/pill_engine/src/world.rs#L1467))
- `get_component_mut` ([world.rs:1505](modules/pill_engine/src/world.rs#L1505))
- `get_component_ptr_mut` ([world.rs:1527](modules/pill_engine/src/world.rs#L1527))

Each resolves the alias to the owning `TypeId` and fetches via
`get_trait_storage(type_id)` + `raw_parts` from Phase 1, rather than the
generic `get_storage::<T>()`.

#### 4.3 Row access

`SendPtr<ErasedVecStorage<dyn Component>>` becomes a base-pointer-plus-stride
state for the aliased case; `fetch_with_state` indexes it directly per §2.1.

Mutation needs no new machinery: `Mut::new`
([change_detection.rs:60](modules/pill_engine/src/query/change_detection.rs#L60))
takes `(&mut T, &mut ComponentTicks, Tick)` — plain references, both derivable
from `raw_parts`. `DerefMut`'s tick write is unchanged.

#### 4.4 Change detection

`Added` / `Changed` ([filter.rs:266](modules/pill_engine/src/query/filter.rs#L266))
compare ticks by `ComponentId` and work once `get_bit` is alias-aware. Needs a
test proving a tick written by one binary is observed by the other.

**Tests:**
- Decisive: binary A registers, binary B aliases, B runs `Query<(&mut Shared,)>`,
  A observes the writes.
- Soundness: two systems in different binaries writing the same aliased
  component must report conflicting masks and never batch in parallel.
- Archetype: entities spawned from both binaries land in one archetype.
- Benchmark: aliased row access within noise of typed.

### Phase 5 — Migrate `pill_spline`

1. Mark `Spline` as a shared-identity component.
2. Confirm one column, both sides typed-speed, hot reload preserving data
   registered from either binary.
3. Delete the duplicate-registration workaround, if any exists by then.

### Phase 6 — Optional: adopt in the 2D renderer

`sprite_instances_named`
([component.rs:377](modules/optional/rendering/pill_2d_renderer/src/component.rs#L377))
hand-rolls name+size resolution and per-row `get_dyn`. Once Phases 1-4 land it
can use ordinary typed queries, deleting three `unsafe` blocks and the
`read_shared_component` helper.

Not required for correctness — the renderer works today — but it removes the
codebase's main source of hand-written erased access and would validate the new
path against a real workload.

---

## 5. Open questions

1. **Stable name source.** `std::any::type_name` is explicitly not guaranteed
   stable across compiler versions. A shared component needs an explicit stable
   name, probably an attribute argument. Does it need to be namespaced to avoid
   two unrelated crates colliding on `Transform`?
2. **Interaction with the 128-bit `ComponentMask` ceiling.** Aliasing *reduces*
   bit pressure, since duplicates now share one bit. Worth confirming
   `available_slots` ([component.rs:500](modules/pill_engine/src/component.rs#L500))
   reporting stays accurate.
3. **Does the C# path need this?** `resolve_component_id_by_name_any` exists to
   bind managed components by name. If shared identity subsumes that, the two
   name-resolution paths should merge rather than coexist.
4. **Scope.** Is `Spline` the only component needing this today? The engine work
   in Phases 1-4 amortizes across many shared components but is substantial for
   one.

---

## 6. Recommended order

Phase 0 immediately — it is small, independent of the `Trait-Type-Map` blocker,
and closes a silent data-loss path.

Phases 1-4 once `Trait-Type-Map` is pushed.

**Phase 3 and Phase 4.1 must land in the same change.** Phase 3 makes two
`TypeId`s share one column; Phase 4.1 makes them share one scheduler mask bit.
Between those two points the scheduler sees non-overlapping masks for systems
that write the same rows and will batch them in parallel — a data race. Do not
merge Phase 3 alone, even behind a feature flag that any test exercises.

Phase 4 is the bulk of the work: ~6 `get_storage` call sites plus the four
`get_bit` consumers, not a single `init_state` fallback. It should land with the
benchmark from its test section, so the zero-cost claim in §2.1 is verified
rather than assumed.

Phase 5 validates. Phase 6 is cleanup and can be deferred indefinitely.
