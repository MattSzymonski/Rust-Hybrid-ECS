# Porting the PBR renderer into `pill_master_renderer`

Plan for bringing [Pillware/Pill#38](https://github.com/Pillware/Pill/pull/38)
("feat: renderer: PBR, IBL, Passes") into this engine as
`modules/optional/rendering/pill_master_renderer`, plus a new
`modules/optional/rendering/pill_assets` cooking pipeline.

**Status: Phase 1 is built and tested. Phases 2-7 are still plan only.**

The asset store the renderer depends on now exists — see §3. Everything below
assumes meshes, textures, materials and shaders are stored in it.

Source of truth for the port is the PR head, not `main`:
`sp0lsh/Pill-Engine` @ `af052d1ef9bc31e2419275d50be9177e6abc68be`, branch
`feat/renderer`. 121 files, +7,824 / -3,498.

---

## 1. What is being ported

Ten graphics files, nine resource files, and the asset pipeline. Sizes are the
blob sizes at the PR head and are the honest measure of effort here.

### `engine/pill_engine/src/graphics/` → `pill_master_renderer/src/`

| File | Size | What it is |
|---|---:|---|
| `pass_pbr_opaque.rs` | 42.5 KB | The PBR opaque pass. The bulk of the work. |
| `pass_mesh.rs` | 14.9 KB | Unlit/simple mesh pass. |
| `pass_background.rs` | 9.7 KB | Equirect environment background. |
| `renderer.rs` | 8.3 KB | `PillRenderer` + `Pass` traits, handle types. |
| `render_queue.rs` | 7.6 KB | Sort-key composition, draw-call ordering. |
| `pass_tonemap.rs` | 6.0 KB | HDR → LDR resolve. |
| `pass_egui.rs` | 5.9 KB | UI pass. **Out of scope** (no egui here). |
| `dummy_renderer.rs` | 3.8 KB | Headless stub. |
| `mod.rs` | 1.0 KB | Re-exports. |
| `pass_tonemap_README.md` | 1.2 KB | Notes worth keeping. |

### `engine/pill_engine/src/resources/` → `pill_master_renderer/src/assets/`

Each becomes an `impl Asset` stored in the engine's `AssetManager` (§3), not a
`Resource`.

| File | Size | Notes |
|---|---:|---|
| `material.rs` | 24.4 KB | Becomes `PBRMaterial` in the PR. |
| `mesh.rs` | 20.7 KB | Vertex layouts, cooked-mesh load. |
| `texture.rs` | 9.3 KB | 2D, cubemap, HDR. |
| `shader.rs` | 6.5 KB | WGSL module wrapper. |
| `resource_manager.rs` | 8.3 KB | **Not ported** — `AssetManager` replaces it (§3). |
| `sound.rs` | 3.6 KB | Out of scope. |
| `resource.rs`, `resource_storage.rs`, `mod.rs` | 2.8 KB | **Not ported** — `Asset` + `Handle<T>` replace them (§3). |

### Components → `pill_master_renderer/src/component.rs`

`pbr_renderable_component.rs` (9.6 KB), `mesh_component.rs` (1.9 KB),
`camera_component.rs` (4.3 KB), `transform_component.rs` (5.4 KB),
`render_state_component.rs` (1.3 KB).

Per your instruction, **all renderer components and resources live inside
`pill_master_renderer`** — same layering as `pill_2d_renderer` today.

### `engine/pill_assets/` → `modules/optional/rendering/pill_assets/`

| File | Size |
|---|---:|
| `rules/glb_to_cooked_mesh.rs` | 15.5 KB |
| `rules/equirect_to_ibl.rs` | 14.6 KB |
| `rules/obj_to_cooked_mesh.rs` | 6.8 KB |
| `rules/procedural_equirect.rs` | 6.6 KB |
| `rules/hlsl_to_wgsl.rs` | 2.8 KB |
| `rules/png_to_cooked_tex.rs` | 2.2 KB |
| `lib.rs` (Pipeline + Rule) | 3.6 KB |

Shaders: 8 HLSL files + 2 includes, ~14 KB total.

---

## 2. The porting problem

The PR targets an engine that differs from this one in every layer the renderer
touches. This is a **reimplementation against a new substrate**, not a file copy.

| Concern | PR's engine | This engine |
|---|---|---|
| ECS | `Scene` + `SceneManager`, components in per-scene storage | Archetype `World`, `Query<(&A, &mut B)>` |
| Component decl | Hand-written `impl Component` | `#[derive(PillComponent)]` or `repr(C)` + `register_component_with_layout` |
| Multi-instance data | `ResourceManager` (PillTypeMap + PillSlotMap + name map) | ✅ `AssetManager` resource (§3) — **built** |
| Singletons | same `ResourceManager` | `World::insert_resource`, one per type — unchanged |
| Handles | `PillSlotMapKey`, `Handle<T>` in `pill_core` | ✅ `pill_engine::asset::Handle<T>`, generational — **built** |
| Systems | `RenderingSystem` reading `SceneManager` | `#[pill_hot] fn(Query<..>, Res<..>)` with scheduler access analysis |
| Errors | `anyhow` + `PillError` | `#[engine_error]` → `thiserror` + `miette` |
| Math | own `pill_core::math` | `pill_core::math` = glam aliases ✅ **compatible** |
| Hot reload | none | components cross a DLL boundary; layouts must be `repr(C)` and name-resolved |

Two consequences worth stating plainly:

1. **`pass_pbr_opaque.rs` cannot be ported mechanically.** It queries the old
   `Scene`, groups by material/mesh, and stages instances. The GPU half (bind
   group layouts, pipeline construction, uniform packing, draw encoding) ports
   closely; the world-facing half is rewritten against `Query`.
2. **Hot reload constrains the components.** `pill_2d_renderer` resolves
   `Position`/`Sprite` by stable type name and verified `repr(C)` size because
   host and DLL assign different `TypeId`s. Every renderer component here needs
   the same treatment, which the PR's components do not have.

---

## 3. Asset storage — ✅ BUILT

**Resources were left alone.** They remain singletons, which is the right model
for a clock or an input snapshot. Multi-instance data is a different storage
problem and got its own store:
[`pill_engine::asset`](modules/pill_engine/src/asset.rs).

### What exists

```rust
pub trait Asset: Send + Sync + 'static {}

pub struct Handle<T: Asset> { /* index: u32, generation: u32 */ }
// Copy + Eq + Hash + Debug. Carries no borrow, so it lives inside components.

pub struct AssetManager { /* TraitTypeMap<dyn Asset, VecOptionFamily> + per-type slots */ }
impl Resource for AssetManager {}

impl AssetManager {
    pub fn add<T>(&mut self, asset: T) -> Handle<T>;
    pub fn add_named<T>(&mut self, name: impl Into<String>, asset: T) -> Handle<T>;
    pub fn get<T>(&self, handle: Handle<T>) -> Option<&T>;
    pub fn get_mut<T>(&mut self, handle: Handle<T>) -> Option<&mut T>;
    pub fn handle_by_name<T>(&self, name: &str) -> Option<Handle<T>>;
    pub fn get_by_name<T>(&self, name: &str) -> Option<&T>;
    pub fn remove<T>(&mut self, handle: Handle<T>) -> Option<T>;
    pub fn contains<T>(&self, handle: Handle<T>) -> bool;
    pub fn len<T>(&self) -> usize;
    pub fn is_empty<T>(&self) -> bool;
    pub fn iter<T>(&self) -> impl Iterator<Item = &T>;
    pub fn iter_handles<T>(&self) -> impl Iterator<Item = (Handle<T>, &T)>;
}
```

**Storage mirrors how components are held** — a type-erased column per type in
a `TraitTypeMap`, minus archetypes, which group *entities* and mean nothing for
an asset. The family is `VecOptionFamily` rather than `VecFamily` so unloading
frees one slot without shifting the rest: every live handle keeps its index.

**`AssetManager` is itself one resource**, so the whole store arrives through
the existing `Res<AssetManager>` / `ResMut<AssetManager>` parameters and the
scheduler's existing `ResourceId` conflict analysis already covers it. **No
scheduler change was needed.**

**Inserted automatically** by `Engine::new`, alongside `Time`, so a project
never constructs one and no code has to check whether it exists.

### What this means for the renderer

Every renderer resource in §1 becomes an `Asset`, not a `Resource`:

```rust
impl Asset for Mesh {}
impl Asset for Texture {}
impl Asset for PBRMaterial {}
impl Asset for Shader {}
impl_trait_accessible!(dyn Asset; Mesh, Texture, PBRMaterial, Shader);
```

A pass reads them through one parameter:

```rust
fn pbr_opaque(
    assets: Res<AssetManager>,
    query: Query<(&Transform, &PbrRenderable)>,
) { /* assets.get(renderable.mesh) */ }
```

Components store `Handle<Mesh>` / `Handle<PBRMaterial>` rather than an index or
a name. **Handles are generational**, so a handle kept across an unload resolves
to `None` instead of silently addressing whatever was loaded into the slot next
— a missing model rather than a wrong one.

This replaces the PR's `ResourceManager`, `Resource` trait,
`ResourceStorage` and `PillSlotMapKey` wholesale. Those files are **not ported**.

⚠️ **Handles are `repr(Rust)`.** A component storing one crosses the DLL
boundary on hot reload, so either `Handle<T>` gains `#[repr(C)]` or components
store a plain `{ index, generation }` pair and rebuild the handle. **Decide in
Phase 3** — it is a two-line change now and a silent corruption later.

### Verification

13 unit tests pass (isolated harness; `pill_engine` itself cannot build yet —
Phase 0.3). Covers many-per-type storage, per-type independence, mutation,
removal, **stale-handle aliasing**, slot reuse, name binding and rebinding,
iteration skipping holes, and handle hashability.

One thing the compiler caught: `TraitTypeMap` stores
`Box<dyn TraitVecOptionStorage<..>>`, and a trait object carries no auto-trait
bounds, so `AssetManager` lost `Send + Sync` and could not be a `Resource`. The
guarantee genuinely holds — `Asset: Send + Sync`, and `add` is the only way a
value enters a column — so there is a documented `unsafe impl Send/Sync`,
matching how `DynamicColumn` handles the same situation at
[archetype.rs:359](modules/pill_engine/src/archetype.rs#L359). Worth revisiting
if `Trait-Type-Map` ever adds the bounds upstream.

### Also built alongside it

[`pill_engine::time::Time`](modules/pill_engine/src/time.rs) — elapsed ms since
start, previous frame duration, and a clamped delta. Advanced directly in
`process_frame` before systems run, **not** as a registered system, because
`build_execution_graph` orders systems independently of registration order and a
time system could be batched after its readers. 6 tests pass.

---

## 4. Crate layout

```
modules/optional/rendering/
  pill_assets/                     NEW  — offline cooking, a build-time tool
    src/lib.rs                          Pipeline + Rule
    src/rules/{hlsl_to_wgsl, png_to_cooked_tex,
               glb_to_cooked_mesh, obj_to_cooked_mesh,
               equirect_to_ibl, procedural_equirect}.rs
    src/bin/cook.rs                     CLI entry point

  pill_master_renderer/            REWORKED — currently a copy of the 2d renderer
    src/component.rs                    Transform, Camera, MeshRenderable, PbrRenderable
    src/resources/{mesh,texture,material,shader}.rs
    src/passes/{background,pbr_opaque,tonemap,mesh}.rs
    src/render_queue.rs
    src/renderer.rs                     Renderer + Pass trait
    src/error.rs                        #[engine_error] RendererError
    res/shaders/*.wgsl                  cooked output, committed
    managed/RendererComponents.cs       C# mirrors
```

`pill_assets` is a **build-time tool**, not a hot-loadable module: it depends
on `gltf`, `png`, `tobj`, `image`, none of which belong in a game DLL. Runtime
loading of cooked files lives in `pill_master_renderer::resources`.

---

## 5. Phases

Each phase compiles and is testable on its own.

### Phase 0 — Unblock the workspace ⚠️ *prerequisite*

**The workspace does not resolve today.** Verified, three separate causes:

1. **Stale crate name.** `a945d27` renamed `pill_wgpu_renderer` →
   `pill_2d_renderer`, but **15 files still import the old name**:
   `pill_host` (8), `pill_editor` (3), `project_rs` (2), `pill_engine/Cargo.toml`,
   `pill_host/Cargo.toml`. `cargo metadata` fails on the missing manifest.
2. **Workspace glob no longer matches.** `modules/Cargo.toml:16` declares
   members as `"optional/*"`, but the renderers now sit at
   `optional/rendering/*` — one level deeper. Needs `"optional/rendering/*"`
   added (and bears on **Q5**: a glob covering that directory would also
   swallow `pill_assets`).
3. **`Trait-Type-Map` is out of date** — missing `ErasedVecStorage`,
   `ErasedVecStorageInfo`, `ErasedVecStorageOps`, `insert_erased`. Confirmed
   pre-existing by stashing all local changes. Needs the push from your other PC.

Nothing below can be verified until all three are cleared.

### Phase 1 — Asset store in `pill_engine` (§3) — ✅ **DONE**
`Asset`, `Handle<T>`, `AssetManager`, auto-inserted by `Engine::new`. 13 tests.
`Time` built alongside it, 6 tests. Neither is compile-verified in-crate yet —
blocked on Phase 0.3.

### Phase 2 — `pill_assets` skeleton + shader cooking
`Pipeline`/`Rule`, `cook` binary, `hlsl_to_wgsl`. Produces the WGSL the later
phases consume. Deps: `anyhow`, `glob`, `naga`.

### Phase 3 — Renderer assets
`Mesh`, `Texture`, `Shader`, `PBRMaterial` as `impl Asset` + their cooked-file
loaders, stored in the `AssetManager` (§3). `png_to_cooked_tex`,
`obj_to_cooked_mesh` in `pill_assets`.
**Settle the `Handle<T>` `repr(C)` question here** (§3) before any component
stores one.

### Phase 4 — Renderer core + mesh pass
`Renderer`, `Pass` trait, `render_queue.rs`, `pass_mesh.rs`. Components:
`Transform`, `Camera`, `MeshRenderable` (storing `Handle<Mesh>`).
**Milestone: a textured mesh on screen.**

### Phase 5 — PBR opaque pass
`pass_pbr_opaque.rs` (42 KB) + `PbrRenderable`. `glb_to_cooked_mesh` for glTF.
The largest single phase; likely to split further once underway.

### Phase 6 — IBL + background
`equirect_to_ibl`, `procedural_equirect`, `pass_background.rs`. Irradiance and
prefiltered specular baking, BRDF LUT.

### Phase 7 — Tonemap + wiring
`pass_tonemap.rs`, HDR render targets, host/editor integration, C# mirrors,
example project.

**Out of scope:** `pass_egui.rs` (no egui in this engine), `sound.rs`,
networking, the PR's `Scene`/`SceneManager`.

---

## 6. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Workspace does not build (Phase 0) | Blocks everything | Fix the rename before starting |
| `Trait-Type-Map` out of date | `pill_engine` won't compile | Push from other PC |
| `pass_pbr_opaque.rs` is 42 KB against a foreign ECS | Largest unknown | Port GPU half closely, rewrite query half; split Phase 5 |
| Hot reload + `repr(C)` components | Silent corruption if wrong | Follow the `pill_2d_renderer` name+size resolution pattern exactly |
| `Handle<T>` is `repr(Rust)` in a component | Silent corruption across a reload | Settle Q1b in Phase 3 — `#[repr(C)]` or store the raw pair |
| Project `init` re-adds assets each reload | Store grows without bound | Name-keyed loaders + `handle_by_name` check (Q1) |
| wgpu version drift (PR era vs 26.0.1 here) | API churn | Re-derive against 26.0.1, do not trust PR call sites |
| HLSL→WGSL toolchain | Build-step fragility | Commit cooked WGSL so a normal build never cooks |
| Every project linking this gains wgpu | ~215 ms per hot patch | Already true of `pill_2d_renderer`; documented |

---

## 7. Open questions

**Q1 — Assets across hot reload.** ✅ *Resolved by construction.* The
`AssetManager` is inserted by `Engine::new` and lives in the world's resource
map, which a reload does not clear — `clear_systems_owned_by` retires systems
only, and explicitly leaves "component registrations, entities, and resources"
untouched ([engine.rs:637](modules/pill_engine/src/engine.rs#L637)); no code in
`pill_host` removes a resource. Loaded
meshes, textures and their GPU buffers therefore survive a project reload with
no rebuild. **Still to verify once the workspace builds:** that a project
re-running `init` does not re-add the same assets each generation and grow the
store without bound. Phase 3 should make loaders name-keyed and
`handle_by_name`-checked so re-registration is idempotent.

**Q1b — `Handle<T>` across the DLL boundary.** *New, replaces Q1.* A component
storing a `Handle<Mesh>` crosses the host/DLL boundary on hot reload, where
`repr(Rust)` layout is not guaranteed to agree. Either give `Handle<T>`
`#[repr(C)]`, or have components store a plain `{ index, generation }` pair.
*Needed before Phase 3*, and cheap now versus a silent corruption later.

**Q2 — Coexistence with `pill_2d_renderer`.** Three renderers now define their
own `Position`/`Sprite`/`Color`. Does `pill_master_renderer` define `Transform`
etc. independently (chosen approach), and can a project link two renderers at
once? *Affects C# mirror names — `TracyLive.Position` would collide.*

**Q3 — Camera.** The PR's `CameraComponent` assumes its `Scene`. Should camera
be a component queried per frame, or a renderer-held resource?
*Needed before Phase 4.*

**Q4 — Cooked assets in git.** Cooked meshes/textures are large binaries.
Commit them (simple, bloats the repo) or cook on first build (needs the
toolchain on every machine)? Recommendation: commit cooked **shaders** only.
*Needed before Phase 2.*

**Q5 — `pill_assets` as workspace member.** Under `optional/`, the glob
`optional/*` would pick it up — but it is a build tool, not a module, and would
pull `gltf`/`png` into workspace resolution. Exclude it, or move it to
`devops/tools/`? *Needed before Phase 2.*

**Q6 — Target platform for Phase 5+.** The PR cites MacBook M1. Is Windows/DX12
the target here, or is cross-platform parity required?

**Q7 — Scope confirmation.** Is full PBR+IBL the goal, or is a textured-mesh
renderer with render passes (Phase 4) the real destination? This changes the
estimate by roughly a factor of three.

---

## 8. Effort

| Phase | Estimate | Depends on |
|---|---|---|
| 0 Unblock | 0.5 d | — |
| ~~1 Asset store~~ | ~~1–2 d~~ ✅ done | — |
| 2 pill_assets + shaders | 1–2 d | Q4, Q5 |
| 3 Renderer assets | 2–3 d | 2, Q1b |
| 4 Core + mesh pass | 3–4 d | 3, Q3 |
| 5 PBR opaque | 5–8 d | 4 |
| 6 IBL + background | 4–6 d | 5 |
| 7 Tonemap + wiring | 2–3 d | 6, Q2 |

**Phase 4 ("a textured mesh on screen"): ~7–9 days remaining.**
**Full PBR + IBL: ~18–26 days remaining.** Assumes the Phase 0 blockers are
cleared and excludes debugging GPU output, which for PBR/IBL is historically
significant.

---

## 9. Recommended next step

**Clear Phase 0.** Phase 1 is done but cannot be compile-verified until the
workspace resolves, so every later phase would be building on untested ground.
The rename and the workspace glob are mechanical; the `Trait-Type-Map` push has
to come from your other machine.

Then answer **Q4, Q5, Q7** to unblock Phase 2, and **Q1b** before any component
stores a `Handle<T>`.
