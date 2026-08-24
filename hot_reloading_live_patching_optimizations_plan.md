# Hot Reloading & Live Patching Optimization Plan

## Executive Summary

This engine has **three** mechanisms for replacing running code, layered by
strength of guarantee:

1. **Whole-artifact reload** - rebuild a module or the project, load the new DLL
   alongside, re-register, migrate state, retire the old image. Pre-existing,
   mature, and carefully reasoned about.
2. **Dispatch slots** (`#[pill_hot]`, `#[pill_hot_fn]`) - an atomic pointer per
   function. Provable: a call that reaches the dispatcher runs current code.
3. **Prologue patching** - overwrite a live function's first bytes with a jump.
   Macro-free, and best-effort by construction.

The audit found the pre-existing reload path to be in good shape: the graveyard
model is deliberate, `rehome_native_columns` explicitly refreshes column function
tables so archetype storage survives eviction, and `clear_systems_owned_by`
retires both systems and their dispatch slots together.

**The real findings are in the newer patching layer, and all four concern
pointers or files that outlive what they point at.** None are theoretical:
each is reachable through ordinary use.

The largest optimization opportunity is not in the invocation path - steady-state
cost is one acquire load, measured below the noise floor - but in
**classification**, which re-reads every source file in a crate on every patch
attempt and walks the directory tree twice.

## Current Architecture

### Detection

`pill_host/src/watcher.rs` spawns two threads per watched directory: `notify`'s
own `ReadDirectoryChangesWatcher` thread, and a debounce worker that owns the
watcher handle and blocks on an mpsc channel. The worker coalesces a burst,
reports `detection_delay_ms`, and bumps an `AtomicU64` with `Release`. The frame
loop reads it with `Acquire`.

Measured: detection is the debounce constant plus ~1 ms.

### Reload (whole artifact)

`build_runner.rs` decides whether cargo must run at all, using three independent
signals: an artifact **stamp** (build command + each artifact's size and mtime),
a toolchain/feature **marker**, and source **mtimes** followed recursively
through path dependencies. Artifacts are then **staged** into `target/hot/`, so
another `cargo build` writing the shared per-crate slot cannot corrupt what the
host loads.

`NativeLibrary::load_copy` copies the staged artifact to a per-process temporary
path before `LoadLibrary`, because Windows locks a mapped DLL.

Retirement is a bounded graveyard: `MAX_GRAVEYARD_GENERATIONS = 2`. Beyond that
the oldest image is unmapped and its temporary file deleted. That is safe because
of two deliberate mechanisms: `clear_systems_owned_by` removes the retired
generation's systems *and* their dispatch slots, and `rehome_native_columns`
re-points every archetype column's function table at a still-mapped generation.

### Patching

`HotPatchSession` (one per crate) classifies an edit as body-only, generates a
single-function patch crate, compiles it by **replaying cargo's own rustc line**,
loads it, and installs the replacement by one of three routes:

| Route | Chosen when | Install |
|---|---|---|
| Engine registry slot | `#[pill_hot]` system | `Engine::hot_patch` |
| Per-artifact slot | `#[pill_hot_fn]` | `install_plain_function` in every artifact |
| Prologue | anything else | 12-byte absolute jump in every artifact |

Discovery for the third route comes from `pill_hot_scan::generate_function_inventory`,
run by each participating crate's build script, so no source is annotated.

### Ownership and lifetime model

- Patch images (`Host::loaded_patches`) are **never unloaded**: a slot or a jump
  may point into one for the rest of the process.
- Module images are unloaded after two further generations.
- Therefore any host-side raw address pointing *into a module image* is the
  dangerous direction, and is what the findings below concern.

## Correctness and Safety Findings

### P1-A. Prologue patch records survive a project reload

- **Location**: `pill_host/src/runtime.rs`, `run_one_frame`; `HotPatchSession::forget_prologue_patches`.
- **Problem**: `forget_prologue_patches()` is called only inside the optional-module
  reload loop (Step 0b). The project reload (Step 1) does not call it, so
  `Generation::prologue_restores` keeps addresses pointing into the **previous**
  project image.
- **Impact**: a rollback after a project reload writes saved bytes to an address
  in a retired image. After two further reloads that image is unmapped.
- **Evidence**: `grep forget_prologue_patches runtime.rs` returns only the two
  call sites inside the module loop; the project reload is ~70 lines later.
- **Proposed solution**: clear on any reload, not only a module reload.
- **Risk**: none - it only discards records that no longer describe anything.
- **Benefit**: removes a class of write-to-retired-image.

### P1-B. `restore_prologue` can write to a reused address

- **Location**: `pill_engine/src/hot_patch.rs`, `restore_prologue`.
- **Problem**: it re-reads the function extent, so an *unmapped* address is
  refused. It does **not** verify that the bytes it is about to overwrite are the
  jump it installed. If a new image occupies that address and the extent lookup
  succeeds, the saved bytes are written over unrelated code.
- **Impact**: silent corruption of a live function, at the worst possible moment
  (a rollback, when the developer is already recovering from something).
- **Evidence**: the function checks `function_extent` and length only.
- **Proposed solution**: verify the current first bytes are the absolute jump
  this module writes before restoring. Cheap, exact, and needs no extra state.
- **Risk**: a legitimate restore is refused if something else patched the same
  function afterwards - which is correct behaviour, not a regression.
- **Benefit**: turns a silent corruption into a refusal.
- **Implemented.** The guard reads the first two bytes and requires `48 B8`, the
  `mov rax, imm64` this module writes. The read happens before `VirtualProtect`,
  which is sound: `function_extent` has already established the range is a mapped
  function, and executable pages are readable. Covered by
  `restoring_over_code_that_is_not_our_jump_is_refused`, which asserts the target
  still behaves correctly afterwards rather than only that an error was returned.

### P1-C. Prologue patching has no atomicity across the 12-byte write

- **Location**: `pill_engine/src/hot_patch.rs`, `patch_prologue`.
- **Problem**: 12 bytes are written with `copy_nonoverlapping`. A thread executing
  that function concurrently can observe a torn instruction stream.
- **Impact**: undefined behaviour if the invariant is violated.
- **Status**: **not solvable within this mechanism.** No 12-byte write is atomic,
  and "write the first byte last" does not help because the original first byte
  is not a valid prefix for the new instruction. The real mitigations are
  suspending threads (heavy, risky) or the slot route (already available).
- **Existing mitigation**: `declare_patching_thread()` restricts patching to the
  frame thread, where no system is executing. This is now enforced rather than
  assumed.
- **Decision**: keep the model, document it precisely, and make sure every
  refusal that stems from it points at `#[pill_hot_fn]`. Recorded, not "fixed".

## Concurrency Findings

- **Invocation vs reload**: slot routes use `Release` store / `Acquire` load on
  the same atomic, which is exactly the pairing needed to make the patch image's
  code visible before its address. Correct, and the weakest ordering that is.
- **Commit**: slot installs are a single pointer store, so a caller sees old or
  new, never half. Prologue installs are **not** atomic - see P1-C.
- **Old-generation lifetime**: bounded graveyard, with systems and slots retired
  together and column function tables re-homed. Patch images never unloaded.
- **`PATCHING_THREAD` token**: the address of a thread-local. Unique per live
  thread; could in principle be reused after a thread exits. The frame thread
  lives for the process, so this is not reachable here. Noted, not changed.
- **Locking**: the only lock in the subsystem is the analytics `Mutex`, taken on
  reload and patch completion. It is never held across dynamically loaded code
  and never taken on an invocation path. `print_patch_tally` deliberately takes
  it separately rather than extending `print_reload_events`' borrow.

## Performance Findings

### Steady-State Invocation Cost

One `Acquire` load and one indirect call per patched function. Measured during
the original research at 1.469 ns/call against 1.617 ns/call for a direct call -
i.e. below the noise floor of ABI and register-allocation accidents. **No change
proposed.** In release builds `#[pill_hot_fn]` compiles to the body itself, so
the cost is provably zero there.

### Reload-Time Cost

Measured save-to-live: **~0.48 s**, of which ~400 ms is `rustc`.

The one avoidable cost found:

### P2-A. Classification re-reads the whole crate on every attempt

- **Location**: `pill_host/src/hot_patch/mod.rs`, `classify`.
- **Problem**: `rust_sources()` walks the directory tree and every `.rs` file is
  read in full, on every patch attempt. The "edited before arming" check then
  walks the tree a **second** time.
- **Evidence**: `classify` measured at 1-7 ms typically but **53 ms and 72 ms**
  observed; it scales with crate size, and these are small crates.
- **Proposed solution**: skip reading files whose mtime is not newer than the
  snapshot, and reuse the single directory walk for both purposes.
- **Risk**: a file whose mtime does not advance would be missed. **Not** a
  correctness hazard: the edit still reaches the running process through the full
  reload that the pending generation triggers - only the fast path is skipped.
- **Implemented, with a correction.** The first attempt compared file mtimes
  against `SystemTime::now()` and **broke every classification test**. On Windows
  those are different clocks: file times come from the coarse system clock and a
  file written *after* a snapshot can report an earlier time. The gate now
  compares a file's mtime against the mtime recorded when it was read - one
  clock, exact comparison. The snapshot's content and recorded time are updated
  together so the pair cannot drift.
- **Measured benefit: none, on this repository.** Every crate the host watches
  (`pill_dummy_color`, `pill_spline`, `examples/project_rs`) contains exactly one
  source file, so there is nothing to skip. `classify` measured 6-24 ms before and
  after. The change is algorithmic - cost becomes proportional to the edit rather
  than to crate size - and it removes a second directory walk that ran on every
  attempt. It is kept on those grounds, **not** on measured evidence.

### Memory / Allocation Cost

Transient `Vec<String>` per install for diagnostics labels; a few `PathBuf`s per
classification. All reload-time, none per-invocation. **Not worth changing** -
they are dwarfed by a 400 ms compile.

One unbounded growth:

### P3-A. Patch artifacts accumulate in the system temp directory forever

- **Location**: `pill_host/src/hot_patch/mod.rs`, `apply`.
- **Problem**: generated `.rs` and `.dll` files go to `std::env::temp_dir()`,
  outside the per-process directory that `cleanup_temporary_files` already
  sweeps. They are never removed, by this run or any later one.
- **Proposed solution**: write them into the per-process temporary directory that
  already has a cleanup path. The **flags cache stays where it is** - it is a
  deliberate cross-session cache, and moving it would discard the 1.2 s saving it
  exists for.
- **Risk**: none; the images stay mapped for the process lifetime either way.
- **Implemented and verified**: 12 artifacts observed in
  `pill_standalone_temp/<pid>/` after three patches, none in the system temp
  directory.

### Locking / Contention

No contention found. Single analytics mutex, reload-time only.

## Simplification Opportunities

Examined and **rejected**:

- Collapsing the three install routes into one. They exist because they have
  genuinely different guarantees; merging them would either weaken the slot
  route or make the prologue route claim a guarantee it cannot keep.
- Replacing `HashMap` dispatch with indexed slots. The registry is touched at
  registration and patch time, never per invocation - the map is not on any hot
  path.
- Removing the per-crate `HotPatchSession` in favour of one global session. The
  per-crate compiler flags are the whole reason it is per-crate.

Already done during this session's earlier work, recorded here for completeness:
the source scanner was consolidated into `pill_hot_scan` so the host and every
build script share one implementation.

## Proposed Changes

| # | Change | Files | Benefit | Risk |
|---|---|---|---|---|
| 1 | Clear prologue records on any reload | `runtime.rs` | removes stale writes into retired images | none |
| 2 | Verify bytes before restoring | `pill_engine/src/hot_patch.rs` | silent corruption becomes a refusal | refuses a doubly-patched restore, correctly |
| 3 | mtime-gated classification, one tree walk | `hot_patch/mod.rs` | classification proportional to the edit | missed edit if mtime does not advance; mitigated with `>=` |
| 4 | Patch artifacts into the swept temp directory | `hot_patch/mod.rs` | bounded disk use | none |

## Implementation Order

1. Change 1 (correctness, trivial).
2. Change 2 (correctness, self-contained).
3. Change 4 (hygiene, self-contained).
4. Change 3 (performance, touches the most-tested path - do it last, with tests).

## Validation Strategy

- `cargo test` for `pill_hot_scan`, `pill_engine` (both feature configurations),
  `pill_host` (both), `pill_dummy_color`, `pill_spline`.
- `cargo build --workspace` and the standalone with `pill_host/hot_patch`.
- `cargo fmt --check` and `cargo clippy` on the touched crates.
- Live: patch a function, reload the project, confirm the stale record is gone;
  patch repeatedly and confirm artifacts land in the swept directory.
- Re-measure save-to-live after change 3 to confirm no regression.

## Deferred / Rejected Ideas

- **Suspending threads during a prologue patch** (P1-C). Enumerating and
  suspending threads on Windows is heavy and introduces deadlock risk against the
  loader lock. The slot route already provides a safe alternative.
- **Making the 12-byte write atomic.** Not possible; see P1-C.
- **Bounding `loaded_patches`.** Unloading a patch image is exactly what the
  design forbids - a jump or slot may point into it forever.
- **Replacing the analytics `Mutex`.** No contention exists to remove.

## Final Checklist

- [x] Audit the pre-existing reload path and graveyard model
- [x] Audit the patching layer's pointers into module images
- [x] Audit atomic orderings on both slot routes
- [x] Audit locking
- [x] Change 1: clear prologue records on any reload
- [x] Change 2: verify installed bytes before restoring
- [x] Change 4: patch artifacts into the swept temp directory
- [x] Change 3: mtime-gated classification (kept on algorithmic grounds; no
      measurable win on this repo's single-file crates)
- [x] Change 5: name the real reason a rollback is refused after a reload
      (found by the re-audit, below)
- [x] Tests for each change - 4 added (`an_untouched_file_is_not_re_read`,
      `a_touched_file_is_re_read`, `restoring_over_code_that_is_not_our_jump_is_refused`,
      `a_prologue_generation_a_reload_invalidated_says_so`)
- [x] Full validation sweep - all suites green in both feature configurations
- [x] Live end-to-end run - both patch routes, a project reload, and all three
      rollback outcomes exercised against a running host
- [x] Clippy clean across every touched file
- [x] Re-audit the diff - found and fixed three things: an incoherence (snapshot
      content updated without its recorded mtime), a dangling brace from a clippy
      `collapsible_if` fix, and change 1's misleading refusal (change 5)

### Change 5. A rollback refused after a reload blamed the wrong thing

Found by the live re-audit, not by the tests - and caused by change 1.

- **Location**: `pill_host/src/hot_patch/mod.rs`, `HotPatchSession::rollback`.
- **Problem**: rollback picks its route from `overwrote_prologues()`, which is
  just "are there saved bytes". Change 1 empties that list, so a
  prologue-delivered generation became indistinguishable from a slot-delivered
  one and took the slot route - which refused with *"no loaded artifact declares
  `pill_spline::catmull_rom`; it may live in a crate nothing has loaded yet"*.
  The crate was loaded. The refusal was correct; the reason was not, and it
  pointed a developer at a build problem that did not exist.
- **Reachable by**: patch an un-annotated function, trigger a project reload,
  patch it again, then roll back to the first generation. Observed on a running
  host.
- **Fix**: `Generation` now carries `prologue_history_dropped`, set where the
  restores are cleared, and rollback checks it first and says what actually
  happened.
- **Verified**: the new test fails with the exact misleading message when the
  branch is disabled, and the corrected message was then confirmed on a running
  host. Rolling back to a still-valid generation and to generation 0 both still
  succeed, so the check does not over-refuse.

## Measured results

Save-to-live, after all changes, on a running host:

| Route | Function | Time |
|---|---|---|
| Per-artifact slot | `pill_dummy_color::get_color_a` | 0.518-0.541 s |
| Prologue | `pill_spline::catmull_rom` | 0.597-0.675 s |

Detection is 0.061-0.062 s of that, against 0.301 s before the debounce was
lowered. The first patch of a session costs ~0.83 s - a cold `rustc`.

Change 4's effect is direct: 12 patch artifacts (`.dll`, `.dll.lib`, `.pdb`,
`.rs` per generation) landed in `pill_standalone_temp/<pid>/` and none in the
system temp directory. The `.pdb` alone is 18.8 MB per generation, so a session
of a dozen edits used to leave roughly a quarter of a gigabyte behind with no
cleanup path.

Change 3 has **no measured effect on this repository** and is not claimed as a
performance win. See its entry above.

### One refusal seen during the live run, and why it is not a regression

The first prologue patch attempted right after a project reload once failed with
`E0463: can't find crate for pill_spline`, because that reload rebuilt
`pill_dummy_color` - which `pill_spline` links - leaving the staged rlib's
dependency closure stale. The fast path refused, said so precisely, fell back to
a full reload, and the next patch of the same function succeeded. A second run of
the same sequence did not reproduce it, so it depends on the interleaving. This
is the residue of P0-2b: `refresh_staged_rlib` restages the package's own rlib
but not its dependencies'. The behaviour is correct - refuse and fall back - and
predates these changes.

## Post-implementation notes

**Formatting.** `cargo fmt --check` reports differences across this repository,
including in files untouched by this work (`archetype.rs`,
`examples/resources_demo.rs`). Drift counts were compared before and after: 6/6
in `hot_patch.rs`, 0/0 in `hot_patch/mod.rs`, 4/4 in `native_library.rs`, 1/1 in
`runtime.rs`, 5/5 in `pill_hot_scan/src/lib.rs` - i.e. **zero new drift
introduced**, measured by running the same `rustfmt` over each file's committed
and current versions. The pre-existing differences were left alone rather than
reformatting files unrelated to the change.

**Clippy.** Every warning in a touched file was fixed, except one that was
answered rather than silenced: `apply` takes nine arguments, and they are nine
distinct collaborators rather than fields of an implicit struct, so it carries a
targeted `#[allow]` with the reasoning next to it.

One further `#[allow(dead_code)]` was added, on the `isolated_systems!` test
macro in `pill_engine/src/hot_patch.rs`. That warning is pre-existing - it fires
at `HEAD` too - and is inherent to the macro: each instantiation uses only the
helpers its own test needs.

**Release profile.** The `hot_patch` release guard was verified to fire: building
with `debug_assertions` off stops at its `compile_error!`, which is the exact
predicate the guard is written against. A full `--release` build could not be
used for that check because this repository's release profile does not build for
two reasons unrelated to this work - `-C prefer-dynamic` from
`modules/.cargo/config.toml` conflicts with `lto = "fat"`, and clearing those
flags then hits `panic = "abort"` against a precompiled `panic_unwind` std.
