# Compilation & Hot-Reload Pipeline

This document explains how crates in this workspace get built, linked, and
loaded, why two builds of the "same" crate can silently overwrite each other
on disk, and what that means for which modules can safely be hot-loadable,
directly linked, or both.

## 1. The workspace and what shares it

`modules/Cargo.toml` defines one Cargo workspace:

```
members = [
    "pill_editor", "pill_engine", "pill_core", "pill_core_macros",
    "pill_host", "pill_standalone", "pill_build_support",
    "optional/*",   # globbed: every crate directory under optional/
]
```

Everything under `modules/optional/*` is a workspace member automatically —
that's how a new module needs no manifest edit to be discovered. This
includes the real optional modules (`pill_spline`, `pill_dummy_math`, etc.)
**and** a generated member, `optional/host_project_project`, which is a copy
of the actual project's manifest (`examples/project_rs/Cargo.toml`) with every
relative path rewritten to an absolute one and its `[lib] path` pointed at the
real `examples/project_rs/src/lib.rs`. This generated member is what lets the
project compile *as a workspace member* — sharing one `Cargo.lock`, one
`target/` directory, and one dependency-resolution graph with every optional
module — which is required for type identity (see §4).

`examples/project_rs` itself is **not** a workspace member on disk; the host
regenerates `optional/host_project_project/Cargo.toml` fresh on every startup
(`materialize_host_project_member` in `pill_host/src/config.rs`), so it always
reflects whatever `project_rs/Cargo.toml` currently says.

One consequence that matters a lot below: **everything in this list shares
one `target/debug/` directory.** A `cdylib` crate's build output always lands
at `target/debug/<crate-name>.dll` (or `lib<name>.so`/`.dylib`), no matter
which command triggered the build or which other packages were built
alongside it. There is exactly one filesystem slot per crate name.

## 2. `-C prefer-dynamic` — what it actually does, verified against the rustc source

`modules/.cargo/config.toml` sets `rustflags = ["-C", "prefer-dynamic"]` for
every build inside this workspace, with a comment claiming this shares one
copy of `pill_engine`/`pill_core` across every crate. **That comment
overstates what the flag actually does**, confirmed by reading rustc's own
linking algorithm
([`dependency_format.rs`](https://github.com/rust-lang/rust/blob/main/compiler/rustc_metadata/src/dependency_format.rs))
and by direct experiment with `dumpbin` on this workspace's real build
output:

> With `prefer-dynamic` set, the compiler's initial preference is dynamic
> linkage — but it then **attempts static linking first**, for the whole
> dependency graph. Only if static linking is impossible (some dependency has
> no `.rlib` available at all) does it fall through to the dynamic path and
> "greedily maximize the number of dynamically linked libraries."

Since almost every crate in this workspace declares `crate-type = ["rlib",
"dylib"]` (or `["cdylib", "rlib"]`), an `.rlib` is *always* available, so a
pure-static solution to the whole graph always exists — and rustc takes it.
**`prefer-dynamic` therefore does not force any particular dependency to be
shared at runtime; it only changes the outcome when static linking would
otherwise be impossible.**

What this means concretely, verified by building this workspace and
inspecting the actual output:

- `pill_engine` is `crate-type = ["rlib"]` **only** — deliberately, per its
  own doc comment: a `dylib` build with the `rendering` feature would export
  more symbols than Windows' 65535-symbol DLL export limit allows
  (`LNK1189`). There is no `pill_engine.dll` at all; every crate that uses
  `pill_engine` always statically embeds its code, with no alternative. This
  is not a bug or an oversight — it's an explicit design constraint, and it
  means every process/DLL that links `pill_engine` carries its own private
  copy of that code (though not its own copy of `pill_core`'s statics — see
  below).
- `pill_core` is `crate-type = ["rlib", "dylib"]`. Because an `.rlib` is
  always available for it too, the "static-first" rule above means most
  builds **also** statically embed `pill_core`, not share it — confirmed with
  `dumpbin /exports`/`/imports` on `pill_dummy_color.dll`, `pill_spline.dll`,
  and `project.dll`: none of them import `pill_core.dll` at all, meaning each
  carries its own embedded copy of `pill_core`'s code, statics, and tracing
  dispatcher. Only `pill_standalone.exe`, the final host binary, dynamically
  links `pill_core.dll` in practice — the exact reason a `bin` target ends up
  on a different path through the algorithm than a `cdylib` target wasn't
  fully pinned down, but the empirical result was consistent across repeated
  clean rebuilds.
- **The practical upshot**: the "one shared engine copy across host and every
  loaded module" goal the `.cargo/config.toml` comment describes is **not
  actually happening** for modules today. Each loaded module's `.dll`
  contains its own private copy of `pill_core`'s statics and tracing
  dispatcher, separate from the host's and from every other module's. This
  predates everything done in this session and is a separate, real question
  worth its own investigation — it is not something this document's fixes
  address.

### Forcing genuine dynamic linking: drop `rlib` entirely

The only way to make rustc actually choose the dynamic path for a specific
dependency is to make the static path **impossible** for it — i.e., give that
one crate `crate-type = ["dylib"]` with no `rlib` at all. This was verified
directly: `pill_color_core` (see §7) was built once as `["dylib", "rlib"]`
(dependents statically embedded it — the compiled `pill_spline.dll` grew by
exactly `pill_color_core`'s code size and adding/removing
`pill_color_core.dll` from disk made no difference to loading `pill_spline.dll`)
and once as `["dylib"]` only. After the second change, deleting
`pill_color_core.dll` and attempting to `LoadLibrary` the *unchanged*
`pill_spline.dll` failed with Windows error 126 ("the specified module could
not be found") — proof the dependency is now genuinely resolved at load time,
not embedded. Curiously, `dumpbin /dependents` and `/imports` still don't list
`pill_color_core.dll` in `pill_spline.dll`'s standard import table even in
the dylib-only case; Rust's dylib-to-dylib linking on Windows apparently uses
a mechanism `dumpbin`'s default views don't fully surface, so the
`LoadLibrary` failure test is the reliable way to confirm this, not
`dumpbin` alone.

The trade-off: a `dylib`-only crate cannot be statically linked at all
afterward, including by a monolithic/release build that clears
`prefer-dynamic` (see the note already in `.cargo/config.toml`). That's fine
for a crate like `pill_color_core` that nothing outside this workspace needs,
but would not be a safe change to make to `pill_core` itself without checking
every consumer, including release builds.

`pill_spline`, `pill_dummy_math`, and the other optional-module crates use
`crate-type = ["cdylib", "rlib"]`. `cdylib` is a C-ABI-exporting shared
library meant to be *loaded* (`dlopen`/`LoadLibrary`), not linked against by
other Rust crates at compile time — and Cargo refuses outright to combine
`dylib` and `cdylib` on one crate ("cannot set the crate type of both dylib
and cdylib" is a hard manifest error, not just a naming collision). So a
crate that needs to be both hot-loadable (`cdylib`) and a genuine shared
dynamic dependency for other Rust code (`dylib`) cannot be one crate — see §7
for how this was resolved for `pill_spline` → `pill_dummy_color`.

## 3. The `module-abi` feature and why it exists

Every optional-module crate exports three fixed `#[no_mangle] extern "C"`
functions the host looks up by name at runtime through `LoadLibrary` /
`GetProcAddress` (Windows) or the equivalent on other platforms:

- `pill_module_abi_version() -> u32`
- `pill_module_name() -> *const c_char`
- `pill_module_init(*const EngineApi) -> u32`

These are gated behind a Cargo feature, `module-abi`, **on by default**:

```toml
[features]
default = ["module-abi"]
module-abi = []
```

The reason: if `project_rs` links a module crate **directly** as an ordinary
Rust dependency (to call its Rust functions/types at compile time, like
`Spline::from_points(...)`), that module's `#[no_mangle]` exports get pulled
into `project.dll` too. Two crates directly linked into the same `cdylib`
that both export a symbol named `pill_module_init` is a **linker error**
(`LNK2005: symbol already defined`) — the linker cannot have two functions
with the same exported name in one binary. This is exactly what happened the
first time `project_rs` linked more than one dummy module directly: five
crates' worth of `pill_module_init`/`_name`/`_abi_version` collided.

The fix already in place: any crate `project_rs` links directly is required
with `default-features = false`, stripping its ABI exports for *that build
only*, while the crate's ABI exports stay on by default for its *standalone*
build (the one the host hot-loads via `pill_config.yaml`).

**This is the part that only works when the two builds land in different
places.** See §5.

## 4. `TypeId`, shared metadata, and why the workspace-member trick matters

`pill_engine`'s components are keyed partly by `std::any::TypeId` (see
`pill_engine/src/component.rs`) and, for persistable components, also by a
stable string (`std::any::type_name::<T>()`) plus a schema hash
(`pill_engine/src/world.rs`). `TypeId` is derived from a hash that includes
the compiling crate's own metadata fingerprint — which is itself influenced
by the resolved dependency graph, enabled features, and compiler flags used
for that specific build. Two *separately compiled* copies of "the same" Rust
type — even from identical source, if compiled with different dependency
graphs, feature sets, or as separate crate instances — get **different**
`TypeId`s and are treated as unrelated types by anything that keys off
`TypeId` equality.

This is why:

- The project must compile *inside this workspace* (via the generated
  `host_project_project` member) rather than as a free-standing crate with
  its own lockfile: only then does it resolve `pill_engine`, `pill_core`, and
  any directly-linked module crate through the *exact same* dependency graph
  as everything else built here, keeping `TypeId`s consistent.
- A module that the project links directly (e.g. `pill_spline` for the
  `Spline` type) must be **the same compiled artifact** on both sides of any
  code that compares component types — which the shared-workspace,
  shared-lockfile setup already guarantees, *provided* nothing else disturbs
  it (see §5 for how it currently can be disturbed).
- Persistable components are matched across a hot reload by **stable type
  name + schema hash**, not by `TypeId` — that's specifically what lets a
  freshly recompiled module's component type "reconnect" to the same
  archetype storage the previous generation used, even though hot-reloading
  necessarily produces a new compiled artifact (and therefore a new
  `TypeId`) every time.

## 5. The collision: one crate name, two required feature sets, one output path

Here is the mechanism that broke things, confirmed by direct reproduction
with `dumpbin /exports`:

A crate like `pill_dummy_math` needs to exist as **two logically different
builds**:

1. **Standalone, `module-abi` ON** — built by `cargo build --package
   pill_dummy_math` when the host loads it as an independent optional module
   via `pill_config.yaml`. This build exports `pill_module_init` etc.
2. **As a dependency of `project`, `module-abi` OFF** — built as part of
   `cargo build --package project`, because `project_rs`'s manifest requests
   `pill_dummy_math = { ..., default-features = false }`. This build does
   **not** export those symbols (correctly — two of them together in
   `project.dll` would be a linker error, per §3).

Both builds are for a crate named `pill_dummy_math`, in the **same
workspace**, writing to the **same `target/debug/pill_dummy_math.dll`**.
Whichever one ran more recently is what's sitting at that path afterward —
there is only one filesystem slot per crate name, and Cargo has no concept of
"the same crate name but two different artifacts for two different
purposes." Building `project` after building the standalone modules silently
overwrites every directly-linked module's standalone `.dll` with the
ABI-stripped variant, even though the module wasn't named on `project`'s
build command line at all — it's pulled in transitively as project's
dependency, compiled with project's requested feature set, and its `cdylib`
output copy step still writes to that module's own canonical output path.

This was reproduced with the *original*, non-batched, one-`cargo`-invocation-
per-module code — it is **not** something introduced by the batching/
parallelization/skip-check performance work; it was latent from the moment
any module crate became both hot-loadable *and* a direct dependency of the
project with a conflicting feature requirement. Verified with `dumpbin
/exports`: after building `project`, every one of the five dummy modules'
`target/debug/<name>.dll` files had **zero** `pill_module_*` exports, matching
the `project`-linked (`module-abi` off) build rather than the standalone one.

**The practical rule this implies:** *a crate cannot currently be both listed
in `pill_config.yaml`'s `modules:` and directly depended on by `project_rs`
(or by another optional module also built in this workspace) with a different
feature resolution.* Whichever build happens last wins, non-deterministically
from the host's perspective (it depends on build order and mtime-based
skip-checks, not on anything the config file expresses).

## 6. What can safely link to what, today

| Relationship | Safe? | Why |
|---|---|---|
| Project links a module crate directly (Rust-level, e.g. `Spline::from_points`) | Yes | Requires `default-features = false` on that dependency edge to avoid the `project.dll` multi-export linker error (§3). |
| Two directly-linked module crates, neither depending on the other | Yes | No shared crate name needs two different feature resolutions. |
| One module crate depends on another module crate (e.g. `pill_spline` → `pill_dummy_color`) | Yes, **as long as the depended-on crate is never separately listed in `pill_config.yaml`** | The dependency edge needs `default-features = false` on the shared crate; if that same crate is *also* built standalone with the feature on, its output path collides (§5). |
| A crate is both project-linked (`default-features = false`) **and** listed in `pill_config.yaml` (`module-abi` on by default) | **No** | This is exactly the §5 collision: same output path, two required feature sets, last build wins. |
| Two crates hot-loaded from `pill_config.yaml`, unrelated to each other and to the project | Yes | No shared crate name, nothing to unify. |
| The host builds `pill_config.yaml`'s modules in one batched `cargo build -p a -p b ...` invocation | Yes, **only if no member of that batch is a build dependency of another member** | Batching two packages in one invocation unifies their shared dependencies' features across the whole invocation, which can silently re-enable a feature one side needed off (a milder version of §5, inside a single command rather than across two). The host detects this by checking each candidate module's `Cargo.toml` for a dependency on another module in the same batch and splits them into separate batches when found. |
| The project's build and an optional module's standalone build run as two genuinely separate `cargo build -p X` invocations against the same `target/debug`, for a crate name used only one way | Yes | No feature disagreement exists for that crate name, so there's nothing to overwrite inconsistently. |

Given the table, the modules created in this session (`pill_spline` +
`pill_dummy_math/text/color/timer/random`) are all currently **project-linked
only**. None of them can also be listed in `pill_config.yaml` without
reintroducing the §5 collision, because `project_rs` depends on every one of
them directly. `pill_config.yaml`'s `modules:` list should stay empty (or
only name a *different* crate that nothing else depends on) until one of the
two structural fixes below is applied.

## 7. Sharing code between modules without static embedding: `pill_color_core`

`pill_spline` needed `pill_dummy_color`'s `get_color_a` logic. Depending on
`pill_dummy_color` directly (as tried first) meant statically embedding a
`cdylib` crate's code — including its `module-abi`-gated `#[no_mangle]`
exports when that feature happened to be enabled for the build — which is
exactly the kind of embedding that caused the §5 collision once two
differently-featured builds of the same crate needed to coexist.

The fix: a new crate, `pill_color_core` (`modules/pill_color_core/`), holding
just the shared logic, with no `cdylib` output and no ABI surface to gate:

```toml
[lib]
crate-type = ["dylib"]   # no "rlib" — see §2's "forcing genuine dynamic linking"
```

Both `pill_dummy_color` and `pill_spline` depend on it as an ordinary Rust
crate. Because it's `dylib`-only, static linking is impossible for it, so
every dependent genuinely dynamically links one shared `pill_color_core.dll`
— verified with the `LoadLibrary`-failure test in §2, not just `dumpbin`.
This is the general pattern for sharing logic between module crates (or
between a module and the project) going forward: put the shared logic in its
own `dylib`-only crate rather than depending on another `cdylib` module
directly, and there is no `module-abi`-style feature gymnastics needed for it
at all, since a plain shared-logic crate has no ABI surface to collide with
anything.

**This solves code-sharing between modules. It does not solve the §5 project-
linking collision** — `pill_spline` and every dummy module are still each
individually a direct `project_rs` dependency with `default-features =
false`, which is unrelated to whether they also depend on
`pill_color_core`. §9 below is still the open question for that.

## 8. Does a `cargo build -p X` need to rebuild everything, or can it reuse existing artifacts?

Cargo's normal incremental model applies: a build reuses any unit (crate
compiled with a specific feature/flag fingerprint) whose fingerprint already
matches something cached in `target/debug/deps/`, and only recompiles units
whose fingerprint changed (source edited, dependency's fingerprint changed,
features differ, etc.). Building `pill_dummy_math` alone does **not** require
rebuilding `pill_spline` or unrelated crates — Cargo's dependency graph
already scopes `-p <name>` to exactly that package plus whatever it actually
depends on.

The catch relevant to this document: **"fingerprint changed" includes enabled
features.** `pill_dummy_math` built with `module-abi` on and the *same crate*
built with `module-abi` off are two different fingerprints, hence two
different cached units — but only **one** of them can occupy
`target/debug/pill_dummy_math.dll` at a time, because that final copy step is
keyed by crate name, not by fingerprint. Cargo is completely correct and
consistent about *which unit it uses to satisfy a given build graph*; the
inconsistency only appears in the **copied-out final artifact path**, which
is shared and un-versioned by feature set. This is also why the fixed
per-invocation overhead described in the earlier performance investigation
(profiling showed ~1-1.5 seconds of pure metadata/lockfile-resolution
overhead per `cargo` process against this workspace's 754-package lockfile)
is unavoidable per invocation regardless of how small the requested package
is: Cargo always resolves the *whole* workspace graph before narrowing to the
requested package, even when nothing in that narrower scope changed.

## 9. Two ways forward for the still-open §5 collision (not yet decided)

1. **Keep every module single-purpose.** A module is either project-linked
   (like all 6 are today) or `pill_config.yaml`-hot-loadable, never both.
   Simplest, zero further code changes, but the "prove a module is
   independently hot-reloadable" demo from earlier in this session can't
   coexist with "project calls the module's functions directly" for the same
   crate — you'd need a *different* crate for each purpose, or accept picking
   one.
2. **Give the two builds separate output identities.** For example, build the
   standalone hot-loadable copy into a distinct `--target-dir`, or under a
   renamed output artifact, so `project`'s build and the standalone module
   build never share one filesystem path. This lets one crate serve both
   roles safely, at the cost of a more involved change to
   `OptionalModuleConfig`'s output-path resolution, roughly double the disk
   space for any dual-purpose crate's build outputs, and a `--target-dir`
   switch loses Cargo's cross-invocation cache sharing for that copy (so the
   dedicated hot-load build would always compile from scratch on first use of
   the new directory, though normal incremental caching still applies to it
   afterward).
