# Introduction to `pill_master_renderer`

The 3D renderer of Rust-Hybrid-ECS. It takes the scene the engine holds and  
turns it into frames on a window, via [wgpu](https://wgpu.rs).

---

## How to read this

This walks the same code seven times, each pass closer than the last. You are  
not meant to read all of it — read until the picture is clear enough to open an  
editor, then come back when a name in the source doesn't land.

| Level | Answers                                                    |
| ----- | ---------------------------------------------------------- |
| 0     | What is this, in one paragraph?                            |
| 1     | What does it do, and where does it sit in the engine?      |
| 2     | What are the moving parts, and how are they separated?     |
| 3     | What happens in one frame, in order?                       |
| 4     | What GPU objects exist, and who owns them?                 |
| 5     | How does a queue of instances become instanced draw calls? |
| 6     | What holds it together — errors, profiling, reload?        |
| 7     | Where to start reading, and what the shorthand means       |

Two conventions throughout: **paths are relative to the crate root**  
(`src/…`, `docs/…`), and a name in `code font` is a real item you can search for.

A note before you build anything: this crate lives on the Windows box. The  
files are mounted into this Linux editor so you can read and change them, but a  
local `cargo build` would target the wrong OS. Build and run it over SSH  
instead — see [§7](#building-and-running-it).

---

## Level 0 — The one-paragraph version

`pill_master_renderer` is a forward renderer on wgpu, driven by a declarative  
pass chain. A gameplay project describes its scene as ECS components and its  
appearance as plain data assets; the renderer mirrors those assets onto the GPU,  
flattens the scene into an owned `RenderFrame` once per update, sorts the  
drawables into one queue, and records the whole frame into a single command  
buffer. It keeps the GPU mirror of four asset types — meshes, textures, shaders,  
materials — plus the pass objects the chain needs, and rebuilds only the entries  
whose asset content version moved.

---

## Level 1 — The big picture

### What a renderer does

Strip away the APIs and a renderer does three things per frame:

1. **Know the scene** — where every drawable is, what it looks like, which
  camera is looking at it.
2. **Keep the GPU in step** — make the device hold a copy of whatever the scene
  needs (geometry, textures, shaders, uniform values).
3. **Record and submit** — write the commands that transform and shade those
  drawables into pixels, hand them to the GPU, and present the result.

This crate is organised around exactly those three, in that order. The  
separation isn't cosmetic: step 1 belongs to the ECS, step 3 belongs to wgpu, and  
almost every design decision here is about not letting the two leak into each  
other.

### Where it sits

```
pill_core             base types, math, errors, logging, the slot arena
pill_engine           ECS: World, resources, systems, AssetManager, Time
pill_master_renderer  ── this crate
pill_host             the host that drives the engine and owns the window
pill_standalone       the generic launcher that picks a project
examples/*            gameplay projects
```

Three facts about that position:

- **It is not hot-reloadable.** Sibling renderers under `modules/extensions/`  
compile into a DLL the host swaps out while running. This one doesn't: it is  
built around a live window surface and is driven from the host's own frame  
loop. The host links it directly under its `rendering` feature  
(`modules/hosting/pill_host/Cargo.toml` → `rendering = ["dep:winit", "dep:pill_master_renderer"]`).
- **Projects never see wgpu.** A project depends on this crate with  
`default-features = false` and touches only its data half — components and  
assets. wgpu stays in the host's dependency graph, which is what lets a  
headless host and a windowed one load the same project artifact.
- **It is one of several backends.** `pill_2d_renderer` and  
`pill_embedded_renderer` sit beside it. This one is the general 3D renderer.

### The frame loop

```
   gameplay systems run (post-update ordering, engine-defined)
            │
            ▼
   rendering_system          ← this crate; flattens the world into RenderFrame
            │
            ▼
   RenderingHost::run_one_frame
            │  takes &RenderFrame and &AssetManager out of the world
            ▼
   Box<dyn PillRenderer>::render(frame, assets)
            │  the wgpu Renderer, or the HeadlessRenderer stub
            ▼
   FrameOutcome + RenderMetrics
```

The host owns a `Box<dyn PillRenderer>`. That trait object is the seam: the same  
frame loop runs whether the backend is real wgpu or the headless stub that  
accepts everything and reports `FrameOutcome::Skipped`.

---

## Level 2 — The architecture

### Four layers

Nearly every file here belongs to one of four layers. Mixing them up is the  
main source of confusion on a first read.

| Layer                                       | Lives in           | Owned by                    | Contents                                                                                                                      |
| ------------------------------------------- | ------------------ | --------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| **Assets** — plain data a game authors      | `src/assets/`      | the world's `AssetManager`  | `Mesh`, `Texture`, `Shader`, `Material`, `RenderPass`, `RenderingPipeline`                                                    |
| **Components** — what is where in the scene | `src/components/` | the ECS `World`, per entity | `TransformComponent`, `CameraComponent`, `MeshRendererComponent`, `DirectionalLightComponent`                                 |
| **GPU resources** — the uploaded mirror     | `src/resources/`   | the `Renderer`              | `RendererMesh`, `RendererTexture`, `RendererShader`, `RendererMaterial`, `RendererPass`, `RendererCamera`, `EngineParameters` |
| **The frame** — one update's renderer input | `src/frame.rs`     | extracted from the ECS      | `RenderFrame` and its parts                                                                                                   |

Read down the table and the rule appears: **the `Renderer*` prefix means "this**  
**is the GPU side"**. `Mesh` is bytes a game holds; `RendererMesh` is a vertex and  
index buffer. `Material` is a shader handle plus values; `RendererMaterial` is  
two `wgpu::BindGroup`s.

### The two boundary rules

Everything else follows from these.

**Rule 1 — the renderer reads the world only for assets.**

```rust
pub fn render(&mut self, frame: &RenderFrame, assets: &AssetManager) -> Result<FrameOutcome>
```

That is the entire input: the frame the ECS built, and the store the assets live  
in. No `&World`, no ECS queries, no borrows into component storage. By the time  
`render` is called, `rendering_system` has already *decided* everything that  
needs deciding — which camera, which passes, which instances — and copied it  
into owned values: asset *keys* (`u64`) instead of handles, the camera by value  
instead of by reference.

The assets are the exception, and deliberately so: they are read where they  
live, borrowed for the call, so the renderer can diff them against what it last  
uploaded without a copy ever being made. The borrow is also what makes the  
frame consistent — nothing can mutate the store while a frame is being drawn.  
This buys two things — the wgpu code needs no ECS borrow discipline, and a frame  
is a plain struct a test can build without a world.

**Rule 2 — assets are data, and the GPU mirrors them.**

A game edits the `Mesh` it holds. It never touches a buffer. Each asset carries  
a *content version*, and the renderer diffs those versions against what it last  
uploaded:

```
game edits Mesh          renderer sees version moved
      │                          │
      ▼                          ▼
 AssetManager                 sync_meshes
 (version: 4 → 5)         re-upload that one mesh;
                          its neighbours keep their buffers
```

One edited texture re-uploads while everything around it is left alone. Nothing  
in `src/assets/` names a wgpu type, and nothing in `src/resources/` knows the  
ECS exists.

### Module map

`src/`

| File                     | Role                                                                                 | Approx. |
| ------------------------ | ------------------------------------------------------------------------------------ | ------- |
| `lib.rs`                 | re-exports and `register()`                                                          | 120     |
| `api.rs`                 | the `PillRenderer` trait, `FrameOutcome`, metrics, the headless stub                 | 150     |
| `renderer.rs`            | `Renderer` (the frame skeleton) and `State` (device, targets, frame state)           | 575     |
| `rendering_resources_manager.rs` | `RenderingResourcesManager`: the GPU object per asset, and the queue a frame draws from | 560 |
| `pipeline.rs`            | `ScriptableRenderingPipeline`: the chain's pass objects, and its per-frame plan        | 690     |
| `surface.rs`             | the window surface, its swapchain, and the GPU bootstrap                             | 370     |
| `frame.rs`               | `RenderInstance`, `ResolvedPass`, `RenderFrame`, `rendering_system`                  | 580     |
| `components/`            | the scene contract, one file per component, and `register_components`                | 485     |
| `assets/`                | the six asset types and their builders                                               | 1010    |
| `resources/`             | one module per GPU resource kind, each with the handle that names it                 | 2395    |
| `drawers/mesh_drawer.rs` | batching and draw recording                                                          | 510     |
| `render_queue.rs`        | the packed `u64` sort key                                                            | 160     |
| `instance.rs`            | one instance's transform, in the shader's layout                                     | 210     |
| `config/`                | bind-group indices, batch size, and the shipped pipelines, each with its shaders (`common_shaders/`, `simple_pipeline/`, `pbr_pipeline/`, `post_processing/`) | 672 |
| `error.rs`               | `RendererError`, `capturing_validation`                                              | 140     |
| `profiler.rs`            | GPU timing queries                                                                   | 670     |
| `build.rs`               | cooks `*.hlsl` → `*.wgsl` before the crate compiles                                  | 94      |

`managed/RendererComponents.cs` is the generated C# mirror of the components, so  
a C# project binds the same field layouts.

---

## Level 3 — One frame, stage by stage

Now the sequence, with the function names. This is the skeleton every later  
level hangs off.

```
Renderer::render(frame, assets_manager)
 ├─ 1. early out                  minimized surface, or no camera → Skipped
 ├─ 2. rendering_resources_manager.sync         textures → shaders → meshes → materials
 ├─ 3. pipeline.ensure            offscreen targets, then pass objects
 ├─ 4. rendering_resources_manager.build_queue  one RenderQueueItem per instance, then sort
 ├─ 5. pipeline.plan              hand each pass its draws
 ├─ 6. State::render              encoder → one wgpu render pass per plan → submit
 └─ 7. metrics                    prepare_micros, submit_micros, draw_calls, passes
```

Step 0 happens earlier, and is not in this function.

### 3.0 `rendering_system` — flattening the world

Registered by `register()` as a **post-update** system, so gameplay has already  
written every component it reads. It does four things:

- **Resolve the pass chain** — only when `(asset revision, pipeline asset key)`  
moved. `resolve_passes` drops disabled passes, drops handles that no longer  
resolve, sorts by `RenderPass::order`, and warns if the surface-writing pass  
isn't last. An empty result means the built-in pass.
- **Pick the camera** — `pick_camera` takes the enabled camera with the highest  
`priority`; ties go to the **lowest entity id**, so the choice can't wander  
with traversal order.
- **Collect instances** — every entity with a `TransformComponent` and a  
`MeshRendererComponent`, skipping any whose mesh or material handle no longer  
resolves. A dangling handle draws nothing; it does not fail the frame.
- **Advance the clock** — `seconds` accumulates `Time::delta_seconds()`, so it  
advances by the time the game saw pass, not by wall clock at extraction.

The result is a `RenderFrame` — a `Resource` in the world, keyed by  
`shared_name()` so the host and a project module agree on which resource it is.

### 3.1 Early out

```rust
if self.minimized || !frame.has_camera {
    return Ok(FrameOutcome::Skipped);
}
```

`Skipped` is not an error. A minimized window, or a scene with no enabled  
camera, is a normal frame with nothing to present.

### 3.2 `rendering_resources_manager.sync` — making the GPU match the store

Gated on the manager's revision, so unchanged frames cost one comparison. Inside,  
the order matters:

```
1. sync_textures   ─┐  materials bind these by handle, so they
2. sync_shaders    ─┘  have to exist before a material is rebuilt
3. sync_meshes        independent of the other three
4. invalidate_dependent_materials
5. sync_materials     rebuilds whatever step 4 forgot
```

Each `sync_*` walks the store's column for that type, compares the asset's  
content version against the version the GPU object was built from, and rebuilds  
the ones that moved. Removed assets have their slot and version record dropped.

Two subtleties:

- **A rebuilt texture or shader bumps `resource_epoch`.** Pass bind groups  
reference those objects by handle, so a change to one has to invalidate them.
- **A rebuilt texture or shader also forgets the materials that read it.**  
`invalidate_dependent_materials` removes the material's version record, so  
step 5 rebuilds it in the same pass, against the new object. The material's  
own content version never moved — the object underneath it did.

Failures here are per-asset: a texture that won't decode is logged by name and  
skipped, and the rest of the revision still reaches the GPU.

### 3.3 `pipeline.ensure` — building the chain's GPU objects

Only when something they depend on moved. The cheap gate is a tuple:

```rust
let inputs = (chain_generation, context.resource_epoch, width, height);
if self.inputs == Some(inputs) { return; }
```

An unchanged frame costs one tuple comparison — the expensive signature strings  
are only built once that tuple moves. Then two signatures decide:

| Signature             | Decides                   | Covers                                                                           |
| --------------------- | ------------------------- | -------------------------------------------------------------------------------- |
| `offscreen_signature` | the offscreen **targets** | every target name and scale the chain declares, at the surface size              |
| `chain_signature`     | the **pass objects**      | the chain's content, the resource epoch, and each pass's parameters and textures |

Both iterate their maps in slot order rather than `HashMap` order — otherwise an  
arbitrary iteration order could make an unchanged chain look new. Passes are then  
built in chain order, carrying a growing `defined_targets` set so a pass may only  
read targets an **earlier** pass declared. A pass that reads its own output, or a  
later pass's, is refused rather than bound to whatever the map happens to hold.

### 3.4 `rendering_resources_manager.build_queue` — resolving instances to GPU handles

One `RenderQueueItem` per instance, then one sort:

```rust
render_queue.push(RenderQueueItem {
    key: compose_render_queue_key(order, shader, material, mesh),
    entity_index: index as u32,
});
render_queue.sort_unstable();
```

`order` is the material's `rendering_order`; the three handles are the *GPU*  
handles, resolved through the renderer's own key→handle maps. An instance whose  
mesh never uploaded is skipped; one whose material never built falls back to the  
default material; one whose shader is missing falls back to the default shader.  
So a partially-broken asset revision still draws something.

The key layout is covered in [§5](#level-5--the-draw-path).

### 3.5 `pipeline.plan` — handing each pass its draws

For each resolved pass, `plan` decides what it records:

| Pass                                    | `PassSlot`    | What it draws                                                          |
| --------------------------------------- | ------------- | ---------------------------------------------------------------------- |
| Geometry, names a shader, shader loaded | `Drawable`    | only the queue items whose material's shader is that shader            |
| Geometry, names no shader               | `Unshaded`    | the **whole queue**, each instance through its own material's pipeline |
| Geometry, shader missing/unbuilt        | `Unsupported` | nothing; `report_skip` names it once                                   |
| Fullscreen, pipeline built              | `Drawable`    | one triangle, no vertex buffers                                        |
| Fullscreen, no pipeline                 | `Unsupported` | nothing                                                                |

Two things worth knowing:

- `Unshaded` is the **built-in chain**: no pipeline is built per pass, each  
instance draws through the pipeline its material names.
- If the whole plan comes out empty, a synthetic `frame.clear` pass is added.  
A swapchain image nobody cleared presents whatever was in it last.

A shader-filtered pass pays for a `Vec` of queue items; the unfiltered case  
borrows the queue via `Cow`.

### 3.6 `State::render` — recording and submitting

The only place a frame is acquired and submitted. The swapchain itself belongs  
to `Surface`; `render` asks it for an image and hands that image back when it  
is presented.

1. **Acquire the surface texture.** `Lost` / `Outdated` is recovered by
  reconfiguring the surface and retrying **once** — not by returning an error  
   and leaving the renderer dead until some later resize event arrives.
2. **Update the uniform buffers** — engine parameters (fog, frame size, time),
  then the camera (position, view-projection, built for the *viewport's* aspect  
   ratio, not the surface's).
3. **One command encoder** for the whole frame. Inside it, one wgpu render pass
  per planned pass.
4. **Clear only on the first pass.** The plan marks an entry `clear` when it is
  the first one pushed, so the first recorded pass opens its targets with  
   `LoadOp::Clear`; the rest `Load` and add to what's there.
5. **Geometry** joins the mesh drawer; **fullscreen** sets its pipeline, binds
  the groups its shader declares, and `draw(0..3, 0..1)` — the triangle's  
   corners come from `SV_VertexID`, so there is no vertex buffer at all.
6. **Submit inside `capturing_validation`.** wgpu validates a command buffer at
  submission, and a validation failure would otherwise reach the  
   uncaptured-error handler as a panic. This is the last creation-class failure a  
   frame can hit.
7. **Present.**

---

## Level 4 — The GPU layer

### `State` — the device, the targets, and the frame

`Renderer` holds the asset caches; `State` holds everything a frame's lifetime  
touches. The split exists so surface lifecycle (new / resize / reconfigure) has a  
home independent of the caches.

| Field                              | What it is                                                      |
| ---------------------------------- | --------------------------------------------------------------- |
| `surface`                          | the window's swapchain — see `Surface` below                    |
| `device`, `queue`                  | every GPU object in the crate is created from these             |
| `depth_format`, `depth_texture`    | `Depth32Float`, shared by the geometry passes                   |
| `offscreen`                        | `HashMap<String, RendererTexture>` — the chain's colour targets |
| `mesh_drawer`                      | the instance buffer and its batching state                      |
| `camera_bind_group_layout`         | layout every camera bind group is built from                    |
| `renderer_resource_storage`        | the five slot maps plus the engine parameters                   |

**Why `State` owns the offscreen targets** rather than the passes: two passes  
name the same target — one writes it, the next reads it — and a texture owned by  
either would be a lifetime the chain can't express. The chain reaches them  
through `State` too, which is why `offscreen` and `depth_texture` are  
`pub(crate)`: a pass's target formats and attachments come from there.

A resize clears `offscreen` and the pass slots, because every offscreen target is  
sized to the surface and a stale size shows up as a picture that shrinks with the  
window instead of resizing with it.

### `Surface` — the swapchain and the GPU bootstrap

`src/surface.rs`. Everything above needs a device before it can exist, and an  
adapter can't be picked without something to present to, so the three are built  
together and handed back as one unit:

```rust
let (surface, device, queue) = Surface::create(window, width, height).await?;
```

`Surface` owns what is surface state — the swapchain, its `SurfaceConfiguration`,  
and the colour format chosen from the surface's capabilities — and takes  
`&wgpu::Device` as an argument wherever it needs one. It is deliberately not the  
device's owner; `State` is.

| Method           | What it does                                                         |
| ---------------- | -------------------------------------------------------------------- |
| `create`         | instance → surface → adapter → device → queue → configure            |
| `acquire`        | gets a frame; `Lost` / `Outdated` reconfigures and retries once      |
| `resize`         | reconfigures the swapchain to a new pixel size                       |
| `format`, `size` | the colour format, and the current pixel size                        |
| `configuration`  | the `SurfaceConfiguration`, for creating surface-sized resources     |

**Why the alpha mode has a fallback.** `Surface::configure` returns nothing, and  
a refused configuration is reported through the device's uncaptured-error path,  
which panics by default — taking the whole host down before any frontend sees a  
`RendererError`. A compositing alpha mode needs a compositing window, which a  
capability list does not promise, so the requested mode is tried inside its own  
error scopes and `Opaque`, the mode every surface must support, is the fallback.

Nothing here names a windowing crate: the surface is built from a  
`RendererWindow`, and every size that reaches it is a plain `u32`.

### `ScriptableRenderingPipeline` — the chain as GPU objects

`src/pipeline.rs`. The game's chain is an *asset*: a `RenderingPipeline` holding  
`RenderPass` handles — the order and nothing else, because a pass already carries  
the shader it draws with and the values it reads. It is written through  
`RenderingManager` and resolved once per frame into `ResolvedPass` values. This  
is the other side of that declaration — what the chain becomes once the device  
has seen it.

| Field             | What it is                                                                  |
| ----------------- | --------------------------------------------------------------------------- |
| `passes`          | one `PassSlot` per pass of the chain, in chain order                        |
| `signature`       | the chain content those passes were built from                              |
| `inputs`          | `(chain_generation, resource_epoch, width, height)` — the per-frame gate   |
| `offscreen_key`   | the offscreen layout the current targets were built for                     |
| `skipped_passes`  | passes already reported as undrawable, so each is named once                |
| `chain_log`       | the chain last printed, so an unchanged one prints nothing                  |

It is deliberately narrow: no window, no frame loop, no asset cache. What it  
needs of those arrives in a `ChainContext` — the device state by reference, the  
shader and texture caches by key, and the resource epoch — which is what keeps  
it a unit rather than a set of methods that happen to live together.

Two invalidations, and the difference matters:

- `invalidate` drops the passes and both signatures, and keeps the offscreen  
targets. An asset edit is what calls it, and an asset edit changes no target.
- `invalidate_targets` is `invalidate` plus the offscreen key, for a surface  
size that moved — every offscreen target is built at the surface size.

### The two pipelines the renderer ships

`src/config/`. A `RenderingPipeline` is an asset, so the renderer hands a project  
one as easily as a project hands the renderer one:

| Module                   | The frame                                                                                 |
| ------------------------ | ----------------------------------------------------------------------------------------- |
| `config/simple_pipeline/` | one lit geometry pass, through the built-in lit shader, to the surface                    |
| `config/pbr_pipeline/`   | the geometry half: the lit pass, its shader, a neutral material                           |
| `config/post_processing/` | the four fullscreen passes that end the frame                                            |

`pbr_pipeline`'s `mod.rs` is what composes the two halves into one pipeline asset,  
and every module here keeps its shaders in a `shaders/` beside itself — the lit  
fragment stage under `config/simple_pipeline/`, the geometry fragment stage under  
`config/pbr_pipeline/`, the four fullscreen stages under `config/post_processing/`.  
The cooker's rule matches one level and does not walk, so each of those directories  
is a root of its own (see [The shaders](#the-shaders)). The vertex stage the lit  
passes share is not one pipeline's, so it sits in `config/common_shaders/`.

`pbr_pipeline` is the default. `register` installs it and points  
`RenderingManager` at it, so a project that declares no frame still draws one; a  
project that wants a different frame calls `set_pipeline` after registering, and  
one that wants the renderer's fallback chain clears the manager. Both `install`s  
are idempotent — a store that already holds the chain gets the same handles back  
rather than a second copy — which is what makes calling one on every generation  
safe.

**The default is a real choice, not a no-op.** The chain's geometry pass names a  
shader, so it draws only the instances whose material was built from that  
shader; a scene with materials of its own does not appear through it. That is  
the rule for every shader-naming pass (§3.5), not something the default does  
differently — but a project only notices it once the default has replaced the  
built-in chain.

### `RenderingResourcesManager` — the store-to-GPU diff

`src/rendering_resources_manager.rs`. One GPU object per live asset, and the content version it  
was built from:

| Field            | What it is                                                         |
| ---------------- | ------------------------------------------------------------------ |
| four `*_handles` | key → the GPU object, one map per asset type                       |
| four `*_versions` | key → the version that object was built from                      |
| `synced_revision` | the store revision these are level with, so a frame can skip out  |
| `resource_epoch`  | bumped when a shader or texture object is created or dropped      |
| the two defaults  | the built-in lit shader, and a material built from it             |

`build_queue` lives here for the same reason the syncs do: an instance names its  
mesh and material by *asset* key, the queue needs the *GPU* handles, and these  
caches are what turn one into the other. The renderer never indexes a handle map  
itself — it asks for the queue, and for `chain_context` when the chain needs to  
build.

### The bind group convention

Four group slots, fixed by the engine. Every shader in the engine uses them, and  
a missing one is padded with an **empty layout** so every pipeline has the same  
shape:

| Set | Index                                         | Holds                                   | Declared by                                           |
| --- | --------------------------------------------- | --------------------------------------- | ----------------------------------------------------- |
| 0   | `ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX`   | fog, frame size, time                   | `EngineParameters` — one instance, refilled per frame |
| 1   | `CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX`   | camera position, view-projection        | `RendererCamera`                                      |
| 2   | `MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX` | the material's or pass's uniform values | `RendererShader` (#2 layout)                          |
| 3   | `MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX`   | the material's or pass's textures       | `RendererShader` (#3 layout)                          |

The padding is the load-bearing part. A shader that declares no parameters still  
gets an empty layout at set 2, so the drawer can bind groups 0…3  
unconditionally and never has to ask which slots a shader uses.

### The uniform blocks

**`EngineParametersData`** — 48 bytes, one per frame, shared by every pass:

```
offset  0  vec3  fog_color          (12 bytes)
offset 12  float fog_density        ( 4 bytes)
offset 16  vec4  frame_size         xy = pixels, zw = reciprocal
offset 32  vec4  time               x = seconds, y = delta, z = frame
```

The fog pair leads because the HLSL `EngineParams` in  
`config/common_shaders/common.hlsl` declares it there — the two  
are one layout written twice, and a field added on  
one side has to be added on the other. The frame size and time are the engine  
side's extension; no shipped shader reads them yet. Note  
`update_data` clamps the dimensions before taking reciprocals, so a minimized  
surface can't leave an infinity in the buffer for a shader to read per pixel.

**`CameraParametersData`** — a `vec4` position and a 4×4 view-projection. The  
projection inputs are sanitized on every update: a field of view outside  
`(0, 180)`, a near plane ≤ 0, or a far plane at or inside near are all replaced  
with defaults, and each replacement is named. The reason is blunt — a NaN matrix  
paints a blank frame and leaves nothing to trace. Substitutions are reported once  
per change, not once per frame.

### The resource caches

Each `Renderer*` type is built during `rendering_resources_manager.sync` (whenever its asset's version  
moves) and lives in one of five slot maps.

**`RendererShader`** — the pipeline a draw binds, plus:

- `parameter_slots` — the uniforms the shader declares, by name, **in**  
**declaration order** (an `IndexMap`).
- `texture_slots` — same, each with its texture and sampler binding indices.
- `parameters_bind_group_layout` / `textures_bind_group_layout` — `Option`s,  
present only when the shader declares slots of that kind.
- `pass_engine_parameters` / `pass_camera_parameters` — flags, so a drawer binds  
groups 0 and 1 only when the shader actually reads them.

The pipeline state is fixed here and not per draw: `TriangleList`, `Ccw` front  
face, back-face culling, `Depth32Float` with `Less` comparison and depth writes  
on, one sample. A shader refusing to build is reported against the asset that  
named it, never as a panic.

**`RendererMaterial`** — two `Option<BindGroup>`s, nothing else. Notably it does  
**not** store the uniform buffer: a bind group holds what it references, so the  
buffer lives exactly as long as the group reading it.

The packing rule is worth memorising, because a pass packs identically:

> Walk the **shader's declared slots** in declaration order, and write one  
> 16-byte region per slot. A declared slot the material left unbound writes as  
> zeros.

Walking the shader's slots rather than the material's map is why a value bound  
under a name the shader never declares is ignored rather than shifting  
everything after it. Each slot is a full `vec4` for alignment, even for a bare  
scalar — which is why the built-in fragment shader reads `material.specularity.x`.

Unbound texture slots fall back to the renderer's default colour or flat-normal  
texture, created at startup with their handles kept beside the maps. A **depth**  
slot in a material is an error rather than a fill: only a pass can hand a shader  
the renderer's depth buffer.

**`RendererPass`** — what recording a pass needs: its pipeline and its own  
bind groups. Built only when a pass names a shader that loaded. Its pipeline is  
built for the **target it writes**, not the surface — a lit frame lands in a  
half-float target and the tonemapping pass lands on the swapchain, and one  
pipeline can't serve both. A fullscreen pass has no vertex layouts at all.

Geometry passes with no shader have no `RendererPass`; they are `Unshaded` and  
draw through material pipelines.

**`RendererTexture`** — always a `texture` + `texture_view` + `sampler` triple,  
because every consumer needs all three. One type covers three different things:

| Constructor         | Used for                | Format                                                  |
| ------------------- | ----------------------- | ------------------------------------------------------- |
| `new_texture`       | uploaded image assets   | `Rgba8UnormSrgb` for `Color`, `Rgba8Unorm` for `Normal` |
| `new_render_target` | offscreen chain targets | `Rgba16Float`                                           |
| `new_depth_texture` | the shared depth buffer | `Depth32Float`                                          |

The type decides the format because **the bytes alone don't say how to read**  
**them**. Colour is uploaded as sRGB so the hardware linearises it; a normal map is  
uploaded as raw linear data so its directions arrive exactly as stored. A  
`TextureType::Depth` *asset* is refused — no file decodes to a depth buffer, and  
only the renderer's own buffer can be one. `new_texture` also checks the byte  
length against the declared extent, because a mismatch would otherwise surface  
as a validation panic instead of an error naming the asset.

Offscreen targets are `Rgba16Float` on purpose: the values a lit frame produces  
run well past 1.0, and a target that can't hold them clips the picture before the  
pass that was going to bring it back into range runs. Their size is  
`surface / target_scale`, and the **first pass to name a target decides its**  
**scale** — the target belongs to the chain, not to any one pass.

**`RendererMesh`** — vertex buffer, index buffer, `index_count`. Refuses a mesh  
with no vertices or no indices (wgpu won't create a zero-sized buffer, and an  
empty mesh is an asset mistake worth naming). Its `data_layout_descriptor` is  
derived from `MeshVertex`'s field order, so the descriptor and the uploaded bytes  
can't drift apart.

**`SlotMap` / `KeyData`** — a small generational arena, now living in
`pill_core::slot_map` rather than in this crate. A handle is an `index` plus the
`version` the slot carried when the handle was issued; a lookup succeeds only
while the two still match. When a slot is reused its version is bumped, so a
stale handle stops validating instead of silently naming its successor. Each of
the five renderer handle types is stamped from `pill_core::define_slot_key!` in
the `resources/` module of the resource it names, so a type and the key that
addresses it sit together; the base crate exports the macro so any crate can
build its own handles over the same arena. The handles are re-exported
`pub(crate)` - they name slots in the renderer's own maps, so they are plumbing,
not part of what this crate exports.

### Signatures and the epoch

Two pieces of bookkeeping keep `pipeline.ensure` from rebuilding the world every  
frame:

- **`resource_epoch`** — bumped whenever a shader or texture GPU object is  
created, recreated or dropped. Those are exactly the objects a pass's bind  
groups reference by handle, so a change to one is what invalidates them.
- **The signature strings** — `chain_signature` and `offscreen_signature`  
serialize what the chain's GPU objects depend on, including pass parameters and  
the textures a pass names. Iterated in slot order, as noted, so `HashMap`  
ordering can't fake a change.

---

## Level 5 — The draw path

This is where a sorted list of integers becomes instanced draw calls.

### The packed key

`render_queue.rs` packs a draw's identity into one `u64` so the queue sorts with  
a single integer comparison:

```
bits 56-63  material rendering order, stored inverted (u8::MAX - order)
bits 48-55  shader slot index          bits 40-47  shader handle version
bits 32-39  material slot index        bits 24-31  material handle version
bits 16-23  mesh slot index            bits  8-15  mesh handle version
bits  0-7   unused, always zero
```

Ascending sort is the whole trick. Draws agreeing on shader + material + mesh  
land next to each other, which is precisely what the mesh drawer fuses into one  
instanced draw. The leading inverted order byte bands a pass by  
`Material::rendering_order` **first** — and because it's inverted, a *larger*  
order value is drawn *earlier*.

Pairing each slot index with the version its handle was issued at means a key  
names one *incarnation* of a slot: keys from before a slot was reused sort  
separately instead of aliasing.

Two caveats the code itself flags:

- **The packing is lossy.** Every field is one byte, so an index or version above  
255 truncates to its low byte. Don't read a key back as a substitute for the  
handle it was packed from.
- `RenderQueueItem`'s derived `Ord` compares `key` first, then `entity_index` —  
so instances sharing state stay in frame order within their batch.

### The instance transform

`Instance` is 36 bytes: a `Matrix3f` holding three `float3` columns —  
translation, rotation, scale.

It's only 3×3 because the translation rides in the first column and the shader  
rebuilds the model matrix from the three vectors; there's no need for a fourth  
column of `[0,0,0,1]`.

The rotation encoding has a story behind it. The vertex shader builds  
`(rot_x * rot_y) * rot_z` and multiplies vectors **row-first** (`v * M`), which  
WGSL defines as `transpose(M) * v` — so what it renders is the rotation with  
every angle negated. `Instance::new` therefore decomposes the quaternion with  
`to_euler(EulerRot::ZYX)` (matching the order the shader's multiplication spells  
out; glam returns the angles reversed) and negates them. A test pins this: it  
rebuilds what the shader effectively applies and compares against  
`Mat3::from_quat`, including past half a turn, where the previous encoding lost  
the winding.

### Two layouts, one location space

The vertex and per-instance buffers share one attribute location space, so a  
shader location belongs to exactly one of them:

| Source                              | Locations     | Contents                                 |
| ----------------------------------- | ------------- | ---------------------------------------- |
| `RendererMesh` (step mode `Vertex`) | 0, 4, 5, 6, 7 | position, UV, normal, tangent, bitangent |
| `Instance` (step mode `Instance`)   | 1, 2, 3       | translation, rotation, scale             |

The gaps at 1–3 for mesh attributes are not an accident: `slangc` maps HLSL  
semantics to locations that way (`TEXCOORD0` → 4, `NORMAL` → 5, `TANGENT` → 6,  
`BINORMAL` → 7), and the comments in `renderer_mesh.rs` say so at each attribute.

### Batching

`MeshDrawer` owns one growable instance buffer, reused every frame. A frame's  
sorted queue is split into chunks of `INSTANCE_BATCH_SIZE` (10,000), and each  
chunk gets its **own region** of the buffer:

```rust
fn batch_region(batch_index, max_batch_size, instance_count) -> Range<u64> {
    let stride = size_of::<Instance>();
    let start = (batch_index * max_batch_size * stride) as u64;
    start..start + (instance_count * stride) as u64
}
```

Why regions at all: every `write_buffer` of a frame lands *before* the command  
buffer runs, so if batches shared the front of the buffer, every draw would end  
up reading whichever batch was written last. Why whole-batch offsets: it keeps  
each region four-byte aligned, which `write_buffer` requires, and stops a  
partial trailing batch from overwriting the batch before it. Two tests pin  
exactly those two properties. Capacity grows in whole batches, so a frame one  
instance over the limit reallocates once rather than every frame after.

### Recording

Per batch:

1. Stage the batch's `Instance`s into a `Vec`.
2. `write_buffer` that region.
3. `set_vertex_buffer(1, region)` — slot 1, slot 0 being the mesh.
4. Walk the sorted items, **decomposing each key back into handles**:

```rust
let shader = RendererShaderHandle::new(
    fields.shader_index.into(),
    NonZeroU32::new(fields.shader_version.into()).unwrap(),
);
```

1. On a shader, material or mesh change, flush whatever instances have
  accumulated and rebind. `DrawingContext` is the accumulator: it holds the  
   currently bound handles and the instance range gathered since the last draw.

The upshot is that a draw is recorded **only where state changes or a batch**  
**ends**. Runs that agree on all three handles become single instanced draws, and  
state deliberately carries across batch boundaries — a shader shared by two  
adjacent batches isn't rebound just because the batch ended.

Each recorded draw logs a telemetry line with the batch number, instance range,  
and the shader/material/mesh names, which is what makes a mis-batched frame  
traceable from the log alone.

---

## Level 6 — What holds it together

### Errors: never crash the host over an asset

wgpu reports a refused pipeline, bind group or texture through its  
**uncaptured-error handler**, which panics by default — and there is no return  
value to check. So a driver refusal would arrive as a crash rather than as a  
mistake to report.

`capturing_validation` fixes that:

```rust
device.push_error_scope(ErrorFilter::Validation);
device.push_error_scope(ErrorFilter::OutOfMemory);
let value = make();
// scopes are a stack: last pushed, first popped
let out_of_memory = pollster::block_on(device.pop_error_scope());
let validation   = pollster::block_on(device.pop_error_scope());
```

Every creation path in the crate goes through it, so a failure arrives as a  
message naming the asset, pass or shader that asked for the work. That is the  
difference between *"the host died"* and *"this pass is not drawn, and here is*  
*why"*.

The posture throughout is: **create-time failures are reported and skipped, not**  
**fatal.** A shader won't build → its pass is named once and skipped. A texture  
won't upload → logged, left unrecorded so a later revision retries. A mesh has no  
geometry → refused by name. One bad asset doesn't hold the rest of the revision  
hostage, and doesn't get retried every frame.

`RendererError` carries plain strings rather than wgpu error values, so a host  
linking this crate doesn't have to name a graphics type to report a renderer  
failure.

### Diagnostics

The renderer prints its once-per-change warnings with `println!("[render] …")`  
rather than through the log target, and the comments explain why: the host  
filters that target out. These are the lines worth knowing about when a window  
shows the wrong thing:

```
[render] Pass chain: shadow(0) opaque(1204) tonemap(1)
[render] Pass <name> is not drawn: <reason>
[render] The pipeline the manager holds no longer resolves; running the built-in pass
[render] Pass chain: N of M pass handles no longer resolve and are left out
[render] Pass chain: 2 surface pass(es) at [0, 3]; only the last pass's writes reach the window
[render] texture `<name>` is not uploaded: <error>
[render] Pass <name> binds texture `<slot>`, which is not loaded; the shader's default is used
```

The chain line reports each pass with its draw count, which is the one thing  
that tells a chain doing what the game asked apart from one quietly falling back  
to the built-in pass. A line per frame would bury everything else, and an  
unchanged chain is the normal case.

### Profiling

`profiler.rs` adds GPU-side measurement — timestamp queries, occlusion queries,  
pipeline statistics. The design points:

- Query sets are allocated **once** with a per-frame cap.
- Each frame resolves into one slot of a **three-deep ring**; the blocking  
readers take the slot from one frame back, so a readback never waits on the  
frame being recorded.
- Every capability is optional — a device without the timestamp or pipeline  
statistics features just gets `None` where a query set would be.

### Reload

Two things make the crate survive a project reload:

- **`register()` is idempotent.** Every step checks whether the thing it's about  
to install already exists. That matters because a reloaded project artifact  
calls it again each generation, and asset columns outlive the reload.
- **`shared_name()` on `RenderFrame` and `RenderingManager`.** The host binary  
and a project module are separate compilation units; the shared name is the  
string both sides agree on so a resource written by one is the resource read by  
the other. Drop it and the renderer silently reads a resource the project never  
wrote.

`invalidate_assets()` is the escape hatch on the diffing scheme: it clears every  
version record so the next sync rebuilds every GPU object from the store. Use  
it when the asset set was replaced wholesale and the content-version diff can no  
longer be trusted.

### The shaders

Four directories hold stage sources, so `build.rs` runs the `pill_assets`  
pipeline over four roots. The rule matches one level below the directory it is  
given — `<root>/shaders/*.hlsl`, or `*.hlsl` where the root is itself the  
shaders directory — never a search, so each tree takes a call of its own:

| Root                              | What it holds                                        |
| --------------------------------- | ---------------------------------------------------- |
| `src/config/common_shaders/`      | the vertex stage every lit pass starts from          |
| `src/config/simple_pipeline/`     | the built-in lit fragment stage                      |
| `src/config/pbr_pipeline/`        | the geometry fragment stage                          |
| `src/config/post_processing/`     | the fullscreen vertex stage and the four fragment stages |

Nothing compiles a shader at runtime — wgpu parses the cooked WGSL through naga  
like any other `ShaderSource::Wgsl`.

The lit pair has three readers, which is why its vertex stage is not private to  
the pipeline that names it: `simple_pipeline` and the renderer's fallback material  
are built from the same pair, and the PBR geometry pass reuses the vertex stage —  
every lit pass in the crate starts from the same instance layout.

The six chain sources were adopted from the project that first defined the chain,  
which is why the crate owns them now: a pipeline the renderer installs by itself  
cannot load its shaders from somebody else's `res/`, so they are cooked in and  
embedded instead. They split along the seam the passes already have — one  
fragment stage for the lit pass, five stages for the four fullscreen ones.

`config/common_shaders/` is the one flat tree: it is not any pipeline's, and a  
shared directory holding a couple of stages does not need a `shaders/` level to  
keep them apart. Its header is not beside them for the same reason a header never  
sits in a matched directory — the rule refuses a name that declares no stage  
rather than skipping it. So `common.hlsl` is in `common_shaders/include/`, the one  
directory beside the sources no glob reaches, and `slangc -I` is handed it so  
`#include "common.hlsl"` resolves from every tree.

The built-in pair is a working example of the conventions:

- `default_vertex.hlsl` — builds the model matrix from the instance's three  
vectors, computes a normal matrix as `transpose(inverse(model3x3))`, and packs  
the TBN basis into three output **rows** so the fragment stage can rebuild the  
matrix from three locations.
- `default_lit_fragment.hlsl` — Lambert diffuse plus Blinn-Phong specular from a  
hard-coded point light, normal-mapped, with exp-squared depth fog from  
`engine.fog_*`. Note `MaterialParams` declaring two `float4`s with the comment  
explaining why: a `float3` followed by a `float` would put the scalar at offset  
12, inside the colour's own 16-byte slot, where it reads padding.

---

## Level 7 — Getting into the code

### Suggested reading order

`renderer.rs` is the worst place to start, and `resources/` is the largest — the
per-kind GPU caches. Work from the frame outward:

1. `lib.rs` — the crate's own summary, then `register()`.
2. `api.rs` — the contract and the headless stub. Short.
3. `components/` — what a scene *is*. One file per component; read `mod.rs`
  first for `register_components`, then the component you care about.
4. `frame.rs` — how a scene becomes a frame. Together with 3, this is the data
  model, and it's the level where the architecture is legible.
5. `assets/mod.rs`, then whichever of `mesh.rs` / `material.rs` / `shader.rs` /
  `render_pass.rs` you need. Read `render_pass.rs` before  
   `rendering_pipeline.rs`.
6. `render_queue.rs` — small, and §5 above makes it click.
7. `surface.rs` — the swapchain and the GPU bootstrap. Self-contained, and it's
  the only file that needs a window to make sense of.
8. `pipeline.rs` — the chain as GPU objects. `ensure`'s two gates and `plan`'s
  per-pass decisions are the two things to read closely; the module header
  explains why the gates are tuples rather than signatures.
9. `rendering_resources_manager.rs` — the store-to-GPU diff. Read `sync` first for the order,
  then any one `sync_*` in full: they are all the same shape.
10. `renderer.rs` — now read `Renderer::render` (≈ line 183) top to bottom, then
  `State::render`. By this point it is a skeleton, which is the point.
11. `config/` — the shipped pipelines, each with its shaders beside it.
  `pbr_pipeline::install` is the worked example of building a chain out of
  assets; `simple_pipeline` is the smallest version of the same thing, and the
  pair is what `register` chooses between.
12. `drawers/mesh_drawer.rs` — where the sorted queue becomes draw calls.

Every module opens with a `# Responsibilities` / `# Design` header. Reading  
just those across `src/` gives you the architecture in about ten minutes; the  
inline comments are denser and explain *why*, which is where the surprising  
decisions live.

### Building and running it

This is a Windows-hosted project. Build, run and test on the Windows box over  
SSH, not in this Linux container:

```bash
ssh HIST0R-ONE 'cd /d D:\Programming\Rust-Hybrid-ECS && python devops\tools\pill_dev.py status'
ssh HIST0R-ONE 'cd /d D:\Programming\Rust-Hybrid-ECS && python devops\tools\pill_dev.py cycle'
```

`pill_dev.py --help` lists the rest (build, run, stop, log, tests). The same  
actions are one-click entries in the "Power Glove" command palette.

Related material elsewhere in the repo:

- `examples/master_renderer_test/` — the PBR test project, and the fullest  
example of a project driving this renderer end to end. Its chain is five  
passes over four named offscreen targets (`hdr`, `bloom`, `lit`, `ldr`):  
opaque → bloom prefilter → bloom composite → tonemap → lens.
- `examples/project_rs/` — the minimal scene setup.
- `devops/tests/test_pbr_native.py`, `devops/tests/test_renderer_assets.py` —  
the renderer-specific suites.
- `pill_2d_renderer/`, `pill_embedded_renderer/` — the sibling backends, for  
contrast.

### Glossary

| Term                | Means                                                                                                         |
| ------------------- | ------------------------------------------------------------------------------------------------------------- |
| **asset key**       | a `u64` packing a handle's index and generation: how the renderer names an asset without holding a handle     |
| **content version** | per-asset counter; the diff basis for re-uploading GPU objects                                                |
| **revision**        | the `AssetManager`'s global counter; gates the renderer's per-frame asset sync                                |
| **store**           | the world's `AssetManager`; the renderer reads it directly rather than a copy riding in the frame              |
| **chain**           | the resolved list of passes a frame runs, from the pipeline the game set                                      |
| **built-in pass**   | the fallback chain: one geometry pass over the surface, no shader of its own                                  |
| **unshaded**        | a geometry pass with no shader: instances draw through their own material's pipeline                          |
| **slot**            | one entry in a `SlotMap`; identified by index **and** version                                                 |
| **epoch**           | `resource_epoch`: bumped when a shader or texture object is replaced, because pass bind groups reference them |
| **target**          | an offscreen colour texture a pass writes and a later pass reads, named by the chain                          |
| **batch**           | one chunk of the sorted queue, up to `INSTANCE_BATCH_SIZE`, with its own region of the instance buffer        |
| **packing**         | walking a shader's declared slots in order and writing one 16-byte uniform region each                        |
| **cooked**          | WGSL generated from HLSL at build time, by `pill_assets`                                                      |
