# Hot Reloading - Correctness Audit

Audit of every subsystem involved in reloading and live-patching code in a
running process: detection, build, staging, load, registration, migration,
retirement, and the three patch mechanisms.

**Scope** - ~14,000 lines across `pill_host` (`runtime`, `watcher`,
`build_runner`, `native_library`, `optional_module`, `project_module`,
`hot_patch/`, `analytics`), `pill_engine` (`hot_patch`, `world`, `persistence`),
`pill_hot_scan`, and `pill_engine_macros`.

**Method** - read for invariants rather than style, concentrating on the places
where correctness is actually decided: every `unsafe` block and whether its
stated contract is enforced, ownership and lifetime across the DLL boundary,
atomic orderings, error paths that discard information, and resource teardown
ordering. Findings that could be demonstrated on a running host were
demonstrated rather than argued.

**Headline** - the patching layer is in better shape than the reload path. The
one serious defect is in neither: it is in the generation bookkeeping that both
share, it silently discards a developer's edit, and it is reproducible.

**Status: all findings below are fixed.** Each fix is noted under its finding,
and F1 carries a regression test that was verified to fail without it.

---

## Findings

### F1. An edit saved during a reload is silently discarded - **demonstrated**

**Severity: high.** Data loss from the developer's point of view: the file on
disk and the code running in the process disagree, permanently, with no error
anywhere.

**Location** - three sites, all the same shape:

- `pill_host/src/optional_module.rs`, `reload_if_changed` (~line 190)
- `pill_host/src/optional_module.rs`, `consume_pending_reload` (~line 222)
- `pill_host/src/runtime.rs`, after the project reload (~line 736) and after a
  successful patch (~line 694)

**The code**

```rust
let generation = self.reload_generation.load(Ordering::Acquire);
if generation == self.last_processed_generation { return false; }
self.reload(engine, engine_api, workspace_root, generation);

// Re-read rather than storing the captured value: a save during the
// build advances the counter again, and the next frame must retry.
self.last_processed_generation = self.reload_generation.load(Ordering::Acquire);
```

**The problem** - the comment states the intent exactly, and the code does the
opposite of it. Re-reading assigns the *newer* counter value, which makes
`has_pending_reload()` false and guarantees the next frame does **not** retry.
Storing the captured `generation` is what would leave the newer save pending.

Worse, the cancellation machinery is built specifically to let a newer save win.
`run_build_command` polls the same counter and aborts the build the moment it
advances:

```rust
if cancel_flag.is_some_and(|(generation, baseline)| generation.load(Acquire) != baseline) {
    let _ = child.kill();
    return Err(BuildError::Cancelled);
}
```

The build is then correctly abandoned - and the bookkeeping immediately marks the
save that caused the cancellation as processed. The two mechanisms cancel each
other out, and the net effect is that the edit is dropped.

**Demonstrated** on a running host. Two saves to `pill_spline`, the second 0.8 s
into the rebuild triggered by the first:

```
961.082  source change detected                          <- save 1
961.185  optional module reload triggered generation=1
961.185  building optional module pill_spline
961.883  source change detected                          <- save 2, generation -> 2
961.908  ERROR build failed; keeping the old module generation
         error=build cancelled: sources changed again during compilation
```

After that point, for the remainder of the session:

```
pill_spline reload triggers after the cancellation: 0
pill_spline builds after the cancellation:          0
```

The module kept running its pre-edit code while the source on disk held the new
value. Only another save recovers it.

**A second consequence, also demonstrated.** The cancelled module reload still
queued the dependent project reload, and the project *did* rebuild - statically
linking the module's **new** source. So the process ended up running two copies
of the same crate at different versions: the `pill_spline` DLL at the old code,
the project's embedded copy at the new. This is precisely the divergence the
cascade exists to prevent.

**Fix** - assign the captured baseline, not a fresh load:

```rust
self.last_processed_generation = generation;
```

at all four sites. A save that arrives during the work then stays pending and the
next frame acts on it, which is what every comment at those sites already
promises. The change is small; the ordering is already correct, so nothing else
moves.

**Why no test caught it** - every existing suite makes one edit and waits. The
failure needs a second edit inside the build window, which no suite creates.

**FIXED.** All four sites now record the generation they observed before doing
the work. `has_pending_reload()` became `pending_reload_generation() ->
Option<u64>` so the module fast path can hand the exact generation it acted on
back to `consume_pending_reload(generation)`, rather than each end re-reading a
counter that has moved.

`devops/tests/test_reload_edit_during_build.py` pins it, and was verified against
the defect: reverting the one-line change in `reload_if_changed` makes it fail
with the precise diagnosis -

```
[FAIL] The second save was never built within 90s.
       The in-flight build WAS cancelled for it, and then nothing
       rebuilt - so the edit is stranded on disk, uncompiled, and
       the running module still has the previous code.
       module builds seen: 1 (expected more than 1)
```

and it passes with the fix in place, reporting that the module was rebuilt after
the cancellation. The suite is wired into `run_hot_reload_tests.sh` as suite
5/6.

---

### F2. `restore_prologue` is not thread-guarded, `patch_prologue` is

**Severity: medium.** An invariant that is enforced on one path and merely
documented on the other, for two writes with identical hazards.

**Location** - `pill_engine/src/hot_patch.rs`, `restore_prologue` (~line 892).

Both functions overwrite 12 bytes of live code, and neither write is atomic. The
project's answer to that is to confine patching to the frame thread, where no
system is executing, and to *enforce* it:

```rust
// patch_prologue, line 801
check_patching_thread()?;
```

`restore_prologue` carries the same contract in its doc comment -

> The same contract as [`patch_prologue`]: `target` must be the address the bytes
> came from, and no thread may be executing inside them.

- but performs no check. Its other guards are present and good (extent re-read,
and the `48 B8` check that the bytes are the jump this module installed), so the
omission looks like an oversight rather than a decision.

Rollback is developer-triggered through a request file, and today it is processed
from the frame loop, so the invariant currently holds by construction. That is
exactly the situation the guard exists to stop relying on.

**FIXED.** `restore_prologue` now calls `check_patching_thread()?` alongside its
other guards, so the contract its doc comment states is enforced rather than
assumed.

---

### F3. A failed `init` that registered a *new* type leaves a dangling factory

**Severity: low, latent.** Not reachable into a call today. Recorded because the
reasoning that makes it unreachable is incidental rather than designed.

**Location** - `pill_host/src/optional_module.rs`, the init-failure rollback
(~line 325), together with `World::register_component`.

`register_component` overwrites unconditionally:

```rust
self.storage_factories.insert(component_id, StorageFactory::Native(...));
```

so the normal rollback is safe: the previous generation's `init` re-registers
every type it owns and overwrites each factory with pointers into the image that
stays mapped. `clear_systems_owned_by` handles systems and dispatch slots.

The gap is a type the **new** generation registers that the old one does not
know. Sequence: new `init` registers `X`, then fails; `clear_systems_owned_by`
removes systems but not factories; the rollback runs the *old* `init`, which
never mentions `X`; the new library is dropped at end of scope and unmapped. The
factory for `X` is left pointing into an unmapped image.

**Why it does not bite today** - nothing can create a column of `X`: the module
that defines it failed to initialise. `rehome_native_columns` copies the stale
`ops` struct into a map and applies it only to columns whose type id matches, of
which there are none. Copying a dangling function pointer is not undefined
behaviour; calling it would be.

**FIXED.** The rollback path now takes a second registration-sequence snapshot
around the rollback `init`, computes the types the failed generation registered
that the rollback did not re-register, and drops them through
`drop_forgotten_components` - which already removes the storage factory, copier
and serializers via `forget_component_type`. Anything the rollback re-registered
is left alone, because registering a type overwrites its factory with pointers
into the still-mapped generation. The drop is logged at WARN, since it means a
generation introduced a type and then failed.

---

### F4. `parses_real_workspace_timings_when_present` fails on a clean workspace

**Severity: low, but it is failing right now.** A unit test whose result depends
on mutable workspace state outside the test.

**Location** - `pill_host/src/analytics.rs`, `parses_real_workspace_timings_when_present`
(~line 1473).

The test reads the workspace's most recent `cargo --timings` report and asserts
it names at least one executed crate:

```rust
let Some(timing) = parse_latest_cargo_timings(workspace_root) else { return };
assert!(
    !timing.crate_durations_ms.is_empty(),
    "a real --timings report must name at least one executed crate"
);
```

Its comment reasons that "the host always builds with `--timings`, so any recent
report has one". That holds for a report from a build that *did work*. The parser
deliberately keeps only units with a non-zero duration:

```rust
if duration_seconds > 0.0 {
    crate_durations_ms.insert(...);
}
```

and a fully up-to-date build emits a report where every unit has
`"duration": 0`. The `else { return }` guard covers "no report at all" but not
"a report from a no-op build", which is the far more common state in a workspace
that has just been built twice.

**Observed now**, at `HEAD` with a clean tree:

```
test analytics::tests::parses_real_workspace_timings_when_present ... FAILED
  a real --timings report must name at least one executed crate

newest report: cargo-timing-20260824T220241960Z-*.html
units: 160 | non-zero durations: 0
```

This is not a regression from any recent change - it is state-dependent, and it
turns green again after any build that recompiles something. That is the defect:
the suite's result depends on what the developer happened to run last.

**Fix** - treat a report with no executed units the same as no report, extending
the guard that already exists:

```rust
let Some(timing) = parse_latest_cargo_timings(workspace_root) else { return };
if timing.crate_durations_ms.is_empty() {
    return; // a no-op build produces a report in which every unit took 0 s
}
```

or point the test at a fixture instead of the live workspace, as the sibling
`parses_cargo_timings_fixture` already does.

**FIXED** with the first option, plus a stronger assertion in place of the one
that was removed. The test now skips a report with no executed units - a real
report, correctly parsed, that simply describes a no-op build - and instead
asserts what the parser actually promises about the entries it *does* return:
every one has a name and a non-zero duration.

---

### F5. The project probe string was corrupted, breaking a suite

**Severity: low, but it had a suite red.** Found while re-running the regression
net after the fixes above.

`examples/project_rs` prints a probe line every frame batch that
`test_module_project_auto_reload.py` parses to confirm the cascade delivered a
change. The word `midpoint` in that format string had been corrupted to
`midpoixntxx`, so the suite's `PROBE_PREFIX = "midpoint ("` never matched and it
failed with *"No spline probe report after startup"* - while the probe was
printing perfectly well on screen.

Traced through history: the string was intact at `fa47e84` and corrupted in
`e809491`, i.e. by an editing accident in this project's recent live-patching
work rather than by anything in the engine. (`xxsees` on the same line is
pre-existing and deliberate, and was left alone.)

**FIXED** - `midpoixntxx` restored to `midpoint`. The cascade suite passes again:

```
[OK] Project probe reports the new value 'midpoint (400.0, 289.8)'.
[PASS] Auto project reload verified end-to-end.
```

**Worth noting for its own sake**: a suite that greps for a string in program
output fails silently-ish when that string drifts. The failure said the probe was
missing, not that its format had changed, which is a slower thing to diagnose
than it should be.

---

## Verified correct

Each of these was checked against a specific failure mode rather than skimmed.

**The graveyard is sound, and for a stated reason.** `MAX_GRAVEYARD_GENERATIONS
= 2` holds because three separate mechanisms cooperate, in the right order:
`drop_forgotten_components` runs *while* the retiring generation is still mapped;
`rehome_native_columns` then re-points every native column's function table at a
still-mapped generation; `clear_systems_owned_by` retires systems and their
dispatch slots together. The ordering in `optional_module::reload` (drop, rehome,
migrate) is the only order that works, and the comments say why.

**Raw function pointers extracted from `Symbol` wrappers are valid for the
struct's lifetime.** `NativeLibrary` stores `library: Some(library)` alongside
the copied `module_init` / `module_update` pointers, so the image outlives them.
The pointers address the mapped image, not the struct, so moving the struct is
harmless.

**Teardown order is correct.** `Drop for NativeLibrary` drops the library handle
before deleting the temporary file, because Windows refuses to delete a mapped
file - and it reports a failed delete rather than swallowing it.

**Atomic orderings on the reload signal are correct and minimal.** The watcher's
`fetch_add(Release)` pairs with the frame loop's `load(Acquire)`; the slot routes
use Release-store / Acquire-load on the same atomic, which is exactly what makes
a patch image's code visible before its address. (What the code does *with* the
value read is F1; the ordering itself is right.)

**Analytics cannot poison the frame loop.** Every lock uses
`unwrap_or_else(|poisoned| poisoned.into_inner())`, so a panic inside one
reporting path cannot take down later reloads.

**The one `unwrap` in the watcher is guarded.** `strip_prefix` at
`watcher.rs:115` is preceded by the `starts_with` check that makes it
infallible, and the comment says so.

**Cancellation itself is implemented correctly.** The child process is killed
*and* waited on, so no zombie is left; the timeout path does the same. The defect
is downstream, in what the caller records afterwards (F1).

**Patch generation names methods unambiguously.** `find_method` scopes the search
to the owning `impl` block, so two types implementing one trait method cannot
have the wrong body compiled into a patch - the failure mode that a bare-name
search would have produced silently.

---

## Known limits, re-confirmed as deliberate

These are documented decisions, not defects. Listed so the audit is complete.

| Limit | Why it stands |
|---|---|
| The 12-byte prologue write is not atomic | No 12-byte write is. Mitigated by the frame-thread guard (see F2 for the gap) and by `#[pill_hot_fn]`, which needs no code rewriting |
| Prologue rollback does not survive a reload | Records span artifacts, so they cannot be cleared selectively; a partial restore would leave two copies of one function disagreeing |
| Generic functions and generic `impl`s are refused | One instantiation per set of type arguments means no single address. Not a naming problem |
| Leaf functions cannot be prologue-patched | Windows omits unwind data for functions that never touch the stack, so the extent is unknowable. Refusal names `#[pill_hot_fn]` as the way out |
| Prologue patching is Windows x86-64 only | `RtlLookupFunctionEntry` has no portable equivalent. The slot routes are portable |
| `pill_engine` cannot be a `dylib` | A Rust dylib must export every symbol; with `rendering`, wgpu/naga/winit push the export table past the Windows limit of 65535 (`LNK1189`). `pill_core` is the shared boundary instead |

---

## What is left

F1 through F5 are fixed and verified. One item from the audit remains open, and
it is a coverage gap rather than a defect:

**Four of the six loaded modules have no fast path at all.** `pill_dummy_math`,
`pill_dummy_text`, `pill_dummy_timer` and `pill_dummy_random` carry neither a
hot-patch annotation nor a `build.rs` calling
`pill_hot_scan::generate_function_inventory()`, so every edit to them is a full
reload. `test_hot_patch_coverage.py` reports this on every run and fails on it
under `--strict`. Closing it is one build script per crate.

## Verification of the fixes

```
cargo build --workspace                      clean
pill_hot_scan                                 17 passed
pill_engine            --lib                 150 passed
pill_engine  --features hot_patch --lib      165 passed
pill_host              --lib                  71 passed
pill_host    --features hot_patch --lib      118 passed

test_reload_edit_during_build.py             PASSED (fails without the F1 fix)
test_module_project_auto_reload.py           PASSED (was failing on F5)
test_hot_patch_coverage.py                   PASSED
```

## Method notes

F1 was reproduced on a running host and is quoted from its log. F4 was observed
failing during this audit and its cause confirmed against the report on disk. F2
and F3 are read from the source and are argued, not demonstrated; F3 explicitly
includes the argument for why it does not currently bite, so it is not
overstated. The "verified correct" section lists what was checked against a
concrete failure mode - it is not a claim that the code is free of defects
outside the areas named here.
