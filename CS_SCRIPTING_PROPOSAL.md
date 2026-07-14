# Proposal: C# scripting for `tracy_live`

Not yet implemented — this is the design for review before any code is
written. See `HOT_RELOADING_101.md` for how the existing Rust hot-reload path
works; this proposal adds a second, C#-authored backend next to it.

## The problem

`tracy_live`'s systems (`movement_system`, `health_decay_system`,
`gravity_system`, ...) operate on `ecs_hybrid`'s generic `Query<T>` /
`Commands` API. C# cannot author a `Query<(&mut Position, &Velocity)>` — the
type doesn't exist outside Rust generics. So "C# scripting" here can't mean
"C# writes the same kind of system Rust does." It has to mean something
adapted to what C# can actually receive over FFI.

`Scripting-Language-Tests/hot_reloading`'s `game_cs` sidesteps this because
Flappy's game state (`Bird`, `List<Pipe>`) is a plain C# object graph — C#
owns it outright and only calls back into the host for drawing/input.
`tracy_live` has no such object graph; its state *is* the ECS's archetype
storage.

## Alternatives considered

Five genuinely different shapes this could take, from most to least direct
access to ECS memory:

| # | Design | How it works | Performance | Safety | Verdict |
| --- | --- | --- | --- | --- | --- |
| A | **Zero-copy `Span<T>`** (proposed below) | C# indexes directly into the same memory the archetype storage owns, via a bounds-checked `Span<T>` handed out each `Update` call. | Best — no copies, no per-entity call overhead. | Unsafe surface limited to ~10 lines of FFI plumbing; script code itself (`Systems.cs`) is ordinary safe, bounds-checked C#. | **Recommended.** |
| B | **Batched marshaled copy per frame** | Rust copies each component array out to a buffer; C# reads/writes a private managed array; Rust copies the results back after `Update` returns. | ~2x memory bandwidth per frame (copy in + copy out) vs. A; still just a `memcpy`, not per-element overhead, so probably fine at 30000 entities, just strictly worse than A. | A C# bug can only corrupt its own private copy, never the live archetype, until the explicit write-back — a stronger isolation guarantee than A, but only meaningfully stronger if the script author bypasses `Span` and pokes raw pointers, which A doesn't require them to do either. | Viable, but pays a real cost for a safety margin A already provides via `Span` bounds-checking. |
| C | **Parameterized orchestration** | No C#-authored per-entity logic at all. Components/systems stay 100% native Rust; C# only tunes numeric knobs (decay rate, gravity constant, entity count) and toggles systems on/off via `Engine::enable_system`/`disable_system`, which already exist. | Native — identical to the Rust path. | Trivially safe — C# never touches ECS memory, only plain numbers. | Simplest and safest, but doesn't give C# real ownership of gameplay logic — this is the "orchestration-only" idea from the first draft of this doc, generalized to parameter-tuning instead of just enable/disable. |
| D | **Per-entity callback bridge** (`ScriptComponent`-style) | Mirror `ecs_hybrid`'s own `ScriptComponent::update(&mut self, ctx)` pattern, calling into C# once per entity per system per frame. | Bad — 30000 entities × 3 systems × 60+ FPS means millions of P/Invoke calls/sec; call overhead alone would tank FPS by 1-2 orders of magnitude. | Same safety profile as A/B (still just data in/out per call), but the overhead makes it impractical at this entity count regardless. | Ruled out on performance alone — this is the most literal translation of Rust's own scripting trait, but doesn't fit a 30000-entity workload. |
| E | **Out-of-process / IPC scripting** | Run C# in a separate process, talking to the Rust host over shared memory or a socket instead of in-process hostfxr hosting. | Adds serialization + IPC latency on top of whatever transport is chosen — strictly worse than any in-process option for a per-frame hot loop. | Strongest possible isolation — a C# crash can't take down the Rust process at all (today's in-process hosting already gets most of this in practice: `game_cs`'s `Interop.cs` already wraps every call in try/catch specifically so a managed exception never unwinds across the native boundary). | Solves a problem (process-level crash isolation) this demo doesn't have — added complexity for no real payoff here. |

A is recommended; B is the fallback if, after trying A, the shared-memory
model turns out to feel too sharp-edged in practice. C and D are included
for completeness but aren't recommended — C forecloses real scripting, D
doesn't perform. E is out of proportion to what this demo needs.

## Proposed design: zero-copy `Span<T>` over component storage

Expose each component type's underlying storage directly to C# as a
`Span<T>` — bounds-checked, zero-copy, no marshaling, no per-entity P/Invoke
call. This is the same technique Unity DOTS/Burst uses for `NativeArray<T>`,
and it directly answers the performance-and-safety ask:

- **Performance**: C# indexes straight into the same memory the ECS's
  archetype storage already owns. No copies in or out, no per-element
  interop overhead.
- **Safety**: the *only* unsafe code is the FFI plumbing itself — one
  `extern "C"` getter per component type on the Rust side, and one
  `new Span<T>(ptr, len)` call per getter in a C# facade class. That's
  exactly as much unsafe surface as the existing `EngineApi`/`Interop.cs`
  already has for passing `*const u8` strings across the boundary. The
  actual script code a user edits (`Systems.cs`) is 100% safe C# — indexing
  a `Span<T>` out of bounds throws `IndexOutOfRangeException`, it cannot
  corrupt memory.

This only works because `tracy_live` never changes an entity's component set
at runtime — every spawned entity always has the same 5 components, so there
is always exactly **one archetype**. The new engine method this relies on,
`World::component_slice_mut::<T>()`, returns `None` if that invariant doesn't
hold (0 or 2+ archetypes contain `T`), so a future misuse fails loudly
instead of silently returning a partial/wrong view.

**Symmetry worth noting**: a C# `Span<T>` must be re-fetched every `Update`
call and never cached across frames — this is exactly the rule `system.rs`'s
doc comment already states for Rust's own `SystemParam` ("must not escape
the system function"). Same invariant, enforced by convention in C# instead
of by Rust's type system, but the same underlying reason: the population can
be restructured between calls.

## Scope cut: no entity destruction from C#

The existing `cleanup_system` never actually destroys anything under the
demo's default tuning — health only trends upward
(`(h + 0.1).max(0.0)`, never reaches `<= 0.0`). So this proposal's C# path
implements `Movement` / `HealthDecay` / `Gravity` over `Span<T>` and
**does not** implement entity destruction. That sidesteps index-to-`Entity`
handle bookkeeping (structural-change safety that's genuinely hard to get
right from outside Rust's borrow checker) while preserving the demo's
observed behavior exactly.

## What gets built

### Native side (`examples/tracy_live/`)

| File | Purpose |
| --- | --- |
| `cs_components.rs` | `#[repr(C)]` `Position`/`Velocity`/`Health`/`Mass`/`GravityForce` — defined here (host, never reloaded) since C# mirrors their layout. `setup(engine)` registers them and spawns the same 30000-entity population as the Rust path. |
| `hostfxr.rs` | Near-verbatim port of `flappy/src/hostfxr.rs` — already generic, no Flappy-specific code to change. |
| `hot_cs.rs` | Defines the native `EngineApi` (function-pointer table below), runs `dotnet build` for both C# projects once at startup, initializes hostfxr, resolves the loader's `Init`/`Update`, exposes `CsGame::update(dt)`. |

Native `EngineApi` (mirrors the existing `EngineApi` naming style — explicit
named fields, not a generic dispatcher):

```rust
#[repr(C)]
pub struct EngineApi {
    pub entity_count: extern "C" fn() -> u32,
    pub get_positions: extern "C" fn(*mut *mut Position, *mut u32),
    pub get_velocities: extern "C" fn(*mut *mut Velocity, *mut u32),
    pub get_healths: extern "C" fn(*mut *mut Health, *mut u32),
    pub get_masses: extern "C" fn(*mut *mut Mass, *mut u32),
    pub get_gravity_forces: extern "C" fn(*mut *mut GravityForce, *mut u32),
}
```

### `ecs_hybrid` — two additive changes

- `trait_type_map`: `VecStorage<T, Dyn>` gets `as_slice`/`as_mut_slice`
  (next to its existing `get`/`get_mut`/`push`) — purely additive.
- `ecs_hybrid::World`: new `component_slice_mut::<T>()` (shown above,
  "Proposed design" section) built on top of it.

### New C# projects

| Project | Role | Modeled on |
| --- | --- | --- |
| `tracy_live_game_cs_loader` | Stable, loaded once via hostfxr, never reloaded, **`AllowUnsafeBlocks=true`**. `GameHost.cs`/`LoaderInterop.cs` (reload polling, same as `game_cs_loader`) **plus** `EngineApi.cs`/`Engine.cs` — the only code that touches raw pointers, see "Sandboxing" below. | `game_cs_loader`, extended |
| `tracy_live_game_cs` | The reloadable assembly, **`AllowUnsafeBlocks=false`** (i.e. omitted — the compiler default). `Components.cs` (blittable structs — plain structs, no unsafe needed), `Systems.cs` (**the file you edit** — `MovementSystem`/`HealthDecaySystem`/`GravitySystem`, calling `Engine.Positions()` etc. from the loader project), `Interop.cs` (`[UnmanagedCallersOnly] Init(IntPtr)`/`Update(float)` — `IntPtr` instead of `EngineApi*`, so the signature itself doesn't require `unsafe`). | `game_cs`, split for sandboxing |

Reload mechanics are otherwise identical to today's C# Flappy path: no
Rust-side file watcher, you run `dotnet build examples/tracy_live_game_cs -c
Release` (or `dotnet watch build`) in another terminal after editing
`Systems.cs`, and the loader picks it up within ~0.5s.

### CLI flags

`--rs_scripting` / `--cs_scripting` on `tracy_live` (manual `std::env::args()`
parsing, no new dependency). Passing both is an error; passing neither
defaults to `--rs_scripting` with a printed note either way.

## Known trade-off

C# systems run single-threaded inside one `Update(dt)` call — no Rayon
parallelism the way the native Rust systems get from the scheduler. Expect
noticeably lower FPS than `--rs_scripting` at the same entity count. This is
an accepted, documented cost of the demo, not a bug.

## Sandboxing: containing script crashes

A priority above performance: if `Systems.cs` — the file meant to be edited
constantly — has a bug, that must not crash the host process. Two layers,
one already implicit in the design above, one new:

**1. try/catch at every native↔managed boundary crossing.** `game_cs`
already does this (`Interop.cs`, `LoaderInterop.cs`): letting a managed
exception unwind across a `[UnmanagedCallersOnly]` call is fatal in .NET, so
every entry point's body is wrapped in try/catch, turning "the script threw"
into "log it, skip this frame" instead of a crash. `tracy_live_game_cs`'s
`Interop.cs` follows the same pattern. This alone catches the large majority
of ordinary bugs: null refs, a bad `Span` index, divide-by-zero, an uncaught
custom exception.

**2. Compiler-enforced no-unsafe in the assembly you actually edit.** try/
catch can't help with real memory corruption — and the only way `Systems.cs`
could cause that is by using `unsafe` to bypass `Span<T>`'s bounds checking
(raw pointer arithmetic, a mismatched length, etc.). So the projects are
split so that's not an option:

- `tracy_live_game_cs_loader` (stable, rarely touched) has
  `AllowUnsafeBlocks=true` and owns the only code that touches raw pointers:
  `EngineApi.cs` (the P/Invoke struct) and `Engine.cs` (binds the native
  pointer once and hands out safe `Span<T>`s from then on).
- `tracy_live_game_cs` (hot-reloaded, edited constantly) has
  `AllowUnsafeBlocks=false` — the compiler default, and here left off on
  purpose. `Interop.cs`'s entry points take `IntPtr`/`float` (not `EngineApi*`),
  so the signatures themselves don't need `unsafe` either. The result:
  whatever bug ends up in `Systems.cs`, the compiler physically will not let
  it construct a raw pointer, so it cannot corrupt native memory — full stop,
  not "please don't."

**What this does *not* cover**, in any .NET version, unsafe or not:
- **Stack overflow** (e.g. runaway recursion) always terminates the process —
  the CLR can't safely run a handler with no stack left.
- **Infinite loops/hangs** — `Update()` never returning freezes the host's
  main thread. Not a crash, but not recoverable without a watchdog+timeout
  running the call on its own thread, which this proposal doesn't include.

**The `--rs_scripting` path has no equivalent protection today**, and it's
worth knowing why: `Cargo.toml`'s release profile sets `panic = "abort"`, so
a panic anywhere inside the hot-reloaded Rust cdylib — even an ordinary
`unwrap()` on `None` — calls `abort()` immediately, no unwinding, no
`catch_unwind` possible, hard process kill. This isn't a gap we can close the
same way: Rust code sharing the host's address space has no boundary
between "buggy" and "corrupts the engine" the way a managed runtime does.
`--cs_scripting` sandboxes against the common failure modes; `--rs_scripting`
remains "fast, but native code, no safety net" — which is a fair trade to
present as an explicit, honest contrast between the two modes rather than
something to paper over.

If hangs and stack overflows need to be covered too (not just memory
corruption), that requires option **E** from "Alternatives considered" —
running the script in a separate process with a heartbeat/timeout so a
frozen or crashed child gets killed and restarted. That's real added
weight (IPC, serialization, a supervisor loop) and only worth it if "the
engine must never even freeze" is a hard requirement rather than "protect
against normal bugs."

## Open questions for you

1. Does design **A** (`Span<T>`) address the performance/safety concern, or
   would you rather go with **B** (batched marshaled copy) from the
   "Alternatives considered" table above?
2. OK to drop entity destruction from the C# path entirely (per "Scope cut"
   above), given it's a no-op in the existing demo anyway?
3. Any objection to the `--rs_scripting`/`--cs_scripting` default-to-`rs`
   behavior when neither flag is passed?
4. Is "sandboxed against common bugs, not against hangs/stack overflows"
   (the `try/catch` + no-unsafe split above) sufficient, or is full isolation
   against hangs (option E, a separate process) actually a hard requirement?
