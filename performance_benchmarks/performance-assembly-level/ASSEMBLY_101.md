# Assembly Analysis 101 — A Developer's Guide

How to inspect what your Rust code actually compiles to, and use that
knowledge to make it faster.  No prior assembly experience assumed.

---

## Table of Contents

1. [Why Look at Assembly?](#1-why-look-at-assembly)
2. [Generating Assembly](#2-generating-assembly)
3. [Tools](#3-tools)
4. [x86-64 Crash Course](#4-x86-64-crash-course)
5. [Reading Assembly: A Pattern-Based Approach](#5-reading-assembly)
6. [Rust-Specific Patterns](#6-rust-specific-patterns)
7. [What to Look For](#7-what-to-look-for)
8. [Optimization Signals (Cheap Wins)](#8-optimization-signals)
9. [Anti-Patterns (Things LLVM Already Fixes)](#9-anti-patterns)
10. [ECS-Specific Examples](#10-ecs-specific-examples)
11. [Workflow](#11-workflow)
12. [Reference](#12-reference)
13. [CPU Microarchitecture & Pipeline](#13-cpu-microarchitecture--pipeline)
14. [SIMD & Auto-Vectorisation Deep Dive](#14-simd--auto-vectorisation-deep-dive)
15. [LLVM IR — The Intermediate Step](#15-llvm-ir--the-intermediate-step)
16. [Link-Time Optimization (LTO) Under the Hood](#16-link-time-optimization-lto-under-the-hood)
17. [Profile-Guided Optimization (PGO)](#17-profile-guided-optimization-pgo)
18. [Writing Code LLVM CAN Optimize](#18-writing-code-llvm-can-optimize)
19. [Hot/Cold Code Splitting](#19-hotcold-code-splitting)
20. [Reading Hardware Counters](#20-reading-hardware-counters)
21. [Assembly Diffing & Bisecting Regressions](#21-assembly-diffing--bisecting-regressions)
22. [Common LLVM Missed Optimizations](#22-common-llvm-missed-optimizations)
23. [Real ECS Assembly Walkthrough](#23-real-ecs-assembly-walkthrough)
24. [Quick Reference Card](#24-quick-reference-card)

---

## 1. Why Look at Assembly?

- **Confirm LLVM is doing its job.** Most of the time it is. When it isn't,
  you need to know *why* so you can give it code it CAN optimize.
- **Identify the real bottleneck.** A function that looks innocent in Rust
  may hide a dozen branches, three calls, and a memory barrier.
- **Verify an optimization.** If you change code for performance and the
  benchmark improves, check the assembly to confirm *why* — otherwise you're
  guessing.
- **Spot "ghost work."** Code that appears to do nothing useful but survived
  dead-code elimination because of a subtle side-effect.

**When NOT to look at assembly:** when the benchmark shows < 2% difference.
Measurement noise dominates at that scale, and assembly-level changes won't
reliably move the needle.

---

## 2. Generating Assembly

### 2.1 `cargo asm` (recommended)

```bash
cargo install cargo-asm
cargo asm --lib --release ecs_hybrid::query::QueryIterMut::next
```

Flags worth knowing:

| Flag | Effect |
|------|--------|
| `--rust` | Interleave Rust source lines |
| `--intel` | Intel syntax (easier to read) |
| `--simplify` | Skip boilerplate labels |
| `-p ecs_hybrid` | Package name (needed in workspaces) |
| `--features profiling` | Build with specific features |

Most useful command:

```bash
cargo asm --lib --release --rust --intel --simplify \
    "ecs_hybrid::query::iter::QueryIterMut<Q,F>::next"
```

### 2.2 `cargo rustc -- --emit asm`

Produces a `.s` file in `target/release/deps/`:

```bash
cargo rustc --lib --release -- --emit asm
ls target/release/deps/*.s
```

The file is large (entire crate).  Search for your function name — it will
be mangled like `_ZN...`.

### 2.3 Compiler Explorer (godbolt.org)

Copy-paste a function into https://godbolt.org, select `rustc` as the
compiler, add `-C opt-level=3` flags.  Best for quick experiments and
sharing analysis.

### 2.4 Release Profile Matters

Your `Cargo.toml` release profile controls codegen:

```toml
[profile.bench]
opt-level = "s"      # "s" = size, "3" = speed, 0-3
lto = "fat"          # fat LTO = cross-crate inlining
codegen-units = 1    # 1 CGU = maximum optimization
strip = true
panic = "abort"
```

- `opt-level=3` maximises speed (aggressive inlining, loop unrolling,
  vectorisation).
- `opt-level="s"` balances speed and size — often within 2% of `3` but
  produces smaller binaries.
- `lto="fat"` is critical: without it, generic functions (`QueryTarget`,
  `QueryFilter`) cannot be inlined across crate boundaries, and the hot
  loop stays polymorphic with virtual calls.
- `codegen-units=1` gives LLVM the whole program at once.  With >1 CGU,
  some inlining opportunities are lost.

---

## 3. Tools

| Tool | Purpose |
|------|---------|
| `cargo asm` | View specific function's assembly |
| `objdump -d` | Disassemble entire binary |
| `cargo bloat` | Find largest functions by code size |
| **Compiler Explorer** | Interactive assembly exploration |
| `perf record` / `perf annotate` | Profile + disassemble hot instructions |
| **Tracy** | Timeline profiling (already integrated) |

---

## 4. x86-64 Crash Course

### 4.1 Registers

Registers are the CPU's "scratchpad" — 16 named slots that hold values
being worked on.  Think of them as local variables in hardware.

| 64-bit | 32-bit | Purpose |
|--------|--------|---------|
| `rax` | `eax` | Return value, accumulator |
| `rbx` | `ebx` | Callee-saved (preserved across calls) |
| `rcx` | `ecx` | 4th argument, loop counter |
| `rdx` | `edx` | 3rd argument |
| `rsi` | `esi` | 2nd argument |
| `rdi` | `edi` | 1st argument |
| `r8`-`r15` | `r8d`-`r15d` | Extra args, general purpose |
| `rsp` | `esp` | Stack pointer |
| `rbp` | `ebp` | Frame pointer (often omitted) |

### 4.2 Common Instructions

```
mov  rax, rbx     ; rax = rbx                       (copy)
add  rax, 8       ; rax = rax + 8                   (add)
sub  rsp, 40      ; rsp = rsp - 40                  (stack space)
cmp  rax, rcx     ; set flags based on rax - rcx    (compare)
je   .label       ; jump if equal (flags from cmp)  (branch)
jne  .label       ; jump if not equal
ja   .label       ; jump if above (unsigned >)
jl   .label       ; jump if less (signed <)
lea  rax, [rcx+8] ; rax = rcx + 8 (no memory access) (address calc)
call function     ; push return addr, jump to fn    (call)
ret               ; pop return addr, jump back       (return)
test rax, rax     ; set flags based on rax & rax     (is zero?)
xor  eax, eax     ; eax = 0 (idiom for zeroing)     (clear)
```

### 4.3 Memory Addressing

```
[rax]          ; value at address in rax
[rax + 8]      ; value at rax + 8
[rax + rcx*4]  ; value at rax + rcx*4 (array indexing)
```

### 4.4 Calling Convention (System V AMD64)

- First 6 integer args: `rdi, rsi, rdx, rcx, r8, r9`
- First 8 float args: `xmm0`-`xmm7`
- Return value: `rax` (integer) or `xmm0` (float)
- Stack must be 16-byte aligned before `call`

---

## 5. Reading Assembly: A Pattern-Based Approach

Don't try to understand every instruction.  Look for **patterns**.

### Pattern 1: Function Prologue / Epilogue

```asm
push rbp              ; save old frame pointer
mov  rbp, rsp         ; set new frame pointer
sub  rsp, 48          ; allocate 48 bytes of stack locals
...
add  rsp, 48          ; deallocate stack
pop  rbp              ; restore old frame pointer
ret                   ; return
```

If a function has NO prologue/epilogue, LLVM inlined it.

### Pattern 2: Loop

```asm
xor  eax, eax         ; i = 0
.Lloop:
    mov  rcx, [rdi+rax*8]  ; load arr[i]
    add  rax, 1            ; i++
    cmp  rax, rdx          ; i < len?
    jb   .Lloop            ; if below, loop
; done
```

Key signs of a well-optimized loop:
- Single `cmp` + `jb` at the bottom (not mid-loop branches)
- No `call` instructions inside (everything inlined)
- `xmm` registers for floats (SIMD-capable)

### Pattern 3: Bounds Check (often eliminated)

```asm
; Rust: vec.get(i)
cmp  rax, [rdi+16]    ; i < vec.len?
jae  .panic_label     ; if above-or-equal, panic
mov  rcx, [rdi+8]     ; load vec.ptr
mov  rax, [rcx+rax*8] ; load vec[i]
```

If you see NO `cmp`/`jae` before the load, LLVM eliminated the bounds
check — it proved `i < len` from context.  This is the default for
well-structured loops.

### Pattern 4: Virtual Call (expensive)

```asm
mov  rax, [rdi]       ; load vtable pointer
call [rax+24]         ; call through vtable (indirect)
```

If you see `call [reg+offset]`, it's a dynamic dispatch.  These are
slow because the CPU can't predict the target and can't inline the body.

### Pattern 5: Inlined Generic (cheap)

```asm
; Rust: Q::fetch_with_state(state, index)
; Should compile to direct field accesses if monomorphized:
mov  rax, [rdi]       ; load state.field_0 (pointer to Position Vec)
mov  rcx, [rdi+8]     ; load state.field_1 (pointer to Velocity Vec)
movss xmm0, [rax+rsi*4]  ; load Position[index].x (float)
movss xmm1, [rcx+rsi*4]  ; load Velocity[index].x (float)
```

No `call`, no vtable — just pointer arithmetic.  This is what fat LTO
enables.

### Pattern 6: Branch in Hot Loop

```asm
.Linner:
    ; ... work ...
    test rcx, rcx         ; check filter result
    je   .Lskip           ; if filter rejected, skip
    movss [rdx], xmm0     ; store result
.Lskip:
    add  rsi, 1           ; next index
    cmp  rsi, r8          ; more?
    jb   .Linner          ; loop
```

The `je .Lskip` in the inner loop is a per-row branch.  If the filter
accepts all rows (`F = ()`), this branch disappears entirely (LLVM sees
`ACCEPTS_ALL = true` and removes the dead branch).

---

## 6. Rust-Specific Patterns

### 6.1 `Option<T>` is null-pointer-optimised

```rust
Option<&T>      // compiles to a single pointer; None = null
Option<Box<T>>   // same
Option<NonZeroUsize>  // same, 0 = None
```

In assembly, `Option<&T>` is just a register — `test rax, rax; je .none`.

### 6.2 `unwrap()` / `expect()` — panic path is cold

```rust
let x = opt.unwrap();
```

Compiles to:
```asm
test rax, rax       ; is None?
je   .panic_label   ; cold, never taken in practice
; use value in rax
```

The panic path gets moved far away from the hot code (LLVM's cold-code
separation).  Cost: one `test` + one predicted-not-taken branch (~0.5
cycles).

### 6.3 `Vec::push()` — amortised O(1), but visible

```asm
; Vec::push involves:
mov  rax, [rdi+8]     ; vec.len
cmp  rax, [rdi+16]    ; len == cap?
je   .grow_label      ; cold: reallocate
mov  rcx, [rdi]       ; vec.ptr
mov  [rcx+rax*8], rdx ; store new element
inc  qword [rdi+8]    ; vec.len++
```

The `.grow_label` path is cold.  If you pre-allocate with
`Vec::with_capacity(n)`, the grow branch is never taken.

### 6.4 `HashMap::get()` — multiple indirections

```asm
mov  rax, [rdi]       ; load table pointer
; ... hash computation ...
mov  rcx, [rax+rsi*8] ; load bucket
cmp  rcx, rdx         ; compare keys
jne  .probe_next      ; collision chain
```

Hash map lookups involve hash computation + bucket probing.  Not cheap.
Prefer direct indexing (`Vec`, array) in hot paths.

### 6.5 `Arc::clone()` — atomic increment

```asm
lock inc dword [rdi+8]  ; atomic increment of ref count
```

The `lock` prefix is a full memory barrier (~20-50 cycles).  Avoid in
hot loops.

---

## 7. What to Look For

### Sign 1: `call` instructions in the hot loop

Every `call` pushes a return address, jumps, and returns.  More importantly,
it blocks inlining.  If you see `call` inside the innermost loop, find out
what's being called and whether it can be inlined.

```bash
cargo asm --lib --release "my_crate::hot_function" | grep -c '\bcall\b'
```

Aim for **zero** calls in the per-entity loop body.

### Sign 2: `lock` prefix (atomic operations)

Any `lock cmpxchg`, `lock inc`, `lock add` is a memory barrier.  If you
see them in the hot path and you expected lock-free code, something is
wrong — probably an `Arc::clone` or `Mutex::lock` that escaped.

### Sign 3: Many `cmp` + branch pairs

Each branch is a potential misprediction (~15-20 cycles).  LLVM often
converts predictable branches to conditional moves (`cmov`), which are
faster.  If you see branches where you expected `cmov`, the condition
might be too complex for LLVM to hoist.

### Sign 4: Register spill to stack

```asm
mov  [rsp+24], rax    ; spill to stack
; ... 20 instructions ...
mov  rax, [rsp+24]    ; reload from stack
```

The function has more live variables than registers (register pressure).
Consider splitting the function or reducing local state.

### Sign 5: SIMD instructions (`movaps`, `addps`, `mulps`)

These operate on 4 or 8 floats at once.  If your float loop has only
scalar instructions (`movss`, `addss`, `mulss`), LLVM couldn't vectorise
it.  Reasons include: non-contiguous memory access, loop-carried
dependencies, or complex control flow.

---

## 8. Optimization Signals (Cheap Wins)

### Signal: `format!()` or `write!()` in hot path

```asm
call alloc::fmt::format  ; heap allocation in hot loop!
```

Fix: defer formatting to cold paths, use `format_args!` (zero-alloc),
or eliminate entirely.

### Signal: `memcpy` / `memmove` call

```asm
call memcpy
```

A large struct is being copied.  Consider passing by reference or using
`Cow`.

### Signal: Loop body much larger than expected

The inner loop body should be ~5-20 instructions for simple operations.
If it's 100+, something got inlined that shouldn't be, or there are
multiple function calls that couldn't be inlined.

### Signal: Repeated `mov` of the same address

```asm
mov rax, [rdi+8]
; ... 5 instructions ...
mov rax, [rdi+8]   ; same load again — LLVM couldn't cache it
```

This happens when the compiler can't prove the value hasn't changed
(aliasing).  Raw pointers are a common cause — the compiler must assume
anything could alias anything through a `*mut T`.

---

## 9. Anti-Patterns (Things LLVM Already Fixes)

Don't hand-optimise these — LLVM does it for you:

| You write | LLVM produces | Don't bother |
|-----------|--------------|--------------|
| `Vec::get(i).unwrap()` | Direct `mov [ptr+i*N]` | Bounds check eliminated |
| `Option::unwrap()` | `test reg; je panic` + cold panic | Branch predicted not-taken |
| `for i in 0..n { arr[i] }` | Loop with induction variable | Already optimal |
| `match` on enum with 2 variants | Single `cmp` + branch | Don't replace with `if` |
| `Box::new(T{...})` | Often stack-allocated | Allocation elided |
| `iter().map().collect()` | Inline loop | Iterator overhead zero |
| Small `Vec::clone()` | Often elided entirely | LLVM tracks ownership |

**The rule:** if you're optimising something from the left column, stop.
Benchmark first.  Only act if the benchmark says the right column is wrong.

---

## 10. ECS-Specific Examples

### 10.1 The Query Hot Loop

```bash
cargo asm --lib --bench --rust --intel --simplify \
    "ecs_hybrid::query::iter::QueryIterMut<(ecs_hybrid::query::target::&Position, ecs_hybrid::query::target::&Velocity), ()> as core::iter::Iterator>::next"
```

What to look for in the output:

1. **Zero `call` instructions in the loop body.** If you see any, a
   function wasn't inlined (check that `Q::fetch_with_state` and
   `F::matches` are monomorphised to direct code).

2. **Two `movss` loads for two f32 reads.** This is `pos.x` and `vel.x`
   being loaded from component storage.

3. **No bounds check before the load.** `get_unchecked` or LLVM-proven
   bounds elimination.

4. **The `cmp` + `jb` for the loop counter.** Single branch at the bottom,
   no mid-loop branches for `F = ()`.

### 10.2 Filter Path (Changed<T>)

```bash
cargo asm --lib --bench --rust --intel \
    "ecs_hybrid::query::filter::TickFilterState::matches"
```

Look for:
1. Two `cmp` instructions (the tick range check: `tick > last_run && tick <= this_run`).
2. The `matches` function should be ~5 instructions total.  If it's more,
   the `unsafe` pointer access isn't being optimised.

### 10.3 Parallel Dispatch

```bash
cargo asm --lib --bench --rust --intel \
    "ecs_hybrid::engine::Engine::run_systems_parallel"
```

Focus on the closure passed to `rayon::scope`.  It should:
1. Read `work_ref` and `ranges_ref` by reference (no `Arc::clone` inside
   the loop).
2. Call `Q::fetch_with_state` directly (inlined).
3. Have no `lock` prefix (no atomic operations per entity).

---

## 11. Workflow

```
1. cargo bench -- --save-baseline before
2. carg asm ... > before.s           # snapshot assembly
3. [make code change]
4. cargo bench -- --baseline before  # compare
5. cargo asm ... > after.s           # snapshot new assembly
6. diff before.s after.s             # confirm change
```

**Golden rule:** Always pair an assembly change with a benchmark.
Assembly that "looks better" doesn't always run faster (instruction
scheduling, cache effects, branch predictor state).

### What to diff

Don't diff the entire output — focus on the **hot loop body**.  Find it
by looking for the `cmp`/`jb` loop branch and the code between it and the
loop label.

### How to verify an optimization

1. **Before:** `cargo asm` shows `call` in hot loop → benchmark: 100 µs.
2. **Change:** restructure to enable inlining.
3. **After:** `cargo asm` shows no `call` → benchmark: 85 µs (−15%).
4. **Confidence:** Both the assembly AND the benchmark agree.  This is
   as certain as you can get without an electron microscope.

If only the assembly improves but the benchmark doesn't move → your change
didn't affect the bottleneck.  The bottleneck is elsewhere (memory,
cache, branch mispredicts).

---

## 12. Reference

### Quick Lookup: Instruction Speed

Ballpark latencies for modern x86-64 (Zen 4 / Raptor Lake):

| Instruction | Latency | Notes |
|------------|---------|-------|
| `mov reg, reg` | 0-1 | Renamed, essentially free |
| `mov reg, [mem]` | 4-5 (L1), 12-15 (L2), 40+ (L3) | Depends on cache |
| `add/sub/and/or/xor` | 1 | ALU ops |
| `mul/imul` | 3-4 | Integer multiply |
| `div/idiv` | 15-25 | Avoid in hot paths |
| `cmp` + `jcc` (taken) | 1 + ~15-20 | Mispredict penalty |
| `cmp` + `jcc` (not taken) | 1 + 0.5 | Predicted correctly |
| `call` / `ret` | ~2-3 each | Plus branch predictor overhead |
| `lock inc` | ~20-50 | Full memory barrier |
| `movss` / `addss` | 3-4 | Scalar float |
| `movaps` / `addps` | 3-4 | 4× float SIMD |
| `movaps` / `addps` (AVX2) | 3-4 | 8× float SIMD |

### Useful `cargo asm` invocations

```bash
# A specific function
cargo asm --lib --release --rust --intel \
    "ecs_hybrid::query::query::Query<Q,F>::iter_mut"

# A trait method impl
cargo asm --lib --release --rust --intel \
    "<ecs_hybrid::query::iter::QueryIterMut<Q,F> as Iterator>::next"

# Monomorphised for concrete types
cargo asm --lib --release --rust --intel \
    "ecs_hybrid::query::iter::QueryIterMut<(&Position,&Velocity),()>::next"

# With profiling features
cargo asm --lib --release --features profiling --rust --intel \
    "ecs_hybrid::engine::Engine::process_frame"

# The entire module (large output)
cargo asm --lib --release ecs_hybrid::query::iter
```

### Further Reading

- [Compiler Explorer](https://godbolt.org) — Interactive assembly exploration
- [Agner Fog's optimisation guides](https://www.agner.org/optimize/) — Instruction tables, microarchitecture
- [The Rust Performance Book](https://nnethercote.github.io/perf-book/) — Rust-specific profiling
- `cargo asm --help` — More flags and filtering options

---

## 13. CPU Microarchitecture & Pipeline

Understanding the CPU pipeline turns assembly reading from "guesswork"
into "arithmetic."  You can count cycles and predict bottlenecks.

### 13.1 The Execution Pipeline (Simplified)

```
Fetch → Decode → Rename → Dispatch → Execute → Retire
  │        │        │         │          │          │
  │        │        │         │    ┌─────┼─────┐    │
  │        │        │         │    │ ALU │ FPU │    │
  │        │        │         │    │  ×4 │  ×2 │    │
  │        │        │         │    └─────┴─────┘    │
  │        │        │         │    │ Load │ Store │  │
  │        │        │         │    │  ×2  │  ×1   │  │
  │        │        │         │    └──────┴───────┘  │
```

Modern CPUs (Zen 4, Raptor Lake) can execute **4-6 instructions per cycle**
(IPC) thanks to:
- **Superscalar execution** — multiple execution units running in parallel
- **Out-of-order execution** — instructions execute when their operands are
  ready, not in program order
- **Register renaming** — eliminates false dependencies (same register
  reused doesn't create a data hazard)

### 13.2 What Limits IPC

| Bottleneck | Symptom | Typical IPC |
|------------|---------|-------------|
| **Data dependencies** | Every instruction waits for the previous one | 1.0-1.5 |
| **Cache misses** | `mov reg, [mem]` stalls for 40+ cycles | 0.5-1.0 |
| **Branch mispredicts** | Pipeline flushes, 15-20 cycles wasted | 1.5-2.0 |
| **Port contention** | Too many instructions want the same execution unit | 2.0-3.0 |
| **Well-optimised** | Independent work, good caching | 3.0-4.0 |

### 13.3 How to Estimate Cycle Count from Assembly

Given this loop body:

```asm
.Linner:
    movss  xmm0, [rdi+rsi*4]   ; load pos.x      — 5 cycles (L1 hit)
    movss  xmm1, [rdx+rsi*4]   ; load vel.x      — 5 cycles
    addss  xmm0, xmm1          ; pos.x + vel.x   — 3 cycles
    movss  [rcx], xmm0         ; store result     — 1 cycle (store buffer)
    add    rsi, 1              ; i++              — 1 cycle
    cmp    rsi, r8             ; i < len?         — 1 cycle
    jb     .Linner             ; loop             — 1 cycle (predicted)
```

**Cycle analysis:**

1. `movss` from L1: 5 cycles each, but they're independent — both start
   simultaneously.  Effective latency: 5 cycles for the pair.
2. `addss` depends on both loads → starts after loads complete: +3 cycles.
3. `movss` store: processed by store buffer, doesn't block subsequent work.
4. `add` + `cmp` + `jb`: 1 cycle each, executed in parallel with the float
   work since they use different ports (ALU vs FPU).

**Per-iteration cost:** ~8 cycles for the float work, ~1 cycle for the
integer loop overhead (hidden behind float work due to superscalar
execution).  **Total: ~8 cycles per entity.**

At 4 GHz: 8 cycles / 4 GHz = 2 ns per entity.  For 100K entities:
200 µs.  Compare to benchmark.  If benchmark shows 90 µs, the CPU is
doing better than our estimate (likely because two iterations' float
work overlap in the pipeline).  If benchmark shows 400 µs, something
else is going on (cache misses, branch mispredicts, OS interference).

### 13.4 Cache Hierarchy

```
L1 Data: 32 KB,  4-5 cycles,  per-core
L2:      256 KB-1 MB, 12-15 cycles, per-core
L3:      8-36 MB, 40-60 cycles, shared across all cores
RAM:     gigabytes, 100-300 cycles (~50-100 ns)
```

**How to spot cache effects in assembly:**

You can't — assembly doesn't tell you whether `[rdi+rsi*4]` hits L1 or
misses to RAM.  You need hardware counters for that (§21).  But you CAN
estimate: if your data set is 1.6 MB (100K Position × 16 bytes) and L2
is 1 MB, the inner loop will miss L2 after the first ~64K iterations and
pay L3 latency for the rest.

**Rule of thumb for this ECS:**
- Component storage is SoA (Structure of Arrays) — sequential access
- With 100K entities × 8 bytes per component = 800 KB per column
- Two columns (Position + Velocity) = 1.6 MB working set
- Fits in L3 on most CPUs, spills to RAM on smaller L3 configurations
- The hardware prefetcher handles sequential access well, so effective
  latency is closer to L2 than L3

---

## 14. SIMD & Auto-Vectorisation Deep Dive

### 14.1 What SIMD Looks Like

**Scalar** (one float at a time):
```asm
movss  xmm0, [rdi]      ; load one f32
addss  xmm0, [rsi]      ; add one f32
movss  [rdx], xmm0       ; store one f32
```

**SIMD** (four floats at a time):
```asm
movaps xmm0, [rdi]      ; load four f32 (128-bit)
addps  xmm0, [rsi]      ; add four f32 in parallel
movaps [rdx], xmm0       ; store four f32
```

**AVX2** (eight floats at a time):
```asm
vmovaps ymm0, [rdi]     ; load eight f32 (256-bit)
vaddps  ymm0, ymm0, [rsi] ; add eight f32
vmovaps [rdx], ymm0      ; store eight f32
```

### 14.2 When LLVM Can Auto-Vectorise

LLVM auto-vectorises loops when ALL of these hold:

1. **The loop is countable** — trip count known before loop starts
2. **No loop-carried dependencies** — iteration N doesn't depend on
   iteration N-1
3. **Contiguous memory access** — `arr[i]`, `arr[i+1]`, `arr[i+2]`, `arr[i+3]`
   are adjacent in memory
4. **No function calls** in the loop body
5. **No branches** in the loop body (or branches that can be predicated)
6. **Alignment is known or can be handled** — `arr.as_ptr()` alignment

**Example that vectorises:**
```rust
for i in 0..len {
    result[i] = a[i] + b[i];  // contiguous, no deps, no calls, no branches
}
```

**Example that does NOT vectorise:**
```rust
for i in 0..len {
    if filter.matches(i) {       // branch!
        result[i] = a[i] + b[i];
    }
}
```

### 14.3 Forcing Vectorisation

```rust
// Hint alignment to LLVM
assert!(ptr as usize % 32 == 0);  // 32-byte aligned for AVX2

// Use chunks_exact for guaranteed vectorisation
for chunk in data.chunks_exact(8) {
    // LLVM sees 8-element batches, maps to AVX2 naturally
}
```

### 14.4 Detecting Failed Vectorisation

```bash
# Emit LLVM optimization remarks
cargo rustc --lib --release -- -C remark=loop-vectorize
```

This prints messages like:
```
remark: vectorized loop (vectorization width: 4, interleaved count: 2)
remark: loop not vectorized: cannot identify array bounds
```

In `cargo asm` output, look for `movaps`/`addps` (SSE) or `vmovaps`/`vaddps`
(AVX2).  If you only see `movss`/`addss`, vectorisation failed or wasn't
attempted.

---

## 15. LLVM IR — The Intermediate Step

Between Rust source and x86-64 assembly sits LLVM IR — a lower-level
representation that's sometimes easier to reason about than raw assembly.

### 15.1 Generating LLVM IR

```bash
cargo rustc --lib --release -- --emit llvm-ir
# Output: target/release/deps/ecs_hybrid-<hash>.ll
```

Or on Godbolt: select "LLVM IR" instead of "ASM" in the output pane.

### 15.2 Key LLVM IR Patterns

```
; A function
define i64 @add(i64 %a, i64 %b) {
  %sum = add i64 %a, %b
  ret i64 %sum
}

; A loop
br label %loop_header
loop_header:
  %i = phi i64 [0, %entry], [%next, %loop_body]
  %cond = icmp slt i64 %i, %n
  br i1 %cond, label %loop_body, label %exit
loop_body:
  %ptr = getelementptr float, ptr %arr, i64 %i
  %val = load float, ptr %ptr
  ; ... work ...
  %next = add i64 %i, 1
  br label %loop_header
exit:
  ret void
```

### 15.3 Why Look at LLVM IR?

- **Inlining decisions are visible** — `call` vs no `call` in IR tells you
  whether a function was inlined before reaching the backend.
- **Loop structure is explicit** — the `phi` node shows the loop
  induction variable and its update.
- **Aliasing annotations** — `noalias`, `dereferenceable`, `align` tell you
  what LLVM knows about pointer provenance.  Missing annotations mean
  LLVM must assume worst-case aliasing.
- **Vectorisation remarks** — easier to spot in IR than in final assembly
  because the wide loads/stores are explicit.

### 15.4 Common Aliasing Annotations

```llvm
; A pointer that doesn't alias anything else
%ptr = load ptr, ptr %p, !noalias !0

; A pointer known to be dereferenceable for N bytes
%ptr = load ptr, ptr %p, !dereferenceable !{i64 800000}

; A pointer with known alignment
%ptr = load ptr, ptr %p, align 8
```

If these are missing in your hot function's IR, LLVM has to emit
conservative code (redundant loads, no vectorisation).  Raw pointers
(`*mut T`, `*const T`) lose these annotations — that's why `&mut T`
is preferred when possible.

---

## 16. Link-Time Optimization (LTO) Under the Hood

### 16.1 What LTO Actually Does

Without LTO, each crate is compiled to object code independently.
Generic functions (`impl<T> QueryTarget for &T`) are compiled in the
crate that *defines* them, not the crate that *uses* them.  The using
crate sees only a symbol, not the body — so no inlining across crate
boundaries.

With `lto = "fat"` (or `lto = true`), LLVM merges all crates' IR into a
single module and re-optimises.  This enables:

1. **Cross-crate inlining** — `Query::iter_mut()` can inline
   `VecStorage::get()` from trait_type_map.
2. **Dead code elimination across crates** — methods never called in the
   final binary are removed.
3. **Constant propagation across crates** — `DEFAULT_SLICE_ENTITIES = 4096`
   becomes a literal in every call site.

### 16.2 ThinLTO vs Fat LTO

| | ThinLTO | Fat LTO |
|---|---|---|
| **Compile time** | Fast (parallel) | Slow (serial) |
| **Memory** | Low per-module | Entire program in memory |
| **Optimization quality** | Good (95% of fat) | Best |
| **Use case** | Development, CI | Release builds |

### 16.3 Verifying LTO Is Working

Pick a function you know should be inlined (e.g., `VecStorage::get`).
Without LTO, `cargo asm` for your hot function shows `call` to
`VecStorage::get`.  With LTO, the body of `VecStorage::get` appears
directly in the caller's output — no `call`, just a `mov` from the Vec's
data pointer.

```bash
# Without LTO (should show call)
cargo asm --lib --release --no-default-features \
    "ecs_hybrid::query::QueryIterMut<...>::next" | grep -c call

# With LTO (should show 0 calls in hot loop)
cargo asm --lib --release \
    "ecs_hybrid::query::QueryIterMut<...>::next" | grep -c call
```

---

## 17. Profile-Guided Optimization (PGO)

### 17.1 What PGO Does

PGO runs your program with instrumentation, collects branch frequencies
and call counts, then recompiles using that data.  The compiler can then:

- **Reorder basic blocks** — hot path contiguous, cold path out of the way
- **Inline more aggressively** in hot call paths
- **Optimise branch prediction hints** — default predict the common case
- **Register-allocate** hot variables preferentially
- **Devirtualise** — if 99% of calls go to one impl, speculate and
  inline with a guard

### 17.2 How to Use PGO with Rust

```bash
# Step 1: Build with instrumentation
RUSTFLAGS="-C profile-generate=/tmp/pgo-data" cargo build --release

# Step 2: Run representative workloads
./target/release/your_binary --benchmark-heavy
./target/release/your_binary --benchmark-light

# Step 3: Merge profiling data
llvm-profdata merge -o /tmp/pgo-data/merged.profdata /tmp/pgo-data/*.profraw

# Step 4: Rebuild with profile data
RUSTFLAGS="-C profile-use=/tmp/pgo-data/merged.profdata" cargo build --release
```

### 17.3 Expected Gains

For CPU-bound, branch-heavy code (like an ECS query loop with filters),
PGO typically gives 5-15% improvement.  For memory-bound code, the
gains are smaller (0-5%) because the bottleneck is cache, not layout.

### 17.4 PGO + LTO Interaction

PGO and fat LTO work together: PGO tells LLVM *what* to inline, LTO gives
it the *ability* to inline across crates.  Using both typically yields
better results than either alone.

---

## 18. Writing Code LLVM CAN Optimize

LLVM is powerful but conservative.  It must preserve *all observable
behaviour* of your program.  Some Rust patterns accidentally prevent
optimisation.

### 18.1 Kill Aliasing with `&mut` Not `*mut`

```rust
// BAD: raw pointer — LLVM must assume it aliases EVERYTHING
unsafe {
    let world: *mut World = ...;
    (*world).entities.push(entity); // may alias anything
}

// GOOD: exclusive reference — LLVM knows it's the only pointer
let world: &mut World = ...;
world.entities.push(entity);  // noalias annotation present
```

Raw pointers are `noalias` poison.  A single `*mut T` in a function can
disable vectorisation, prevent load hoisting, and force redundant
re-reads throughout the entire function.  **Use raw pointers as late as
possible** — convert `&mut` to `*mut` at the last moment before passing
to a Rayon closure, not at function entry.

### 18.2 Give LLVM Bounds Information

```rust
// BAD: LLVM doesn't know len
let data: &[f32] = ...;
for i in 0..data.len() {  // LLVM must re-check len() each iteration?
    let x = data[i];      // bounds check needed?
}

// GOOD: LLVM sees the relationship
let data: &[f32] = ...;
for chunk in data.chunks_exact(8) {  // LLVM knows exactly 8 elements
    // vectorises naturally
}
```

### 18.3 Avoid `fn()` Pointers in Hot Paths

```rust
// BAD: indirect call through function pointer
type Copier = fn(&TypeMap, &mut TypeMap, usize, usize);
let copier: Copier = ...;
copier(src, dst, idx);  // call [rax+offset] — unpredictable

// GOOD: generic (monomorphised)
fn copy<T: Component>(src: &TypeMap, dst: &mut TypeMap, idx: usize) {
    // compiles to direct code, inlinable
}
```

Function pointers prevent inlining and cause indirect branch penalties.
Generics eliminate both.

### 18.4 Struct Layout Matters

```rust
// BAD: padding wastes cache
struct BadLayout {
    flag: bool,    // 1 byte + 3 padding
    value: f64,    // 8 bytes
    count: u32,    // 4 bytes + 4 padding (to align next field)
}  // 24 bytes

// GOOD: sorted by size
struct GoodLayout {
    value: f64,    // 8 bytes
    count: u32,    // 4 bytes
    flag: bool,    // 1 byte + 3 padding
}  // 16 bytes
```

Run `cargo check -- -Z print-type-sizes` (nightly) to see struct layouts.

---

## 19. Hot/Cold Code Splitting

### 19.1 `#[cold]` and `#[inline(never)]`

```rust
#[cold]  // Move this function far away from hot code
fn handle_error() -> ! { panic!("unreachable"); }

#[inline(never)]  // Never inline — keep icache clean
fn advance_archetype(&mut self) -> Option<()> { ... }
```

`#[cold]` tells LLVM this function is rarely called.  LLVM:
- Moves the function's code to a separate section, far from the caller
- Optimises for size (reduces pressure on instruction cache)
- Marks the call site's path as cold (helps branch predictor)

`#[inline(never)]` prevents the function body from being duplicated into
every call site.  Use when:
- The function is large and called from many places
- The function is rarely taken (cold path in a hot loop)
- Inlining would bloat icache and hurt more than it helps

### 19.2 Verifying Cold Code Placement

```bash
objdump -t target/release/ecs_hybrid | grep handle_error
# Look for the function address — should be far from hot code

perf report --stdio  # Shows instruction cache misses by function
```

### 19.3 The `likely!` / `unlikely!` Hint (Nightly)

```rust
#![feature(core_intrinsics)]
use std::intrinsics::{likely, unlikely};

if unlikely(entity.is_empty()) {  // hint: rarely true
    return;
}
// hot path follows
```

Compiles to a branch with a static prediction hint, which the CPU uses
on first encounter (before dynamic predictor kicks in).  Useful for
eliminating cold-start branch mispredicts.

---

## 20. Reading Hardware Counters

Hardware performance counters tell you *why* code is slow — cache misses,
branch mispredicts, stalled cycles.  They're the ground truth behind
assembly analysis.

### 20.1 Linux (perf stat)

```bash
perf stat -e cycles,instructions,cache-references,cache-misses,\
branch-misses,bus-cycles \
    cargo bench --bench query_iteration -- query_iter_unfiltered/100000
```

### 20.2 Key Metrics

| Counter | What It Means |
|---------|---------------|
| `cycles` | Total CPU cycles consumed |
| `instructions` | Total instructions retired |
| `IPC = instructions/cycles` | Instructions per cycle — < 1.0 means stalled |
| `cache-misses` | L3 (last-level cache) misses |
| `cache-miss-rate = misses/references` | > 5% means memory-bound |
| `branch-misses` | Mispredicted branches |
| `branch-miss-rate = misses/branches` | > 2% means unpredictable branches |
| `stalled-cycles-frontend` | Cycles waiting for instructions (icache miss, decoder) |
| `stalled-cycles-backend` | Cycles waiting for data (dcache miss, full store buffer) |

### 20.3 Quick Diagnosis Table

| Symptom | Metric | Fix |
|---------|--------|-----|
| Low IPC (< 1.5) | `instructions` / `cycles` | Investigate stalls |
| High cache miss rate | `cache-misses` / `cache-references` > 5% | Reduce working set, SoA layout |
| High branch miss rate | `branch-misses` / `branches` > 2% | Simplify conditions, sort data |
| Frontend bound | `stalled-cycles-frontend` high | Reduce code size, fewer `call` sites |
| Backend bound | `stalled-cycles-backend` high | Better data layout, prefetching |
| Good IPC (> 3.0) | Everything fine | You're done — go higher-level |

### 20.4 Windows Equivalent

Windows doesn't have `perf stat`.  Alternatives:
- **Windows Performance Recorder (WPR)** + **Windows Performance Analyzer (WPA)**
- **Intel VTune** — free for non-commercial use, excellent UI
- **AMD uProf** — for AMD CPUs
- **Tracy** — shows CPU time per zone (already integrated)

---

## 21. Assembly Diffing & Bisecting Regressions

### 21.1 Diffing Before/After Assembly

```bash
# Generate before
git stash
cargo asm --lib --release --rust --intel \
    "ecs_hybrid::query::QueryIterMut<...>::next" > /tmp/before.s
git stash pop

# Generate after
cargo asm --lib --release --rust --intel \
    "ecs_hybrid::query::QueryIterMut<...>::next" > /tmp/after.s

# Diff
diff -u /tmp/before.s /tmp/after.s
```

### 21.2 What Changes Matter

| Diff | Impact |
|------|--------|
| `call` appeared/disappeared | **High** — function no longer inlined |
| `lock` prefix appeared | **High** — new atomic operation |
| Loop body grew/shrunk | **Medium** — may affect icache |
| Register names changed | **None** — just allocation differences |
| `mov [rsp+X]` spill count changed | **Low/Medium** — register pressure change |
| `jmp` → `je`/`jne` change | **Medium** — branch direction changed |

### 21.3 Bisecting a Regression to a Commit

```bash
# Find where the regression was introduced
git bisect start
git bisect bad HEAD
git bisect good <last-known-good-commit>
# Git checks out a midpoint commit
cargo bench --bench query_iteration -- query_iter_unfiltered/100000
# Based on result: git bisect good  OR  git bisect bad
# Repeat until the guilty commit is found

git bisect reset
cargo asm --lib --release \
    "ecs_hybrid::query::QueryIterMut<...>::next" > regression.s
git diff <guilty-commit>^ <guilty-commit> -- src/  # See what changed
```

---

## 22. Common LLVM Missed Optimizations

Sometimes LLVM *could* optimize but doesn't.  Here's when to intervene.

### 22.1 Loop-Invariant Code Not Hoisted

```rust
// LLVM might not hoist this because it can't prove
// world.archetypes doesn't change
for entity in 0..count {
    let archetype = world.archetypes.get(&id);  // HashMap lookup per iteration!
}

// Fix: hoist manually
let archetype = world.archetypes.get(&id);
for entity in 0..count {
    // use archetype
}
```

### 22.2 Slice Bounds Check Not Eliminated

```rust
// LLVM can't prove the slice index is in bounds
fn get(slice: &[f32], index: usize) -> f32 {
    slice[index]  // bounds check here
}

// Fix: use iterators (LLVM understands them)
fn get(slice: &[f32], index: usize) -> f32 {
    slice.iter().nth(index).copied().unwrap_or(0.0)
}
```

### 22.3 Auto-Vectorisation Blocked by `fence` or Atomic

Any `Ordering::SeqCst` fence in the loop body blocks vectorisation
for the entire function.  Use `Ordering::Relaxed` or `Ordering::AcqRel`
when the strong ordering isn't needed.

### 22.4 Excess Register Spilling

If you see many `mov [rsp+N], reg` / `mov reg, [rsp+N]` pairs in the
hot loop, LLVM ran out of registers.  Reduce the number of live variables:
- Split large functions into smaller ones
- Extract cold paths into separate functions (`#[inline(never)]`)
- Reduce temporary variables

---

## 23. Real ECS Assembly Walkthrough

Let's trace through the actual hot path of `query_iter_unfiltered/100000`.

### 23.1 Generate the Assembly

```bash
cargo asm --lib --bench --rust --intel --simplify \
    "ecs_hybrid::query::iter::QueryIterMut<(&Position,&Velocity),()> as \
     core::iter::Iterator>::next" > hot_path.s
```

### 23.2 What You Should See

The innermost loop body should look approximately like this:

```asm
; Load Position.x from component storage
mov     rax, qword [rdi]        ; state.pos_ptr (from init_state)
movss   xmm0, dword [rax+rsi*4] ; load Position[index].x

; Load Velocity.x
mov     rcx, qword [rdi+8]      ; state.vel_ptr (from init_state)
movss   xmm1, dword [rcx+rsi*4] ; load Velocity[index].x

; Execute user closure body: sum += pos.x + vel.x
addss   xmm0, xmm1              ; pos.x + vel.x
addss   xmm2, xmm0              ; accumulator +=

; Loop control
add     rsi, 1                  ; index++
cmp     rsi, r8                 ; index < len?
jb      .Linner                 ; continue loop
```

### 23.3 Red Flags in This Output

| If you see... | Problem | Fix |
|---------------|---------|-----|
| `call` anywhere in the loop body | Function not inlined | Check LTO, add `#[inline]` |
| `cmp`/`jae` before the `movss` loads | Bounds check not eliminated | Check loop structure |
| `lock` prefix | Atomic op in hot path | Remove `Arc::clone` from loop |
| `mov [rsp+X]` spills | Register pressure | Reduce live variables |
| `je` mid-loop (filter branch) | Filter is active | Review if `F = ()` was monomorphised |
| No `movss`/`addss` — only integer ops | Wrong types | Check component types match query |

### 23.4 The Ideal Output

For `F = ()` (unfiltered), the inner loop should have:
- **0** `call` instructions
- **0** `lock` prefixes
- **1** conditional branch (`jb` at the bottom)
- **0** bounds checks (`cmp`/`jae` before loads)
- **2** `movss` loads (one per component per row)
- **1-2** arithmetic instructions (the user's closure)
- SSE (`movss`/`addss`) or AVX (`vmovss`/`vaddss`) instructions

Total: ~12 instructions that execute in ~8 cycles thanks to superscalar
execution.  At 4 GHz, that's 2 ns per entity × 100K = 200 µs lower
bound.  Achieving 80-90 µs means the CPU is doing even better — likely
two iterations' work in flight simultaneously.

---

## 24. Quick Reference Card

### Generate Assembly
```bash
cargo asm --lib --release --rust --intel --simplify "crate::module::function"
```

### Count Calls in Hot Loop
```bash
cargo asm ... | grep -c '\bcall\b'
```

### Count Atomic Ops in Hot Loop
```bash
cargo asm ... | grep -c '\block\b'
```

### Find Function Size
```bash
cargo asm ... | wc -l
```

### Compare Two Versions
```bash
diff -u <(cargo asm ...) <(git stash; cargo asm ...; git stash pop)
```

### LLVM IR
```bash
cargo rustc --lib --release -- --emit llvm-ir
```

### Vectorisation Remarks
```bash
cargo rustc --lib --release -- -C remark=loop-vectorize
```

### Hardware Counters (Linux)
```bash
perf stat -e cycles,instructions,cache-misses,branch-misses cargo bench -- ...
```

### Struct Layout (Nightly)
```bash
cargo +nightly check -- -Z print-type-sizes
```

---

### Further Reading

- [Compiler Explorer](https://godbolt.org) — Interactive assembly exploration
- [Agner Fog's optimisation guides](https://www.agner.org/optimize/) — Instruction tables, microarchitecture
- [The Rust Performance Book](https://nnethercote.github.io/perf-book/) — Rust-specific profiling
- [LLVM's Analysis & Transform Passes](https://llvm.org/docs/Passes.html) — What each optimisation pass does
- [Intel Intrinsics Guide](https://www.intel.com/content/www/us/en/docs/intrinsics-guide/index.html) — SIMD instruction reference
- `cargo asm --help` — More flags and filtering options
