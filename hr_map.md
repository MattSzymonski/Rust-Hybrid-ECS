# Hot Reloading - Code Map

Where the hot-reloading code lives, what each file owns, and how the pieces
reach each other. Written for someone opening the subsystem cold.

The single most useful thing to know first: this is **two independent
mechanisms sharing one trigger**. Almost every question about the code resolves
once you know which of the two you are looking at.

---

## The two mechanisms

**Whole-artifact reload.** Rebuild a DLL, load the new image alongside the old
one, re-register its types and systems, migrate persisted state, retire the
previous image. Mature, handles structural change including schema migration,
and costs seconds.

**Live patching** (the `hot_patch` cargo feature). Compile one edited function
into a small DLL and redirect callers to it. Around 400 ms, but it only accepts
a body-only edit and refuses anything structural.

Both are driven by the same `AtomicU64` generation counter that the file watcher
bumps. Each frame the host tries patching first; whatever patching refuses falls
straight through to a reload, so the worst case is the behaviour that existed
before patching was added.

---

## The crates

| Crate | Role | Size |
|---|---|---|
| `pill_host` | All orchestration: watch, build, load, register, migrate, retire, patch | ~8,400 lines |
| `pill_engine/src/hot_patch.rs` | The runtime half: dispatch slots, the system registry, and the Windows prologue-patching primitives | 1,843 |
| `pill_hot_scan` | A dependency-free Rust source scanner, shared by the host and by participating crates' build scripts | 1,530 |
| `pill_engine_macros` | The attributes: `#[pill_hot]`, `#[pill_hot_fn]`, `#[pill_module]`, `#[pill_project]`, `pill_hot_resolver!` | 1,000 |

`pill_engine/src/persistence.rs` (1,363) and `pill_engine/src/world.rs` are not
hot-reload code as such, but the reload path depends on them for schema
migration, forgotten-type handling and column re-homing.

---

## File by file

### Shared trigger

**`pill_host/src/watcher.rs`** (409)
Spawns two threads per watched directory: `notify`'s own
`ReadDirectoryChangesWatcher`, and a debounce worker that owns the watcher handle
and coalesces a burst of editor writes. Bumps the generation counter with
`Release` ordering. Filters out build output, hidden files and editor temporary
files. Depends on nothing else in the host, which is why it is easy to reason
about.

**`pill_host/src/runtime.rs`** (1,121)
The frame loop, and the only place that sequences everything else. Reads the
counter with `Acquire`, decides patch-or-reload, drives module reloads before the
project reload, processes rollback requests, and prints the per-frame report.
Every other module in the host is called from here.

### Reload path

**`pill_host/src/build_runner.rs`** (1,296)
Runs cargo, and decides whether it has to at all using three independent signals:
an artifact stamp (build command plus each artifact's size and mtime), a
toolchain and feature marker, and source mtimes followed through path
dependencies. Stages build output into `target/hot/` so another `cargo build`
writing the shared per-crate slot cannot corrupt what the host loads. Also stages
the shared dependency rlibs for patch linking, and cancels an in-flight build
when a newer save arrives.

**`pill_host/src/native_library.rs`** (594)
Copies the staged artifact to a per-process temporary path before
`LoadLibrary`, because Windows locks a mapped DLL and the next build must be
able to overwrite the original. Resolves the module ABI exports, copies the
function pointers out of the borrowed `Symbol` wrappers, and on `Drop` unmaps the
image before deleting its temporary file - in that order, because Windows refuses
to delete a mapped file.

**`pill_host/src/optional_module.rs`** (567) and
**`pill_host/src/project_module.rs`** (461)
The lifecycle of one loaded artifact. Both follow the same sequence, and the
order is load-bearing:

1. build and stage, then load the new image alongside the old one
2. `init` the new generation, capturing what it registered
3. detect persistable types the new generation stopped registering, and drop
   their columns **while the generation that created them is still mapped**
4. `rehome_native_columns` - re-point every native column's function table at a
   still-mapped generation
5. migrate schemas that changed, matching types by stable name rather than
   `ComponentId`, which differs between generations
6. push the retired image into the graveyard, evicting anything older than
   `MAX_GRAVEYARD_GENERATIONS = 2`

Steps 3 and 4 are what make step 6 safe. If `init` returns non-zero, the module
rolls back by re-running the previous generation's `init`, which is required to
be idempotent.

### Patch path

All of this is behind `#[cfg(feature = "hot_patch")]`, and a release build
refuses to compile it at all.

**`pill_host/src/hot_patch/mod.rs`** (2,990)
The largest file in the subsystem. Owns `HotPatchSession`, one per crate:
classify an edit as body-only, generate the patch source, load the compiled
patch, install the replacement, and keep the generation history that rollback
walks. Also owns the fan-out - a patch is installed into every loaded artifact
that carries a copy of the function, plus every previously loaded patch.

**`pill_host/src/hot_patch/compile.rs`** (996)
Captures cargo's own `rustc` command line for a crate and replays it for the
generated patch, so the patch is compiled against exactly the artifacts the
running module linked. That is what keeps `TypeId` identical across the
boundary. Contains the tokenizer that reads cargo's quoting, and the on-disk
cache of captured flags.

**`pill_host/src/hot_patch/source.rs`** (12)
`pub use pill_hot_scan::*`. It exists so the host's patch code can say `source::`
and stay readable.

### Cross-cutting

**`pill_host/src/analytics.rs`** (1,546)
Every timing and size line the host prints, for both mechanisms. Called from
`build_runner`, `native_library`, both module types and `hot_patch`. Reads
cargo's `--timings` reports to attribute build time per crate.

**`pill_host/src/config.rs`** (1,295)
Reads `modules/pill_config.yaml`, which is the complete answer to which project
runs and which optional modules load - and therefore what gets watched.

---

## How they interact

```
watcher thread ──bump──> AtomicU64 ──read──> runtime::run_one_frame
                                                    │
                          ┌─────────────────────────┴──────────────────┐
                          │ try patching first                         │ otherwise
                          ▼                                            ▼
              hot_patch/mod.rs                                  build_runner
                │  classify   ── pill_hot_scan                    │ cargo, stamp,
                │  generate                                       │ stage, cancel
                │  compile    ── hot_patch/compile.rs              ▼
                │  load                                      native_library
                │  install ─┬─ engine registry ─ pill_engine::hot_patch
                │           ├─ per-artifact slot ─ pill_hot_resolve_install
                │           └─ prologue ────────── patch_prologue (rewrites code)
                │                                                  │
                └──────────────────────────> analytics <───────────┴─ optional_module
                                                                      project_module
```

Internal call edges inside `pill_host`, as they actually are:

| Module | Calls |
|---|---|
| `runtime` | everything else |
| `optional_module`, `project_module` | `build_runner`, `native_library`, `analytics` |
| `hot_patch/mod` | `build_runner`, `native_library`, `project_module`, `analytics`, `console` |
| `build_runner`, `native_library` | `analytics` |
| `analytics` | `console` |
| `watcher` | nothing |

---

## The three install routes

The route decides what guarantee the edit gets, which is why the analytics line
now reports it.

| Route | Chosen when | How it installs | Guarantee |
|---|---|---|---|
| Engine registry | `#[pill_hot]` system | `Engine::hot_patch` | Provable. One registry per process, one atomic pointer store |
| Per-artifact slot | `#[pill_hot_fn]` | `pill_hot_resolve_install` in every artifact | Provable. One atomic store per copy |
| Prologue | Anything else | 12-byte absolute jump written over the function | Best effort. Cannot reach a caller that inlined the body, and the write is not atomic |

The third route needs no annotation at all, which is the point of it - but it
needs the crate to carry an address inventory (below).

---

## The DLL boundary

Every loaded artifact exports these, and the host resolves them by name:

```
pill_module_init            module ABI entry point
pill_module_update          optional per-frame hook
pill_module_abi_version     version negotiation

pill_hot_resolve_install    install a replacement into a per-artifact slot
pill_hot_resolve_reset      put the original back
pill_hot_resolve_address    resolve a function's address by qualified name
pill_hot_resolve_extent_coverage   diagnostics for the prologue route
```

A generated patch exports its own, distinct set:

```
pill_patch_address          where this patch's new body is
pill_patch_resolve_*        so a LATER patch can reach the copies this one linked
```

That last one is what makes patches compose: a patch links its own copy of
everything its body calls, and without the resolver a chain of hot functions
would freeze at whatever the callee looked like when the caller was compiled.

You do not write any of these by hand - `#[pill_module]` and `#[pill_project]`
emit them, via `pill_hot_resolver!`.

---

## Who participates, and how a crate opts in

Only two optional modules currently carry the address inventory:
`optional/pill_spline` and `optional/pill_dummy_color`. Both have a `build.rs`
that is one line:

```rust
fn main() {
    pill_hot_scan::generate_function_inventory();
}
```

plus one line in `lib.rs`:

```rust
include!(concat!(env!("OUT_DIR"), "/function_inventory.rs"));
```

That generates one entry per addressable function - module-level functions,
inherent methods named through their type, and non-generic trait methods named
as `<Type as Trait>::method` - so the host can redirect any of them with nothing
in the source annotated.

`pill_dummy_math`, `pill_dummy_text`, `pill_dummy_timer` and `pill_dummy_random`
have neither a build script nor annotations, so every edit to them is a full
reload. `devops/tests/test_hot_patch_coverage.py` reports this on every run.

---

## Why `pill_hot_scan` is its own crate

It has two consumers that **must agree byte for byte**: the host, deciding
whether an edit is patchable and under what name to look the function up; and
each participating crate's build script, recording the address under that same
name. When those were separate implementations, every inherent method silently
failed to patch - the host asked for `pill_dummy_color::mix` while the inventory
held `pill_dummy_color::Tint::mix`, and the refusal blamed the build script.

The crate has no dependencies so a build script can use it without pulling the
engine into its own build graph.

---

## Tests

| Suite | Covers |
|---|---|
| `devops/tests/test_hot_reload_suite.py` | Reloads, migration, forgotten types, rollback, cascade, coexistence |
| `devops/tests/test_hot_reload_migration.py` | Table-driven schema migration |
| `devops/tests/test_module_project_auto_reload.py` | The module to project cascade in isolation |
| `devops/tests/test_csharp_bridge.py` | The C# side |
| `devops/tests/test_reload_edit_during_build.py` | A save that lands while a rebuild is running |
| `devops/tests/test_hot_patch_coverage.py` | Every crate that can be live-patched actually is |

All six run from `devops/ci_cd/run_hot_reload_tests.sh`. Rust unit tests live at
the end of each file, notably the tokenizer and replay tests in
`hot_patch/compile.rs` and the prologue primitives in
`pill_engine/src/hot_patch.rs`.

---

## Where to go for a given change

| Symptom | Look here |
|---|---|
| Patching refuses an edit | `classify` in `hot_patch/mod.rs`, and `pill_hot_scan` |
| A crate never patches at all | Its `build.rs` and annotations; run the coverage suite |
| The patch compile fails oddly | `hot_patch/compile.rs` - the replayed command line |
| Reload is slower than expected | `build_runner.rs`, the up-to-date check and staging |
| State did not survive a reload | Migration in `project_module.rs` / `optional_module.rs`, and `pill_engine/src/persistence.rs` |
| Something dangles after an unload | The graveyard, plus `rehome_native_columns` in `pill_engine/src/world.rs` |
| An edit was silently ignored | The generation bookkeeping in `runtime.rs` and `optional_module.rs` |

See `hr_audit.md` for the correctness audit of this same code, including the
known limits that are deliberate rather than defects.
