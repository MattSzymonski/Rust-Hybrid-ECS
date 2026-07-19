# Performance Optimization 101

A hands-on field guide drawn from real optimization work on a Rust ECS library. Not theory — what actually worked, what didn't, and how to tell the difference.

---

## Table of Contents

1. [Mindset & Strategy](#1-mindset--strategy)
2. [Finding Opportunities](#2-finding-opportunities)
3. [The Optimization Workflow](#3-the-optimization-workflow)
4. [Benchmarking](#4-benchmarking)
5. [Profiling & Deep Inspection](#5-profiling--deep-inspection)
6. [Assembly Analysis](#6-assembly-analysis)
7. [Tactics & Patterns](#7-tactics--patterns)
8. [Rust-Specific Techniques](#8-rust-specific-techniques)
9. [CPU Architecture & Cache Fundamentals](#9-cpu-architecture--cache-fundamentals)
10. [Memory & Allocation Deep Dive](#10-memory--allocation-deep-dive)
11. [Parallelism & Concurrency](#11-parallelism--concurrency)
12. [SIMD & Auto-Vectorization](#12-simd--auto-vectorization)
13. [Compiler Optimizations Deep Dive](#13-compiler-optimizations-deep-dive)
14. [The "Generation Counter" Hack](#14-the-generation-counter-hack)
15. [ECS-Specific Optimization Patterns](#15-ecs-specific-optimization-patterns)
16. [Case Studies](#16-case-studies)
17. [When to Stop](#17-when-to-stop)
18. [Tool Reference](#18-tool-reference)
19. [Anti-Patterns & Lessons Learned](#19-anti-patterns--lessons-learned)

---

## 1. Mindset & Strategy

### The Golden Rule

> **Algorithmic changes beat micro-optimizations. Every time.**

During this project, optimizations that moved the needle by 5-20% were all algorithmic:
- Cloning fewer things (↓19.4%)
- Caching work that doesn't need redoing (↓18.1%)
- Precomputing static data (↓4.8%)

Micro-optimizations that moved the needle by 0%:
- Replacing `Option::unwrap()` with `unwrap_unchecked()`
- Replacing `Vec::get()` with `get_unchecked()`
- Replacing `entry().or_default()` with `get_mut().expect()`
- Hoisting raw pointers
- Rearranging struct fields

**Why?** LLVM is smarter than you. It already eliminates bounds checks, inlines cross-crate, hoists invariants, and vectorizes loops. Your job is to give it code where those optimizations are *possible*, not to do its job for it.

### The Optimization Pyramid

```
        ┌──────────────┐
        │ Algorithmic  │  ← Biggest wins. O(n²)→O(n), caching, precomputation.
        ├──────────────┤
        │  Data Layout │  ← SoA vs AoS, alignment, cache lines, size reduction.
        ├──────────────┤
        │  Allocation  │  ← Reuse buffers, avoid clones, pre-allocate.
        ├──────────────┤
        │  Branching   │  ← Predictable branches, cold-path extraction.
        ├──────────────┤
        │  Instruction │  ← SIMD, bit tricks, specialized instructions.
        └──────────────┘
```

Start at the top. Only descend when the level above is exhausted.

### Measure First, Optimize Second

Never optimize without a benchmark. Never trust intuition. The three optimizations above that showed 0% gain all *sounded* like they should help. They didn't.

---

## 2. Finding Opportunities

### The Hot-Path Heuristic

A "hot path" is code executed **per entity, per frame**. In an ECS:
- Query iteration: `for (pos, vel) in query.iter_mut()` — **hottest**
- Component access: `pos.x += vel.x` — **hot**
- Archetype matching: checking if an archetype fits a query — **warm** (only when archetypes change)
- System scheduling: building the execution graph — **cold** (once per frame, small N)
- Entity creation/destruction — **cold** (rare relative to iteration)
- Registration/setup — **frozen** (once ever)

**How to spot hot paths without a profiler:**
1. Find every `for` loop over entities
2. Find every `fn` called inside those loops
3. Those are your candidates

### The "What Changed?" Question

For any repeated operation, ask: **"What could have changed since last time?"**

- If **nothing changed**, cache the result (generation-counter pattern)
- If **something changed**, can you detect it cheaply? (dirty flag)
- If **everything changes**, focus on making the operation itself faster

### Symptoms of Waste

| Symptom | Likely Cause | Fix |
|---|---|---|
| `clone()` in a loop | Unnecessary allocation | Reference, `Cow`, or restructure |
| `HashMap::get()` in hot path | Lookup overhead | Direct indexing, precomputed indices |
| `Box::new()` or `Arc::new()` in hot path | Heap allocation | Stack allocation, pooling |
| `format!()` or `write!()` in hot path | Formatting overhead | Defer to cold path |
| Recursive or nested loops of unknown depth | Unbounded work | Flatten, batch, or cap |

---

## 3. The Optimization Workflow

### PASS.md Pattern

Every optimization attempt gets a directory with a `PASS.md` file:

```
pass_N/
    PASS.md     ← What, why, expected impact, actual result
```

The file records:
- **Hypothesis**: What change, why it should help
- **Expected impact**: Which benchmark, estimated % improvement
- **Implementation**: What files changed
- **Benchmark results**: Before/after numbers
- **Verdict**: KEPT or REVERTED, with reasoning

**Why this matters**: Six months later, you won't remember why you changed `get_mut().expect()` to `entry().or_default()`. The PASS.md tells you it was tried and failed, saving you from trying again.

### The Revert Reflex

If a change shows **no measurable improvement** or **regression**, revert it immediately. Dead code and unnecessary `unsafe` blocks accumulate technical debt. Be ruthless.

A change is only "kept" if:
1. Benchmarks show measurable improvement, **OR**
2. It simplifies the code without hurting performance

### One Change Per Pass

Never bundle multiple optimizations in one pass. If the benchmark improves, you won't know which change caused it. If it regresses, you won't know which to revert.

---

## 4. Benchmarking

### Framework: Criterion.rs

Criterion is the gold standard for Rust microbenchmarks. It handles warm-up, statistical analysis, and comparison against baselines.

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "my_benchmark"
harness = false
```

### Configuration for Fast Iteration

During development, you want fast feedback. Criterion defaults are geared toward publication-quality results (100 samples, 3s warmup, 5s measurement). For iteration:

```bash
cargo bench --bench my_bench -- \
    --sample-size 25 \
    --warm-up-time 0.5 \
    --measurement-time 2
```

Or in code (Criterion 0.5+ — on `BenchmarkGroup`, not `Criterion` struct):
```rust
group.sample_size(25);
group.warm_up_time(std::time::Duration::from_millis(500));
group.measurement_time(std::time::Duration::from_secs(2));
```

Since the `criterion_group!` macro doesn't support per-benchmark config in 0.5, use CLI flags via a runner script.

### What to Benchmark

Don't benchmark everything. Benchmark the **hot paths**:

| What | Why |
|---|---|
| Query iteration (unfiltered) | Baseline: raw throughput |
| Query iteration (filtered) | Change detection overhead |
| Query iteration (mutable) | Write-path overhead |
| Entity creation/destruction | Lifecycle cost |
| Archetype migration | Add/remove component cost |
| Full frame loop | End-to-end integration |

### Interpreting Results

- **< 2% change**: Within noise for most setups. Don't trust it.
- **2-5% change**: Real but small. Worth keeping if the code is simpler.
- **5-15% change**: Solid win. Merge it.
- **> 15% change**: Suspicious — double-check you didn't break correctness or change the workload.

**Always check variance.** If `±X` is larger than the difference between means, the result is not statistically significant.

### Criterion Output Locations

```
target/criterion/
    benchmark_name/
        size/
            base/         ← Previous baseline
            change/       ← Current run
            new/          ← Most recent data
            report/       ← HTML report
```

Compare `base/estimates.json` and `new/estimates.json` manually if the HTML report isn't generated.

### Python Benchmark Runner

For projects with many benchmark groups, a runner script is essential:

```python
GROUP_TO_BENCH = {
    "entity_lifecycle": "entity_lifecycle",
    "query_iteration": "query_iteration",
    "archetype_migration": "archetype_migration",
    "scheduler_graph": "scheduler_graph",
    "frame_loop": "frame_loop",
}
```

The runner:
1. Maps logical groups to Criterion benchmark binaries
2. Passes CLI flags for fast iteration
3. Captures both stdout and stderr (Criterion uses stdout)
4. Parses results with regex
5. Saves comparison reports

---

## 5. Profiling & Deep Inspection

### When to Profile

- When you don't know which function is the bottleneck
- When a benchmark shows a problem but you can't identify the cause
- When performance is unexpectedly bad and simple inspection doesn't reveal why

### Tools by Platform

| Platform | CPU Profiler | Assembly | Heap |
|---|---|---|---|
| **Linux** | `perf`, `flamegraph` | `cargo asm`, `objdump` | `heaptrack`, `valgrind --tool=massif` |
| **macOS** | Instruments (Xcode) | `cargo asm`, `otool -tV` | Instruments Allocations |
| **Windows** | WPR + WPA, VTune | `dumpbin /disasm`, `cargo asm` (partial) | WPR Heap traces |

### Cargo-ASM

`cargo asm` generates annotated assembly for specific functions:

```bash
cargo install cargo-asm
cargo asm --release --lib "my_crate::my_function"
```

**Limitations:**
- Generic (monomorphized) functions only appear in the binary that uses them, not the library
- Windows MSVC produces PDB debug info, which `cargo asm` struggles with
- On Windows, use the benchmark binary: `cargo asm --release --bench my_bench "function_name"`

### `perf` (Linux) — Hardware Performance Counters

The single most powerful profiling tool on Linux:

```bash
# Record CPU cycles, cache misses, branch mispredictions
perf record -e cycles,instructions,cache-misses,branch-misses \
    -g --call-graph dwarf -- ./target/release/my_binary

# Interactive analysis
perf report -g graph

# Flamegraph (requires flamegraph.pl from Brendan Gregg)
perf script | stackcollapse-perf.pl | flamegraph.pl > flamegraph.svg

# What to look for:
# - Wide bars = hot functions (lots of CPU time)
# - Tall stacks = deep call chains (consider flattening)
# - "cycles" vs "instructions" divergence = stalls (cache misses, branch mispredicts)
```

**Key `perf` events**:
| Event | What It Tells You |
|---|---|
| `cycles` | CPU wall-clock time — the ultimate metric |
| `instructions` | Actual work done. Low IPC (instructions per cycle) = stalls |
| `cache-misses` | Last-Level Cache misses → memory-bound |
| `L1-dcache-load-misses` | L1 data cache misses → layout problem |
| `branch-misses` | Mispredicted branches → unpredictable control flow |
| `cpu-migrations` | Thread bouncing between cores → pin threads |
| `context-switches` | Premature descheduling → lock contention or IO wait |

**IPC interpretation**:
- **> 2.0**: Excellent — CPU is well-fed with instructions and data
- **1.0–2.0**: Normal for most workloads
- **0.5–1.0**: Some stalls — check cache misses and branch mispredicts
- **< 0.5**: Severely stalled — memory-bound or branch-mispredict-bound

### Flamegraph Interpretation

```
main                 ████████████████████████████████████████
  frame_loop         ████████████████████████████████
    run_systems      ████████████████████
      movement       ████████
        iter_mut     ████           ← Wide here = hot query iteration
          next        ██            ← Hottest single function
      health         ███            ← Less wide = less time in this system
    update_scripts   ██             ← Narrow = cold
```

**How to read**: Width = time spent. Look for the widest bars at leaf level. Those are your optimization targets.

### Cachegrind (Linux) — Cache Miss Simulation

```bash
# Simulate L1/L2/LL cache behavior (no hardware counters needed)
valgrind --tool=cachegrind --cache-sim=yes ./target/release/my_binary

# Annotate source with cache miss counts
cg_annotate cachegrind.out.XXXXX --auto=yes > annotated.txt
```

Cachegrind simulates every memory access — it's ~20-50× slower than native but gives precise cache miss attribution to source lines. Useful when hardware counters aren't available (VMs, CI).

### macOS Instruments

```
# Profile with Time Profiler (CPU sampling)
xcrun xctrace record --template 'Time Profiler' --launch ./target/release/my_binary

# Or from Xcode: Product → Profile → Time Profiler
```

### Windows Performance Recorder (WPR)

```powershell
# Start recording
wpr -start CPU -start Heap -filemode

# Run your application
.\target\release\my_binary.exe

# Stop recording
wpr -stop trace.etl

# Open in Windows Performance Analyzer (WPA)
wpa trace.etl
```

### `cargo-flamegraph` (All Platforms)

Simplest flamegraph generation:
```bash
cargo install flamegraph
cargo flamegraph --bin my_binary -- --my-args
# Output: flamegraph.svg
```

Works on Linux (perf), macOS (DTrace), and Windows (ETW via WPR).

### Source-Level Assembly Analysis

When tools fail, you can still reason about assembly from source code. Know what LLVM can and cannot optimize:

**LLVM can:**
- Eliminate bounds checks when the loop bound proves the index is safe
- Inline across crates (with LTO enabled)
- Hoist loop-invariant computations
- Vectorize simple loops over arrays
- Merge adjacent loads/stores
- Eliminate dead code and redundant computations

**LLVM cannot:**
- Prove semantic invariants invisible in the code (e.g., "this Vec always has the same length as that Vec")
- Optimize across `unsafe` that might alias
- Eliminate bounds checks when the bound comes from a different allocation
- Optimize through virtual dispatch (trait objects)
- Reorder memory operations that might be observed by another thread

### Release Profile Settings

```toml
[profile.release]
opt-level = 3        # Maximum optimization
lto = "thin"         # Cross-crate inlining (or "fat" for even more, slower builds)
codegen-units = 1    # Single codegen unit = more inlining opportunities
strip = true         # Smaller binaries
```

With these settings, LLVM sees the entire program as one compilation unit and can inline aggressively across crate boundaries.

---

## 6. Assembly Analysis

### What to Look For

When reading assembly output (or reasoning about it from source):

1. **Bounds checks**: `cmp index, len; jae panic_label` — a compare-and-branch before every array access. If you see these in hot loops, LLVM couldn't prove the index is safe.

2. **Function calls in hot loops**: `call some_function` — if this appears per-entity, the function wasn't inlined. Check `#[inline]` annotations.

3. **Register spills**: `mov [rsp+offset], reg` followed later by `mov reg, [rsp+offset]` — the compiler ran out of registers and had to use stack storage. Common with many live variables.

4. **Excessive branching**: Multiple `je`, `jne`, `jae` in sequence — each is a potential mispredict. Look for ways to combine checks or make them predictable.

5. **SIMD instructions**: `movdqa`, `paddd`, `mulps` — vectorized operations. If you expected SIMD and don't see these, the loop wasn't vectorized.

### Why Bounds Checks Survive

LLVM eliminates bounds checks when it can prove `index < length`. This requires:

```rust
// LLVM CAN eliminate:
for i in 0..vec.len() {
    vec[i] // i < vec.len() is proven by loop condition
}

// LLVM CANNOT eliminate:
let data: &Vec<T> = get_from_hashmap(key);
for i in 0..entity_count {  // entity_count != data.len() — different allocations
    data[i]  // Bounds check stays!
}
```

The fix isn't `get_unchecked()` — it's restructuring so LLVM sees the connection, or accepting that the check is cheap (well-predicted forward branch = ~0.3ns).

### Useful Compiler Explorer Patterns

Use [godbolt.org](https://godbolt.org) to quickly check what LLVM does with a code snippet. Key flags:
- `-C opt-level=3`
- `-C lto=thin` (or `fat`)
- `-C codegen-units=1`
- `--edition 2021`

### x86-64 Instruction Cheat Sheet

When reading assembly output, these are the instructions you'll see most often in hot Rust code:

**Data movement:**
| Instruction | Meaning | Cost |
|---|---|---|
| `mov rax, rbx` | Copy register to register | ~1 cycle, often 0 (renamed) |
| `mov rax, [rbx]` | Load from memory at address in rbx | ~3-5 cycles (L1 hit) |
| `mov [rbx], rax` | Store to memory | ~1 cycle (store buffer absorbs it) |
| `lea rax, [rbx+8]` | Load Effective Address (arithmetic, no memory access) | ~1 cycle |
| `movdqa xmm0, [rbx]` | 128-bit aligned SIMD load | ~3-5 cycles |
| `vmovaps ymm0, [rbx]` | 256-bit aligned AVX load | ~3-5 cycles |

**Arithmetic:**
| Instruction | Meaning |
|---|---|
| `add/sub/mul/div` | Integer arithmetic |
| `addss/mulss` | Scalar single-precision float |
| `addps/mulps` | Packed (4 × f32) single-precision float (SSE) |
| `vaddps/vmulps` | Packed (8 × f32) single-precision float (AVX) |
| `inc/dec` | Increment/decrement by 1 |

**Control flow:**
| Instruction | Meaning | Mispredict cost |
|---|---|---|
| `cmp a, b` | Compare (sets flags) | — |
| `je/jne target` | Jump if equal / not equal | ~15-20 cycles |
| `jae/jb target` | Jump if above-or-equal / below (unsigned) | ~15-20 cycles |
| `jg/jl target` | Jump if greater/less (signed) | ~15-20 cycles |
| `call func` | Call function (pushes return address) | ~2-3 cycles |
| `ret` | Return from function | ~2-3 cycles |
| `jmp target` | Unconditional jump | ~1 cycle |

**How to spot a bounds check:**
```asm
cmp  rax, rcx        ; Compare index (rax) with length (rcx)
jae  .panic_label    ; Jump if index >= length → panic!
mov  rdx, [rbx+rax*8]; Actual array access (index * 8 for u64)
```

If you see the `cmp`+`jae` pair before every array access, LLVM didn't eliminate the bounds check.

### x86-64 Calling Conventions (System V / Windows)

**Linux/macOS (System V ABI):**
- First 6 integer args: `rdi, rsi, rdx, rcx, r8, r9`
- First 8 float args: `xmm0-xmm7`
- Return value: `rax` (integer) or `xmm0` (float)
- Callee-saved: `rbx, rbp, r12-r15` (function must restore before returning)
- Caller-saved: `rax, rcx, rdx, rsi, rdi, r8-r11` (caller must save if needed)
- Stack: 128-byte "red zone" below `rsp` usable without adjustment (System V only)
- Struct returns: small structs in `rax`+`rdx`, large structs via hidden pointer in `rdi`

**Windows (Microsoft x64 ABI):**
- First 4 integer args: `rcx, rdx, r8, r9`
- First 4 float args: `xmm0-xmm3`
- Return value: `rax`
- **No red zone** — any memory below `rsp` can be clobbered by interrupts
- Shadow space: 32 bytes reserved on stack for called functions
- Callee-saved: `rbx, rbp, rdi, rsi, r12-r15, xmm6-xmm15`

**Why this matters**: When you see `call` instructions in hot loops, check if arguments are being unnecessarily shuffled into the right registers. This can happen when a function doesn't get inlined.

### Spotting Inefficiencies in Assembly

**Pattern 1: Unnecessary stack spill/fill**
```asm
mov  [rsp+24], rax   ; Spill to stack
...  (5 instructions)
mov  rax, [rsp+24]   ; Reload from stack — why was this spilled?
```
Indicates register pressure. Try reducing the number of live variables in the hot loop.

**Pattern 2: Repeated loads from same address**
```asm
mov  rdx, [rbx+16]   ; Load field
...  (no write to [rbx+16])
mov  rdx, [rbx+16]   ; Load same field again — LLVM should have CSE'd this
```
If you see this, `unsafe` aliasing may be preventing LLVM from optimizing.

**Pattern 3: `div`/`idiv` instruction**
```asm
div  rcx   ; 64-bit integer division — 20-80 cycles!
```
Integer division is very expensive. Replace with bit shifts where possible (power-of-2 divisors).

**Pattern 4: `lock` prefix**
```asm
lock add [rax], 1   ; Atomic increment — 20-40 cycles!
```
Atomics are expensive. Use `Relaxed` ordering when possible (no `lock` needed on x86 for loads).

---

## 7. Tactics & Patterns

### Tactic: Clone Elimination

**Symptom**: `.clone()` in a hot path.

**Why it hurts**: Heap allocation + memcpy. Even small allocations add up.

**Fixes (in order of preference)**:
1. **Reference instead of own**: `&T` instead of `T`
2. **Move instead of clone**: Take ownership if the source is disposable
3. **`Cow<T>`**: Clone only when mutation is needed
4. **`Arc::make_mut()`**: Clone only when shared reference exists
5. **Pre-allocate + `extend_from_slice`**: One alloc for many items

**Example (real, ↓19.4%)**:
```rust
// Before: clones each entity's data during archetype migration
new_storage.push(old_storage.get(index).clone());

// After: move the value directly
new_storage.push(old_storage.swap_remove(index));
```

### Tactic: Precomputation

**Symptom**: Computing the same thing repeatedly.

**Why it hurts**: Wasted CPU. Often hidden in innocent-looking code.

**Example: Conflict matrix (real, ↓4.8%)**:
```rust
// Before: computed every frame during scheduling
fn systems_conflict(a: &System, b: &System) -> bool { /* O(components) scan */ }

// After: precomputed once during registration
struct RegisteredSystem {
    conflicts_with: Vec<usize>,  // Precomputed on registration
}
```

### Tactic: Cold-Path Extraction

**Symptom**: Error handling or rare cases inlined into hot loops.

**Fix**: Mark cold paths with `#[inline(never)]` or `#[cold]`.

```rust
// The error case is never taken in normal operation.
// Marking it #[cold] moves it out of the hot icache.
#[cold]
fn handle_error() -> ! { panic!() }

fn hot_loop() {
    for item in items {
        if unlikely_condition {
            handle_error();  // Not inlined in hot path
        }
        // ... normal work
    }
}
```

Note: `#[cold]` only works on function definitions. For closures or trait impls, use `#[inline(never)]`.

### Tactic: Allocation Reuse

**Symptom**: `Vec::new()` or `HashMap::new()` in a function called every frame.

**Fix**: Pre-allocate once, reuse via `.clear()` or `.drain(..)`.

```rust
// Before: allocates every frame
fn update(&mut self) {
    let mut work_list: Vec<Entity> = Vec::new();
    // ... fill and process work_list
}

// After: reuses allocation
fn update(&mut self) {
    self.work_list.clear();
    // ... fill and process work_list
}
```

### Tactic: Dense Storage

**Symptom**: Random memory access patterns, high cache miss rate.

**Fix**: Store data contiguously. In ECS: archetypes (SoA) instead of per-entity structs.

```
// AoS (Array of Structs) — bad cache utilization for component-wise ops
struct Entity { pos: Position, vel: Velocity, health: Health }
entities: Vec<Entity>

// SoA (Struct of Arrays) — good cache utilization
positions: Vec<Position>
velocities: Vec<Velocity>
healths: Vec<Health>
```

### Tactic: Branch Elimination

**Symptom**: `if`/`else` or `match` in hot loops with unpredictable outcomes.

**Fixes (in order of preference):**
1. **Sort data by branch outcome**: Long runs of same outcome → predictor learns pattern
2. **Branchless code**: Replace `if cond { a } else { b }` with arithmetic
3. **Lookup table**: Small number of outcomes → precomputed table
4. **Split hot/cold paths**: Separate loops for common and rare cases

```rust
// Branchful: unpredictable if threshold falls mid-range
for &x in &data {
    if x < threshold { below += 1; } else { above += 1; }
}

// Branchless: cmov instruction, no mispredict possible
for &x in &data {
    let is_below = (x < threshold) as u64;
    below += is_below;
    above += 1 - is_below;
}
```

**When to use branchless**: When the branch is unpredictable (random-ish data) AND the work in each branch is cheap (a few arithmetic ops). If the branches do heavy work, the cmov approach may be worse (it computes both sides).

### Tactic: Bit Packing

**Symptom**: Many small `bool` or `enum` fields wasting space.

**Fix**: Pack multiple small fields into a single integer:

```rust
// BEFORE: 3 × bool = 3 bytes + padding = 4 or 8 bytes
struct Flags { alive: bool, visible: bool, dirty: bool }

// AFTER: 1 × u8 = 1 byte
struct Flags(u8);
impl Flags {
    const ALIVE: u8  = 1 << 0;
    const VISIBLE: u8 = 1 << 1;
    const DIRTY: u8   = 1 << 2;
    fn is_alive(&self) -> bool { self.0 & Self::ALIVE != 0 }
    fn set_alive(&mut self, v: bool) { if v { self.0 |= Self::ALIVE; } else { self.0 &= !Self::ALIVE; } }
}
```

### Tactic: Const Generics for Compile-Time Dispatch

**Symptom**: Runtime branching on values known at compile time.

**Fix**: Use const generics to monomorphize — each variant becomes a separate function with the branch compiled away:

```rust
// BEFORE: branch evaluated every call
fn process<const MODE: u8>(data: &[u32]) {
    for &x in data {
        match MODE {  // This branch is optimized away by monomorphization!
            0 => fast_path(x),
            1 => slow_path(x),
            _ => default_path(x),
        }
    }
}

// Usage: each call instantiates a different monomorphized version
process::<0>(&data);  // fast_path only, no branch
process::<1>(&data);  // slow_path only, no branch
```

### Tactic: Fast/Slow Path Splitting

**Symptom**: A loop where 95% of iterations are simple and 5% require complex handling.

**Fix**: Two-phase processing — handle the common case in a tight inner loop, collect rare cases for later processing:

```rust
// Phase 1: fast inner loop for common case
let mut rare_items = Vec::new();
for (i, item) in data.iter().enumerate() {
    if item.is_simple() {
        process_simple(item);  // Inlined, no branch inside process_simple
    } else {
        rare_items.push(i);    // Defer complex work
    }
}

// Phase 2: handle rare cases separately
for &i in &rare_items {
    process_complex(&mut data[i]);
}
```

### Tactic: Strength Reduction

Replace expensive operations with cheaper equivalents known at compile time:

```rust
// Expensive: integer division (20-80 cycles)
let half = x / 2;

// Cheap: bit shift (1 cycle) — works for power-of-2 divisors
let half = x >> 1;  // x / 2

// Expensive: modulo (same as division)
let remainder = x % 16;

// Cheap: bitwise AND (1 cycle)
let remainder = x & 15;  // x % 16

// Expensive: multiplication by constant
let scaled = x * 320;

// Cheaper (sometimes): shift+add — but LLVM usually does this for you
let scaled = (x << 8) + (x << 6);  // x * 320 = x * 256 + x * 64
```

**Note**: LLVM does strength reduction automatically for constants. Only use manual reduction when benchmarking proves LLVM didn't.

### Tactic: Iterator Chain Optimization

**Symptom**: Multiple `.map().filter().collect()` chains allocating intermediate Vecs.

**Fix**: Chain iterators — they're lazy and allocation-free:

```rust
// BAD: allocates three Vecs
let temp1: Vec<_> = data.iter().map(|x| x * 2).collect();
let temp2: Vec<_> = temp1.iter().filter(|x| **x > 10).collect();
let result: Vec<_> = temp2.iter().map(|x| **x + 1).collect();

// GOOD: zero allocations (lazy evaluation)
let result: Vec<_> = data.iter()
    .map(|x| x * 2)
    .filter(|x| *x > 10)
    .map(|x| x + 1)
    .collect();  // Single allocation at the end
```

---

## 8. Rust-Specific Techniques

### Release Profile

Always benchmark with `--release`. Debug mode is 10-100x slower and has different performance characteristics.

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
```

- `opt-level = 3`: Aggressive optimizations (vs `s` for size, `2` for balanced)
- `lto = "thin"`: Cross-crate inlining with reasonable build times. `"fat"` is slightly better but much slower to link
- `codegen-units = 1`: Single compilation unit — maximizes inlining, slower compile

### Cross-Crate Inlining

By default, Rust does NOT inline across crate boundaries (except for generics). LTO enables this.

Without LTO:
```rust
// crate A
pub fn get(&self, i: usize) -> &T { self.data.get(i).unwrap() }

// crate B — get() is a call, not inlined
a.get(index)
```

With LTO: `get()` is inlined, `Vec::get()` bounds check is visible and can be eliminated.

### `#[inline]` Annotations

- `#[inline]`: Hint — LLVM usually inlines small functions anyway
- `#[inline(always)]`: Force inlining — use sparingly, only for trivial functions
- `#[inline(never)]`: Prevent inlining — use for cold paths

**Rule of thumb**: Add `#[inline]` to tiny functions called in hot loops that are defined in a different module. LLVM usually gets it right, but explicit hints help at module boundaries.

### Enum Niche Optimization

Rust uses "niche optimization" — if an enum has unused bit patterns, it packs the discriminant into them:

```rust
// Option<&T> — references can't be null, so None is represented as null pointer
// Size: size_of::<usize>() (8 bytes on 64-bit) — NO extra discriminant

// Option<NonZeroU64> — NonZeroU64 can't be 0, so None is 0
// Size: 8 bytes — NO extra discriminant

// Option<u64> — u64 can be 0, so a discriminant is needed
// Size: 16 bytes — 8 bytes discriminant + 8 bytes value
```

Use `NonZero*` types when you have an `Option` around an integer that's never zero.

### `Vec::with_capacity`

Always pre-allocate when you know the size:

```rust
// 10-20% faster for large collections
let mut result = Vec::with_capacity(known_size);
```

Macro-generated code should include `with_capacity` hints where possible.

### `black_box` — Prevent Optimization

`std::hint::black_box()` tells LLVM "this value is used" — preventing dead code elimination without adding overhead:

```rust
use std::hint::black_box;

// BAD: LLVM sees the result is unused, eliminates the entire computation
let result = expensive_computation();

// GOOD: LLVM must compute the result (but doesn't know how it's used)
let result = black_box(expensive_computation());
```

**Benchmarking without black_box**: LLVM may optimize away the code you're trying to benchmark. Always wrap the final result in `black_box()` and wrap loop inputs in `black_box()` to prevent constant folding.

```rust
fn bench_iteration(c: &mut Criterion) {
    c.bench_function("my_bench", |b| {
        let data = black_box(generate_test_data());  // Prevent precomputation
        b.iter(|| {
            let result = process(black_box(&data));  // Prevent loop hoisting
            black_box(result);                        // Prevent elimination
        });
    });
}
```

### `#[repr(C)]` vs `#[repr(Rust)]` Layout

```rust
#[repr(C)]    // C layout: fields in declaration order, C-compatible alignment
#[repr(Rust)] // Default: compiler free to reorder for size optimization
#[repr(align(64))] // Minimum alignment (for cache line padding)
#[repr(packed)]    // No padding between fields (unaligned access penalty!)
```

**When to use `#[repr(C)]`**:
- FFI (C interop)
- Predictable layout for unsafe pointer math
- Ensuring specific field order for hot/cold splitting

**When to use default `#[repr(Rust)]`**: Almost always. The compiler reorders fields to minimize struct size, which improves cache utilization.

### `PhantomData` — Zero-Cost Type Markers

`PhantomData<T>` has zero size and zero runtime cost. Use it to:
- Mark ownership without storing data
- Indicate type relationships (covariance, contravariance)
- Carry type parameters that don't appear in fields

```rust
struct Query<'w, Q: QueryTarget, F: QueryFilter> {
    world: &'w mut World,
    // ... fields ...
    _phantom: PhantomData<(Q, F)>,  // Zero-size, carries type info
}
```

### Drop Check Overhead

Types implementing `Drop` have hidden drop flags and cannot be moved trivially. For hot-path types, avoid `Drop` when possible:

```rust
// Has hidden drop flag → prevents some optimizations
struct ManagedResource { handle: *mut () }
impl Drop for ManagedResource { fn drop(&mut self) { /* ... */ } }

// No Drop → fully optimized moves
struct SimpleHandle(*mut ());  // Just a pointer, no drop overhead
```

### `#[derive(Copy)]` — Enable Register Allocation

`Copy` types can be passed in registers. `Clone` types require memory allocation or reference passing:

```rust
// Copy: values stay in registers, no memory traffic
#[derive(Copy, Clone)]
struct Vec3 { x: f32, y: f32, z: f32 }

// Not Copy: passes by reference, potential stack spill
struct BigStruct { data: [u8; 256] }
```

**Rule of thumb**: Any type ≤ 32 bytes that's just plain data should be `Copy`.

### `unsafe` Aliasing Rules

LLVM uses `noalias` annotations on `&mut` references — it assumes no other pointer accesses the same memory. Raw pointers (`*mut T`) do NOT have this guarantee. When you convert `&mut` to `*mut`, LLVM loses aliasing information:

```rust
// GOOD: &mut guarantees exclusive access → LLVM can optimize aggressively
fn process(data: &mut [u32]) {
    for x in data.iter_mut() { *x += 1; }  // LLVM knows no aliasing
}

// CAUTION: *mut has no aliasing guarantee → LLVM is conservative
unsafe fn process_raw(data: *mut u32, len: usize) {
    for i in 0..len { *data.add(i) += 1; }  // LLVM assumes possible aliasing
}
```

**Prefer `&mut` over `*mut` when possible**. Only use raw pointers when the borrow checker can't express the ownership pattern.

### Trait Object Overhead

```rust
// Static dispatch: monomorphized at compile time, fully inlinable
fn process<T: MyTrait>(item: &T) { item.do_work(); }

// Dynamic dispatch: vtable lookup, not inlinable
fn process(item: &dyn MyTrait) { item.do_work(); }
```

**Cost of dyn dispatch**: ~5-10ns per call (vtable load + indirect jump). In hot loops, this is significant. Prefer generics (static dispatch) for hot paths, use trait objects only for type-erased storage.

### `panic = "abort"` — Remove Unwinding

```toml
[profile.release]
panic = "abort"
```

Eliminates landing pads (unwinding infrastructure) from the binary. Smaller binary, slightly faster. The cost: panics immediately abort instead of unwinding (no `catch_unwind` recovery). For game engines and performance-critical applications, this is usually the right tradeoff.

### `pub(crate)` vs `pub` — Optimization Hints

Visibility matters for optimization. `pub` items cannot have their representation changed freely (it's part of the public API). `pub(crate)` items can be optimized more aggressively by LLVM because it knows all callers are within the same crate.

---

## 9. CPU Architecture & Cache Fundamentals

Understanding the hardware is essential — many "mysterious" performance problems are really cache problems.

### The Memory Hierarchy

```
┌─────────┐
│  CPU    │  Registers:     ~0 cycles,     ~32 × 8 bytes
│  Core   │  L1 Cache:      ~3-5 cycles,   32-64 KiB (per-core)
│         │  L2 Cache:      ~10-15 cycles, 256-512 KiB (per-core)
├─────────┤
│ Shared  │  L3 Cache:      ~30-50 cycles, 4-32 MiB (shared)
├─────────┤
│  DRAM   │  Main Memory:   ~100-300 cycles, 8-64 GiB
└─────────┘
```

**The key insight**: Accessing main memory is 20-100× slower than L1. If your data doesn't fit in L2/L3, you're memory-bound and no amount of CPU optimization will help.

### Cache Lines (64 Bytes)

CPUs read memory in 64-byte chunks called **cache lines**. When you touch one byte, the CPU loads the entire surrounding 64-byte block into cache.

**Implications for Rust structs:**

```rust
// GOOD: sequential access reads each cache line once, uses all 64 bytes
let positions: Vec<[f32; 3]> = ...;  // 12 bytes per element, 5.3 per cache line
for pos in &positions { /* uses all 12 bytes of each element */ }

// BAD: strided access skips data, wastes cache bandwidth
struct Entity { pos: [f32; 3], vel: [f32; 3], health: f32, ... }  // 128+ bytes
for entity in &entities { /* only reads pos (12 bytes), wastes 116+ bytes */ }
```

**Padding and alignment**: Rust automatically pads structs to meet alignment requirements. Use `#[repr(C)]` for predictable layout or `#[repr(packed)]` to squeeze bytes (at the cost of unaligned access penalties).

### False Sharing

When two threads modify different variables that happen to live on the same cache line, the CPU's cache coherence protocol forces constant invalidation — a huge performance killer.

```rust
// BAD: counters[0] and counters[1] share a cache line (64 bytes, 16 × u32)
let counters: Vec<u32> = vec![0; num_threads];
// Thread 0 writes counters[0], Thread 1 writes counters[1] — FALSE SHARING!

// GOOD: pad each counter to fill its own cache line
#[repr(align(64))]
struct AlignedCounter(u32);
let counters: Vec<AlignedCounter> = ...;
// Each counter now occupies its own cache line — no false sharing
```

**Symptoms**: Parallel code that doesn't scale — 2 threads barely faster than 1, 4 threads sometimes slower than 2.

### Prefetch

The CPU tries to predict which memory you'll need next and fetch it early. Sequential access patterns are highly predictable; pointer-chasing (linked lists, trees, hash maps) defeats the prefetcher.

```rust
// GOOD: linear scan — prefetcher sees the pattern, data arrives before you need it
for i in 0..data.len() { process(data[i]); }

// BAD: index lookup through another array — unpredictable, prefetcher fails
for &idx in &indices { process(data[idx]); }
```

Software prefetch exists (`std::intrinsics::prefetch_read_data` on nightly) but is rarely needed — restructure your data instead.

### Branch Predictor

Modern CPUs predict branch direction with >95% accuracy. The cost of a **mispredicted** branch is ~15-20 cycles (pipeline flush).

```rust
// GOOD: predictable branch — sorted data means long runs of same outcome
// Branch predictor learns the pattern after 1-2 mispredictions
data.sort();
for x in data { if x < threshold { a(); } else { b(); } }

// BAD: unpredictable branch — random outcomes, ~50% mispredict rate
for x in data { if x < threshold { a(); } else { b(); } }
```

**Rule of thumb for hot loops**: One unpredictable branch costs ~7-10ns. Two branches in the same hot path is worth eliminating. Three or more — restructure.

### Instruction Cache (I-Cache)

The CPU caches decoded instructions separately from data. Inlining increases code size and can cause I-cache thrashing if overdone. `#[cold]` and `#[inline(never)]` help by keeping rarely-executed code out of the hot I-cache.

### Translation Lookaside Buffer (TLB)

Virtual→physical address translation is cached in the TLB. Random access to many different pages (>~64 for L1 TLB) causes TLB misses (~20-50 cycles each). Use **huge pages** for large data structures on Linux (transparent hugepages), or ensure sequential access patterns.

### Practical Takeaway

| Problem | Symptom | Fix |
|---|---|---|
| Cache misses | Flat performance regardless of CPU optimizations | SoA layout, smaller structs, prefetch-friendly access |
| False sharing | Poor parallel scaling | Pad per-thread data to cache line |
| Branch mispredicts | High variance in iteration time | Sort data, remove branches, use `match` with obvious pattern |
| I-cache thrashing | Function call overhead despite small functions | `#[inline(never)]` on cold paths, avoid deep call chains |
| TLB misses | High memory latency despite data in RAM | Huge pages, sequential access |

---

## 10. Memory & Allocation Deep Dive

### The Cost of Allocation

| Allocation Type | Approximate Cost | Use Case |
|---|---|---|
| Stack (`let x = ...`) | ~1 cycle (stack pointer bump) | Everything possible |
| Heap small (<128 B) | ~50-100 cycles | Dynamic data |
| Heap medium (128 B–1 KB) | ~100-300 cycles | Vec growth, strings |
| Heap large (>1 KB) | ~300-2000 cycles (may mmap) | Buffers, large collections |
| `Arc::new()` | Heap alloc + atomic increment | Shared ownership |
| `Box::new()` | Heap alloc only | Owned heap data |
| `Rc::new()` | Heap alloc + non-atomic increment | Single-thread shared |

**The default Rust allocator** (system allocator on most platforms) is decent but not specialized for game/ECS workloads. Consider replacing it.

### Custom Global Allocators

```rust
use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
```

| Allocator | Strengths | Install |
|---|---|---|
| **mimalloc** | Excellent for multi-threaded, low fragmentation | `mimalloc` crate |
| **jemalloc** | Proven, good all-around, used by Firefox | `jemallocator` crate |
| **snmalloc** | Microsoft Research, message-passing design | `snmalloc-rs` crate |
| **dlmalloc** | Simple, good single-thread | `dlmalloc` crate |

**Benchmark your workload.** Different allocators shine with different allocation patterns. For ECS (many small, uniform allocations), mimalloc typically wins.

### Arena & Bump Allocators

When you allocate many objects with the same lifetime (e.g., all entities in a frame), use an arena:

```rust
use bumpalo::Bump;

// All allocations share the arena's lifetime — freed all at once
let arena = Bump::new();
for _ in 0..10000 {
    let temp_data = arena.alloc([0u8; 256]);  // No per-object free cost
    process(temp_data);
}
// All 10,000 allocations freed here in O(1)
```

**When to use arenas:**
- Per-frame scratch data that all dies together
- Temporary query results
- Component data with uniform lifetime (the archetype IS an arena)

### Pool Allocators

For fixed-size objects created/destroyed frequently (entities!), use an object pool:

```rust
// Simple free-list pool (or use the `object-pool` crate)
struct EntityPool {
    data: Vec<EntityData>,
    free_list: Vec<usize>,  // Indices of freed slots
}

impl EntityPool {
    fn allocate(&mut self) -> usize {
        self.free_list.pop().unwrap_or_else(|| {
            let idx = self.data.len();
            self.data.push(EntityData::default());
            idx
        })
    }

    fn free(&mut self, idx: usize) {
        self.free_list.push(idx);  // O(1), no actual deallocation
    }
}
```

### Small Vector Optimization

For vectors that are usually small but occasionally large, avoid heap allocation for the common case:

```rust
use smallvec::SmallVec;

// Inline storage for up to 8 elements — no heap alloc for common case
let mut entities: SmallVec<[Entity; 8]> = SmallVec::new();
```

**Where we use it**: Not yet, but ideal for entity lists per archetype (most archetypes have <100 entities, but some have >10,000).

### Measuring Allocation Pressure

```bash
# Linux: count heap allocations
valgrind --tool=massif --massif-out-file=massif.out ./target/release/my_binary
ms_print massif.out

# Or use the allocation-count feature in nightly:
RUSTFLAGS="-Z print-type-sizes" cargo +nightly build --release
```

Or add a wrapper in code:
```rust
use std::alloc::{GlobalAlloc, System, Layout};

struct CountingAllocator;
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}
```

### Allocation-Free Patterns

1. **Pre-allocate and reuse**: `Vec::clear()` doesn't free memory
2. **Fixed-size arrays**: Use `[T; N]` when N is known at compile time
3. **ArrayVec**: `arrayvec::ArrayVec` — Vec with inline storage, panics on overflow
4. **Iterators**: Chain, map, filter — all zero-allocation (lazy evaluation)
5. **Cow**: Clone-on-write only allocates when mutation actually occurs

### `Vec` Growth Strategy

`Vec` doubles capacity on push when full. This means log₂(n) reallocations for n pushes. If you know the final size, use `with_capacity`:

```rust
// BAD: 20 reallocations for 1M elements
for _ in 0..1_000_000 { vec.push(x); }

// GOOD: 1 allocation
let mut vec = Vec::with_capacity(1_000_000);
for _ in 0..1_000_000 { vec.push(x); }
```

After building a Vec, use `shrink_to_fit()` to recover unused capacity — but only if the Vec will persist (the cost of shrink + re-growth may outweigh the memory savings).

---

## 11. Parallelism & Concurrency

### When to Parallelize

Parallelism has overhead: thread synchronization, work distribution, cache coherence. For small N, sequential is faster.

**Rough thresholds for Rayon (ECS workload):**
- **< 1,000 entities**: Sequential wins — overhead dominates
- **1,000–10,000 entities**: Break-even — depends on per-item work
- **10,000–1,000,000 entities**: Parallel wins — near-linear scaling
- **> 1,000,000 entities**: Parallel wins significantly — but watch memory bandwidth

**Measure.** Don't assume `par_iter()` is always faster.

### Rayon Deep Dive

Rayon uses **work-stealing**: each thread has a deque of tasks. When a thread finishes its work, it "steals" from the back of another thread's deque. This self-balances load without central coordination.

```rust
use rayon::prelude::*;

// Simple parallel iteration
data.par_iter_mut().for_each(|item| process(item));

// Configurable batch size (fewer, larger batches = less overhead)
data.par_iter_mut()
    .with_min_len(256)   // At least 256 items per batch
    .for_each(|item| process(item));

// Parallel fold/reduce
let sum: f64 = data.par_iter()
    .map(|item| expensive_computation(item))
    .sum();

// Custom parallel work with join/split
let (left, right) = data.split_at_mut(data.len() / 2);
rayon::join(|| process(left), || process(right));
```

**Batch size tuning**: Too small → overhead dominates. Too large → load imbalance (one thread finishes early, others still working). For uniform work, 256–1024 is a good default. For variable-cost work, smaller batches help load balancing.

### False Sharing (Revisited)

```rust
// BAD: each thread writes to adjacent indices
let results: Vec<AtomicU64> = (0..num_threads).map(|_| AtomicU64::new(0)).collect();
data.par_iter().enumerate().for_each(|(i, item)| {
    results[i % num_threads].fetch_add(compute(item), Ordering::Relaxed);
    // ↑ Adjacent AtomicU64s share cache lines → false sharing!
});

// GOOD: pad to cache line
#[repr(align(64))]
struct PaddedAtomic(AtomicU64);
let results: Vec<PaddedAtomic> = ...;
```

### Atomic Ordering Cheat Sheet

| Ordering | Cost (x86) | When to Use |
|---|---|---|
| `Relaxed` | ~1 cycle | Simple counters, no ordering requirements |
| `Acquire` | ~1 cycle | Load that must see all stores from a `Release` in another thread |
| `Release` | ~1 cycle | Store that must be visible to an `Acquire` in another thread |
| `AcqRel` | ~1 cycle | Combined Acquire+Release (rare; usually `compare_exchange`) |
| `SeqCst` | ~10-40 cycles | Total global ordering — **avoid in hot paths** |

**On x86**: All loads are Acquire, all stores are Release at the hardware level, so `Acquire`/`Release` are free. ARM and RISC-V have weaker hardware ordering — use `Acquire`/`Release` correctly for portability.

### Locks: When and Which

| Lock | Cost (uncontended) | Cost (contended) | Use Case |
|---|---|---|---|
| `AtomicBool` spinlock | ~5ns | Burns CPU | Extremely short critical sections (<50ns) |
| `parking_lot::Mutex` | ~15ns | Parks thread | General purpose |
| `std::sync::Mutex` | ~25ns | Parks thread | Standard, but slower than parking_lot |
| `parking_lot::RwLock` | ~20ns read | Depends | Many readers, few writers |
| `std::sync::RwLock` | ~30ns read | Depends | Standard alternative |

**Rule of thumb**: Always use `parking_lot` instead of `std::sync` locks — faster, smaller, no poisoning.

### Channels

```rust
// crossbeam: faster, more features than std::sync::mpsc
use crossbeam::channel;

let (sender, receiver) = channel::unbounded();
// Or bounded to apply backpressure:
let (sender, receiver) = channel::bounded(1024);
```

For SPSC (single producer, single consumer), use a specialized queue like `ringbuf` or `spsc` crate for zero-allocation message passing.

### Thread Pool Sizing

`rayon::current_num_threads()` defaults to `num_cpus`. For CPU-bound work, this is correct. For IO-bound or mixed workloads:

```rust
rayon::ThreadPoolBuilder::new()
    .num_threads(physical_cores)  // Not logical (hyperthreading) cores
    .build_global()
    .unwrap();
```

**Hyperthreading caveat**: Two logical threads share one physical core's execution units. For pure integer/FP work, hyperthreading often hurts (cache thrashing). For pointer-chasing (hash maps), hyperthreading can help (hide memory latency).

---

## 12. SIMD & Auto-Vectorization

### What Auto-Vectorization Is

LLVM can transform scalar loops into SIMD (Single Instruction, Multiple Data) operations that process 4, 8, or 16 elements at once:

```rust
// Scalar: 1 multiply per instruction
for i in 0..data.len() {
    data[i] *= 2.0f32;
}

// Auto-vectorized (AVX): 8 multiplies per instruction (8 × f32 = 256 bits)
// LLVM does this automatically if conditions are right
```

### Requirements for Vectorizable Loops

LLVM can vectorize a loop when ALL of these hold:

1. **No inter-iteration dependencies**: Each iteration must be independent
2. **Countable trip count**: The loop bound must be known before entering
3. **No function calls**: Calls prevent vectorization (except `#[inline]` math functions)
4. **Uniform control flow**: No `if`/`else` inside the loop (or minimal, predictable)
5. **Contiguous memory access**: Stride-1 access patterns (`data[i]`, not `data[indices[i]]`)
6. **Aligned access** (helpful but not required): `data.as_ptr()` aligned to 16/32/64 bytes

```rust
// CAN be vectorized: independent, countable, no branches, contiguous
for i in 0..len {
    result[i] = a[i] * b[i] + c[i];
}

// CANNOT be vectorized: inter-iteration dependency (result[i] depends on result[i-1])
for i in 1..len {
    result[i] = result[i - 1] + data[i];
}

// CANNOT be vectorized: branch inside loop (LLVM may vectorize with masking on AVX-512)
for i in 0..len {
    if data[i] > 0.0 {
        result[i] = data[i].sqrt();
    }
}
```

### Detecting Vectorization

```bash
# Look for packed/vector instructions in the hot loop
cargo asm --release --lib "my_crate::hot_function" | grep -E "movdqa|padd|mulps|addps|vmovaps|vaddps|vmulps"
```

Or in Compiler Explorer: add `-C target-feature=+avx2` and look for `v`-prefixed instructions (AVX).

### Struct-of-Arrays as Vectorization Enabler

```rust
// AoS: cannot vectorize — each Entity is 28+ bytes, components not contiguous
struct Entity { x: f32, y: f32, z: f32, health: f32 }
for entity in &mut entities { entity.x += 1.0; }  // Scalar only

// SoA: CAN vectorize — all x values are contiguous
struct World {
    x: Vec<f32>,
    y: Vec<f32>,
    z: Vec<f32>,
}
for x in &mut world.x { *x += 1.0; }  // 8 per instruction with AVX
```

**This is why ECS uses SoA.** It's not just about cache — it enables auto-vectorization.

### Portable SIMD (Nightly)

```rust
#![feature(portable_simd)]
use std::simd::*;

fn scale(values: &[f32], factor: f32) -> Vec<f32> {
    values
        .as_simd::<8>()              // Chunks of 8 × f32
        .iter()
        .flat_map(|chunk| {
            let scaled = *chunk * Simd::splat(factor);
            scaled.to_array()
        })
        .collect()
}
```

Or use the `wide` crate for stable Rust:
```rust
use wide::f32x8;  // 8 × f32 = 256-bit AVX

for chunk in data.chunks_exact(8) {
    let v = f32x8::from(chunk);
    let result = v * f32x8::splat(2.0);
    // ...
}
```

### Alignment Requirements

SIMD loads are faster (and sometimes required) when data is aligned:
```rust
#[repr(align(32))]  // AVX alignment
struct AlignedF32x8([f32; 8]);
```

`Vec` already aligns to the type's alignment. For SIMD, use `#[repr(align(32))]` or `#[repr(align(64))]`.

### When Auto-Vectorization Fails

**Most common reasons:**
1. **Aliasing concerns**: `&mut [T]` and `&[T]` parameters might overlap → LLVM emits scalar code. Use `&mut` and `&` correctly; for raw pointers, annotate with `noalias` (nightly).
2. **Non-power-of-two types**: `[i32; 3]` (12 bytes) doesn't fit cleanly in SIMD lanes
3. **Checked arithmetic**: Overflow checks in debug mode prevent vectorization
4. **Float associativity**: `-ffast-math` equivalent not enabled by default (Rust is strict about IEEE 754)

---

## 13. Compiler Optimizations Deep Dive

### Opt-Level Differences

| Level | What It Does | Use For |
|---|---|---|
| `0` | No optimization. Fast compile. | Debug builds |
| `1` | Basic peephole opts, no inlining. | Rarely used |
| `2` | Full optimization (LLVM default). Good inlining. | Standard release |
| `3` | `2` + aggressive loop unrolling, vectorization. | Performance-critical |
| `s` | `2` + size optimizations. | Embedded, WASM |
| `z` | `s` + aggressive size reduction. | Minimum binary size |

**For 99% of performance work**: `opt-level = 3`.

### LTO Modes

```
lto = false    → Each crate compiled independently, no cross-crate inlining
lto = "thin"   → Cross-crate summary-based inlining, fast (adds ~10-20% to link time)
lto = "fat"    → Full cross-crate optimization, slow (adds ~100-300% to link time)
lto = true     → Alias for "fat" (deprecated, use explicit)
```

**Recommendation**: Always use `lto = "thin"` for release. The compile time cost is modest and the performance gain is significant (LLVM can inline across crate boundaries, eliminate bounds checks, and constant-propagate through generic code).

### Codegen Units

```
codegen-units = 1     → Single LLVM module, maximum optimization, slow compile
codegen-units = 16    → Default, parallel compilation, less optimization
codegen-units = 256   → Maximum parallelism, minimum optimization per unit
```

`codegen-units = 1` is the single biggest compile-time-to-performance tradeoff. For CI/release builds set it to 1; for development use the default.

### Profile-Guided Optimization (PGO)

PGO collects runtime data (which branches are taken, which functions are hot) and feeds it back to the compiler:

```bash
# Step 1: Build with instrumentation
RUSTFLAGS="-C profile-generate=/tmp/pgo-data" cargo build --release

# Step 2: Run representative workloads (generates .profraw files)
./target/release/my_binary --benchmark-workload
./target/release/my_binary --another-workload

# Step 3: Merge profile data
llvm-profdata merge -o /tmp/pgo-data/merged.profdata /tmp/pgo-data/*.profraw

# Step 4: Rebuild with profile data
RUSTFLAGS="-C profile-use=/tmp/pgo-data/merged.profdata" cargo build --release
```

**Expected gain**: 5-15% on branch-heavy code. The compiler now knows which branches are "likely" and optimizes accordingly.

### BOLT (Post-Link Optimizer)

BOLT operates on the final binary, reordering code based on profile data to improve I-cache and branch predictor utilization:

```bash
# Requires perf data (Linux only)
perf record -e cycles:u -j any -o perf.data -- ./target/release/my_binary
llvm-bolt ./target/release/my_binary -o ./target/release/my_binary.bolt \
    -data perf.data -reorder-blocks=ext-tsp -reorder-functions=hfsort \
    -split-functions -split-all-cold -dyno-stats
```

**Expected gain**: 5-10% on large binaries. Most effective for executables >1MB with many cold functions.

### Likely/Unlikely Hints (Nightly)

```rust
#![feature(core_intrinsics)]
use std::intrinsics::{likely, unlikely};

if unlikely(error_condition) {  // Hint: this branch is rarely taken
    handle_error();
}

if likely(common_case) {
    fast_path();
}
```

These emit branch-hint prefixes that the CPU uses for prediction. Useful when you know the branch pattern but the compiler/CPU doesn't.

### `assert_unchecked` (Nightly)

```rust
#![feature(core_intrinsics)]
use std::intrinsics::assert_unchecked;

unsafe { assert_unchecked(index < data.len()); }  // Promise to LLVM: no bounds check needed
```

Stronger than `get_unchecked()` — tells LLVM the condition is always true, enabling more optimizations (e.g., narrowing integer ranges).

### `rustc` Optimization Flags Reference

```bash
RUSTFLAGS="-C target-cpu=native"                    # Enable all CPU features (AVX2, BMI, etc.)
RUSTFLAGS="-C target-feature=+avx2,+fma"            # Selective features
RUSTFLAGS="-C panic=abort"                          # No unwinding = smaller, faster (no landing pads)
RUSTFLAGS="-C debuginfo=0"                          # No debug info in binary
RUSTFLAGS="-C embed-bitcode=no"                     # Faster compile, no LTO (don't combine with lto=true)
RUSTFLAGS="-Z virtual-function-elimination=yes"     # Nightly: eliminate unused vtable entries
```

### Compilation Time vs Performance

| Setting | Compile Time | Performance |
|---|---|---|
| Default debug | 1× | 0.03× |
| Default release | 2-3× | 1× |
| + lto=thin | 2.5-4× | 1.05-1.15× |
| + lto=fat | 4-8× | 1.08-1.20× |
| + codegen-units=1 | +50% | 1.02-1.08× |
| + PGO | +100% (two builds) | 1.05-1.15× |
| + BOLT | +20% (post-processing) | 1.05-1.10× |

**Practical release profile** (90% of max perf, reasonable build times):
```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
```

---

## 14. The "Generation Counter" Hack

### Pattern

The single most effective optimization pattern we found:

```rust
struct World {
    data: HashMap<Key, Value>,
    generation: u64,  // Monotonically incrementing counter
    cached_result: (u64, CachedData),  // (generation, result)
}

impl World {
    fn get_cached(&mut self) -> &CachedData {
        if self.cached_result.0 != self.generation {
            self.cached_result = (self.generation, self.compute_expensive());
        }
        &self.cached_result.1
    }

    fn mutate_data(&mut self) {
        // ... change data ...
        self.generation += 1;  // Invalidate cache
    }
}
```

### Why it Works

Instead of asking "has the data changed?" (expensive check), you ask "has the generation changed?" (integer comparison). The generation is bumped at every mutation site. There are fewer mutation sites than query sites, so the bookkeeping cost is amortized.

### Where We Applied It

1. **Query archetype matching** (Pass 24): `archetype_generation` bumps when archetypes are added/removed. Queries cache their matching archetype list keyed by this generation. Saves scanning all archetypes every frame.

2. **Script component archetypes** (Pass 28): Same counter, same pattern. Saves per-script-component archetype scans.

3. **Scheduler graph** (existing): `graph_dirty` flag — equivalent pattern with a boolean instead of a counter.

### When to Use

- You have an expensive computation whose inputs change infrequently
- You can enumerate all mutation sites
- The cache check is cheaper than the computation

### When NOT to Use

- Inputs change every frame (cache always misses — pure overhead)
- Mutation sites are too numerous to track
- The computation is already cheap (integer comparison + branch costs ~1-2ns)

---

## 15. ECS-Specific Optimization Patterns

Entity Component Systems have unique performance characteristics. These patterns are specific to ECS architecture but the principles generalize.

### Archetype Layout (SoA)

The fundamental ECS optimization: group entities with the same component types into "archetypes" and store each component type as a separate contiguous array.

```
Archetype<Position, Velocity, Health>
    positions: Vec<Position>  ← All Position values, contiguous
    velocities: Vec<Velocity>  ← All Velocity values, contiguous
    healths: Vec<Health>       ← All Health values, contiguous
    entities: Vec<Entity>      ← Entity IDs (system can ignore these)
    ticks: HashMap<ComponentId, Vec<ComponentTicks>>  ← Change detection per-component
```

**Benefits:**
- Cache-friendly: iterating Position touches only Position data
- SIMD-friendly: contiguous f32 arrays vectorize naturally
- No per-entity allocation: push to Vec, swap_remove on destroy
- O(1) entity→component lookup: entity stores (archetype_id, index)

**Cost:**
- Archetype migration: adding/removing a component moves ALL of an entity's data between archetypes. This is O(components) and the single most expensive ECS operation.

### Component Bitmask Operations

Component sets are represented as bitmasks (u128 in our ECS). Archetype matching is bitwise AND:

```rust
fn archetype_matches(archetype: &Archetype, query_mask: &ComponentMask) -> bool {
    // O(1): single AND + compare — no per-component iteration
    archetype.component_mask.contains_all(query_mask)
}
```

**128 bits = 128 component types.** Beyond that, consider:
- Multiple u128s in an array (but archetype matching gets more expensive)
- `bitvec` crate for arbitrary-width bitmasks (heap-allocated, slower)
- Component grouping: combine related components into one type

### Change Detection Ticks

Every component instance has two tick values (u32 × 2 = 8 bytes) recording when it was added and last changed:

```rust
struct ComponentTicks {
    added: Tick,     // World tick when component was added to entity
    changed: Tick,   // World tick when component was last mutated
}
```

The world increments a global tick each frame. When a system runs:
1. Record `last_run = current_tick`
2. Iterate entities
3. For `Changed<T>` filter: check if `component.changed > system.last_run`
4. For `Added<T>` filter: check if `component.added > system.last_run`
5. On `&mut T` deref: write `current_tick` to `component.changed`

**Optimization**: Ticks are stored in a parallel Vec to component data, so the filter only touches tick data (8 bytes/entity) not component data (potentially much larger).

### Command Buffer Deferral

Structural changes (add/remove component, destroy entity) cannot happen during iteration (would invalidate iterators). Instead, queue commands and execute them after all systems run:

```rust
// During system execution:
commands.add_component(entity, Health(100));  // Queued, not executed yet

// After all systems:
command_queue.execute(world);  // Batch execute all structural changes
```

**Optimization opportunities:**
- Batch-sort commands by archetype for cache-friendly execution
- Deduplicate (add+remove same component = no-op)
- Coalesce multiple adds/removes on same entity

### Scheduler Conflict Detection

The scheduler determines which systems can run in parallel by checking component access conflicts:

```
System A: reads Position, writes Velocity
System B: reads Position, reads Health    → Parallel (no write conflicts)
System C: writes Velocity                 → Sequential after A (write conflict on Velocity)
System D: writes Position                 → Sequential after A and B (write conflict on Position)
```

**Optimization**: Precompute the conflict matrix at registration time. Each system stores a `conflicts_with: Vec<usize>` — a list of system indices it conflicts with. Frame scheduling becomes O(systems²) in the worst case but with precomputed masks it's O(systems × avg_conflicts), and avg_conflicts is typically small.

### Entity ID Packaging

Pack an entity ID into a single integer (index + generation) for cheap copying and hashing:

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct Entity(u64);

impl Entity {
    fn index(&self) -> u32 { self.0 as u32 }        // Low 32 bits: array index
    fn generation(&self) -> u32 { (self.0 >> 32) as u32 }  // High 32 bits: generation
}
```

**Generation counter**: When an entity is destroyed, its generation is incremented. If code holds a stale Entity handle and tries to use it, the generation won't match → safe error. This enables the "free list" pattern without use-after-free risk.

### Free List for Entity Recycling

Instead of `Vec::remove()` (O(n) shift), use a free list:

```rust
struct EntityPool {
    entities: Vec<EntityData>,
    free_indices: Vec<u32>,  // Stack of recycled indices (LIFO)
    generations: Vec<u32>,   // Generation for each index
}

fn allocate(&mut self) -> Entity {
    match self.free_indices.pop() {
        Some(idx) => Entity::new(idx, self.generations[idx]),
        None => {
            let idx = self.entities.len() as u32;
            self.entities.push(EntityData::default());
            self.generations.push(0);
            Entity::new(idx, 0)
        }
    }
}

fn free(&mut self, entity: Entity) {
    let idx = entity.index();
    self.generations[idx] += 1;  // Invalidate all outstanding handles
    self.free_indices.push(idx); // Recycle the slot
}
```

### Query Archetype Cache

Queries need to know which archetypes contain matching components. Scanning all archetypes every frame is wasteful when archetypes rarely change. The generation-counter pattern (§14) solves this:

1. World maintains `archetype_generation: u64`, bumped on archetype add/remove
2. Query caches `(generation, Vec<ArchetypeId>)`
3. Before iteration: if generation matches, use cache; otherwise rebuild

### Parallel Query Execution

Split matching archetypes into work chunks and distribute across threads:

```rust
// Each thread gets a contiguous range of entities from one or more archetypes
archetype_ranges.par_iter().for_each(|range| {
    for i in range.start..range.end {
        // Process entity at archetype.entities[i]
    }
});
```

**Key insight**: Entity data within an archetype is contiguous and independent — perfect for parallel iteration. No per-entity synchronization needed.

---

## 16. Case Studies

Real optimization passes from this project, with before/after code and benchmark results.

### Case Study 1: Clone Elimination in Archetype Migration (Pass 17, ↓19.4%)

**Problem**: Moving entities between archetypes (adding/removing components) cloned every component value, then dropped the originals.

```rust
// BEFORE: clones each component during migration
for component_id in archetype.component_ids() {
    let old_storage = old_archetype.get_storage(component_id);
    let new_storage = new_archetype.get_storage_mut(component_id);
    let value = old_storage.get(entity_index);
    new_storage.push(value.clone());  // Clone! Heap allocation + memcpy
}
// ... then old_storage.swap_remove(entity_index); // Drop original
```

**Fix**: Move instead of clone. Since the old storage discards the value immediately after (swap_remove), there's no need to clone:

```rust
// AFTER: swap_remove returns the value, push moves it into new storage
for component_id in archetype.component_ids() {
    let old_storage = old_archetype.get_storage_mut(component_id);
    let new_storage = new_archetype.get_storage_mut(component_id);
    let value = old_storage.swap_remove(entity_index);  // Take ownership
    new_storage.push(value);  // Move — no clone, no extra allocation
}
```

**Result**: 19.4% faster archetype migration. For a component like `String` or `Vec<u8>`, clone = heap alloc + memcpy; move = pointer copy (24 bytes).

**Lesson**: Before cloning, ask "does the source need its value afterward?" If not, move.

### Case Study 2: `Arc<dyn Fn>` → `fn` Pointers (Pass 7, ↓18.1%)

**Problem**: Component copy functions were stored as `Arc<dyn Fn(&TypeMap, &mut TypeMap, usize, usize)>` inside a HashMap. Every call involved:
1. Atomic reference count bump (Arc::clone on the function pointer)
2. Virtual dispatch through vtable (dyn Fn)
3. Heap allocation for the Arc itself

```rust
// BEFORE
type ComponentCopier = Arc<dyn Fn(&TypeMap, &mut TypeMap, usize, usize)>;
copiers: HashMap<ComponentId, ComponentCopier>,
```

**Fix**: Function pointers are `Copy`, have no vtable, and require no allocation:

```rust
// AFTER
type ComponentCopier = fn(&TypeMap, &mut TypeMap, usize, usize);
copiers: HashMap<ComponentId, ComponentCopier>,
```

**Result**: 18.1% faster on the migration benchmark (cumulative with Pass 17: 22.2% total).

**Lesson**: If you're storing a closure that doesn't capture anything, use a plain `fn` pointer. It's Copy (no Arc needed), has static dispatch (no vtable), and costs 8 bytes (one pointer).

### Case Study 3: Archetype Generation Cache (Pass 24, ↓4.8%)

**Problem**: Every query scanned all archetypes every frame to find matching ones. With 50 archetypes and 10 systems, that's 500 archetype-match operations per frame — each doing bitmask ANDs and filter-pair iteration.

**Fix**: Add a `u64` generation counter, bumped when archetypes are added/removed. Queries cache their matching archetype list keyed by this counter:

```rust
// World
archetype_generation: u64,  // Bumped on add_archetype / remove_archetype

// Query
cached_matches: Vec<ArchetypeId>,
cached_generation: u64,

fn matching_archetype_ids(&mut self) -> &[ArchetypeId] {
    if self.cached_generation != self.world.archetype_generation {
        // Rebuild: scan all archetypes, store matching IDs
        self.cached_matches = self.world.archetypes.iter()
            .filter(|(_, arch)| arch.matches_query(...))
            .map(|(id, _)| *id)
            .collect();
        self.cached_generation = self.world.archetype_generation;
    }
    &self.cached_matches
}
```

**Result**: 4.8% faster frame loop (full integration benchmark). The scan cost was significant even with only ~50 archetypes.

**Lesson**: The generation-counter pattern (§14) is the single most effective optimization for "scan-and-filter" workloads. Integer comparison replaces O(n) scanning.

### Case Study 4: `entry().or_default()` → `get_mut().expect()` — REVERTED (Pass 27)

**Hypothesis**: `HashMap::entry()` does two lookups (one to find/insert, one to return value). Using `get_mut()` directly should be one lookup.

**Attempt**:
```rust
// BEFORE
let entry = map.entry(key).or_default();

// AFTER
let entry = map.get_mut(&key).expect("key should exist");
```

**Result**: No improvement, slight regression. HashBrown's `entry()` API is already well-optimized — the "second lookup" is actually just a pointer dereference from the entry's internal state.

**Lesson**: Don't assume the standard library is naive. HashBrown, BTreeMap, and Vec are extremely well-optimized. Trust benchmarks over intuition.

---

## 17. When to Stop

### Signs You're Done

1. **Benchmarks show < 2% changes** for successive attempts
2. **The remaining hot path is memory-bound** (cache misses, not CPU)
3. **Further optimizations require algorithmic redesign** (not local changes)
4. **All low-hanging fruit from §2 is picked**

### Signs You Should Revert

1. **The change adds `unsafe` without measurable benefit**
2. **The change makes the code harder to understand**
3. **The change helps one benchmark but hurts another**
4. **The improvement is within measurement noise**

### The 80/20 Rule

80% of the performance comes from 20% of the code. After you've optimized that 20%, further gains require exponentially more effort. Know when to ship.

---

## 18. Tool Reference

### Must-Have

| Tool | Purpose | Install |
|---|---|---|
| **Criterion.rs** | Statistical benchmarking | `cargo add criterion --dev` |
| **cargo-asm** | Assembly inspection | `cargo install cargo-asm` |
| **Compiler Explorer** | Quick ASM checks | [godbolt.org](https://godbolt.org) |
| **flamegraph** | Visualization | `cargo install flamegraph` |

### Nice-to-Have

| Tool | Purpose | Platform |
|---|---|---|
| **perf** | Hardware performance counters | Linux |
| **Instruments** | CPU/memory profiling | macOS |
| **WPR/WPA** | CPU/memory profiling | Windows |
| **VTune** | Advanced CPU profiling | All |
| **heaptrack** | Heap allocation profiling | Linux |
| **cachegrind** | Cache miss simulation | Linux |
| **cargo-bloat** | Binary size analysis | All |
| **cargo-llvm-lines** | Compile-time line counts | All |

### Python Benchmark Runner Template

```python
#!/usr/bin/env python3
"""Run all Criterion benchmarks and collect results."""
import subprocess, re, json, sys
from pathlib import Path

CRITERION_RE = re.compile(
    r"test\s+(\S+)\s+\.\.\.\s+bench:\s+([\d,]+)\s+ns/iter\s+\(\+\/-\s+([\d,]+)\)",
    re.MULTILINE,
)

GROUP_TO_BENCH = {
    "entity_lifecycle": "entity_lifecycle",
    "query_iteration": "query_iteration",
    # ...
}

FAST_FLAGS = [
    "--sample-size", "25",
    "--warm-up-time", "0.5",
    "--measurement-time", "2",
]

def run_bench(group: str) -> dict[str, float]:
    binary = GROUP_TO_BENCH[group]
    cmd = ["cargo", "bench", "--bench", binary, "--"] + FAST_FLAGS
    result = subprocess.run(cmd, capture_output=True, text=True, cwd=PROJECT_ROOT)
    output = result.stdout + result.stderr
    results = {}
    for match in CRITERION_RE.finditer(output):
        name = match.group(1)
        mean_ns = float(match.group(2).replace(",", ""))
        results[name] = mean_ns
    return results

def main():
    baseline = load_baseline()
    for group in GROUP_TO_BENCH:
        results = run_bench(group)
        compare_and_report(group, baseline.get(group, {}), results)
        save_baseline(group, results)
```

---

## 19. Anti-Patterns & Lessons Learned

### Anti-Pattern: Optimizing Without a Benchmark

> "This must be faster."

No. Measure it. Three out of four "obvious" optimizations made zero difference in our ECS project.

### Anti-Pattern: Keeping Dead Optimizations

> "It didn't help, but it doesn't hurt to leave it."

It does hurt. Dead `unsafe` blocks, extra fields, and unused methods accumulate and confuse future readers. **Revert what doesn't work.**

### Anti-Pattern: Micro-Optimizing Before Profiling

> "I'll just inline this function and hoist that variable..."

LLVM already did it. Focus on algorithmic changes first.

### Anti-Pattern: Trusting a Single Benchmark Run

Always check variance. A 3% "improvement" with ±5% variance is noise. Run benchmarks at least twice before concluding.

### Lesson: `cargo asm` on Windows is Painful

MSVC uses PDB debug info, which `cargo asm` struggles to parse. Workarounds:
- Use `--bench` instead of `--lib` to get monomorphized functions
- Install the GNU toolchain: `rustup toolchain install stable-x86_64-pc-windows-gnu`
- Or just use source-level analysis + Compiler Explorer

### Lesson: HashBrown `entry()` is Fast

We tried replacing `entry().or_default()` with `get_mut().expect()` on the assumption that entry lookups are slow. They're not — HashBrown's entry API is already well-optimized. The change showed a regression.

### Lesson: LLVM Eliminates Bounds Checks

We added `get_unchecked()` methods to skip bounds checks in the query hot path. Zero measurable improvement. With ThinLTO + CGU=1, LLVM already hoists and eliminates bounds checks when the loop bound is visible.

### Lesson: Function Pointers Beat `Arc<dyn Fn>`

Replacing `Arc<dyn Fn(...)>` with plain `fn(...)` pointers (Pass 24) eliminated atomic reference counting and virtual dispatch. Function pointers are `Copy` — no allocation, no refcount, no vtable. This was a measurable win.

### Lesson: `#[cold]` Works

Moving error formatting (`Display` impls for error types) to `#[cold]` functions keeps them out of the hot icache. Tiny gain but zero cost.

---

## Appendix A: Data-Oriented Design (DOD)

Data-Oriented Design is a design philosophy, not an optimization technique. It's the principle that informed the ECS archetype layout, and it generalizes far beyond ECS.

### The Core Principle

> **Design your data structures around how the data is accessed, not around the conceptual objects in your domain.**

Object-Oriented Programming asks: "What objects exist?" Data-Oriented Design asks: "What data is processed together?"

### OOP vs DOD: A Before/After

**OOP approach** (mental model: "a game has entities, entities have components"):
```rust
struct Entity {
    id: u64,
    position: Position,
    velocity: Velocity,
    health: Health,
    // ... 20 more components
}
let entities: Vec<Entity> = ...;

// Processing movement: touches 3 fields of a 200-byte struct
for entity in &mut entities {
    entity.position.x += entity.velocity.x;  // Loads 200 bytes, uses 24
    entity.position.y += entity.velocity.y;
}
```

**DOD approach** (mental model: "positions and velocities are processed together"):
```rust
struct World {
    positions: Vec<Position>,    // 12 bytes each
    velocities: Vec<Velocity>,    // 12 bytes each
    healths: Vec<Health>,         // 4 bytes each
    // Each array indexed by entity_index
}

// Processing movement: touches exactly the data needed
for (pos, vel) in positions.iter_mut().zip(velocities.iter()) {
    pos.x += vel.x;  // Hot: 24 bytes per entity in cache (vs 200)
    pos.y += vel.y;
}
```

### Hot/Cold Splitting

Split data structures by access frequency:

```rust
// HOT data: accessed every frame, keep in L1/L2 cache
struct HotData {
    position: Position,
    velocity: Velocity,
}

// COLD data: accessed rarely, can live in L3 or RAM
struct ColdData {
    name: String,
    description: String,
    created_at: Instant,
}

// Entity stores index into both arrays
struct Entity {
    hot_idx: usize,
    cold_idx: usize,
}
```

The hot loop only touches `HotData` — it fits in cache even with millions of entities.

### SoA vs AoS Decision Matrix

| Pattern | Best For | Worst For |
|---|---|---|
| **AoS** `Vec<Entity>` | Random access (need all fields of one entity) | Component-wise iteration (touch only Position of all entities) |
| **SoA** `Vec<Position>, Vec<Velocity>` | Component-wise iteration (movement system) | Random entity access (debug UI clicking an entity) |
| **Hybrid** chunked AoS | SIMD on small fixed groups | General case — complex indexing |

**ECS naturally uses SoA** because the dominant access pattern is "iterate all Position + Velocity".

### When NOT to Use DOD

- Small N (<100 entities): layout doesn't matter, optimize for readability
- Dominant access is random entity lookup: AoS may be faster (single struct per entity)
- Code clarity matters more than performance (non-hot paths)
- Rapid prototyping: OOP is faster to write, optimize later

### Further Reading

- Mike Acton's CppCon 2014 talk: "Data-Oriented Design and C++"
- Richard Fabian's book: "Data-Oriented Design"
- Andrew Kelley's talk: "A Practical Guide to Applying Data-Oriented Design"

---

## Appendix B: Lock-Free & Wait-Free Programming

### Definitions

| Term | Meaning | Example |
|---|---|---|
| **Lock-free** | At least one thread makes progress in bounded time, even if others stall | `AtomicU64::fetch_add` |
| **Wait-free** | Every thread makes progress in bounded time | Simple atomic loads/stores with `Relaxed` |
| **Lock-based** | Thread may block indefinitely waiting for a lock | `Mutex::lock()` |
| **Obstruction-free** | Single thread can make progress if all others are paused | Most `compare_exchange` loops |

### When to Use Lock-Free

**Use lock-free when:**
- The critical section is extremely short (<10 instructions)
- Contention is high (many threads want the same data)
- You cannot afford context-switch latency (audio thread, game loop)
- You need guaranteed progress (real-time systems)

**Don't use lock-free when:**
- The operation is complex (multiple steps that must be atomic)
- The data structure is large (lock-free linked lists are very complex)
- You're not sure it's correct (lock-free bugs are subtle and catastrophic)
- A `parking_lot::Mutex` is fast enough (it usually is)

### The Simplest Lock-Free Pattern: Atomic Counter

```rust
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

// Thread 1..N: increment
let id = COUNTER.fetch_add(1, Ordering::Relaxed);
```

`fetch_add` is lock-free on all modern CPUs. It maps to `lock xadd` on x86 (~20 cycles).

### Compare-and-Swap (CAS) Loop

The universal lock-free primitive:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

fn try_increment_max(counter: &AtomicU64, max: u64) -> Result<u64, u64> {
    loop {
        let current = counter.load(Ordering::Acquire);
        if current >= max {
            return Err(current);  // Would exceed max
        }
        match counter.compare_exchange_weak(
            current, current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(current + 1),  // Successfully incremented
            Err(_) => continue,                // Someone else changed it, retry
        }
    }
}
```

**CAS loop costs**:
- Uncontended: ~10-20 cycles (single `lock cmpxchg`)
- Mildly contended (2-4 threads): ~30-100 cycles (some retries)
- Heavily contended (>8 threads): ~200+ cycles (many retries, cache line bouncing)

### Memory Ordering Decision Tree

```
Do you need ordering between threads?
├── No → Relaxed
└── Yes
    ├── Is this a store that must be visible to other threads?
    │   └── Yes → Release
    ├── Is this a load that must see another thread's stores?
    │   └── Yes → Acquire
    ├── Is this a read-modify-write (fetch_add, compare_exchange)?
    │   └── Yes → AcqRel (or Relaxed if no ordering needed)
    └── Do you need total global ordering (rare)?
        └── Yes → SeqCst (expensive on ARM/RISC-V)
```

### Common Lock-Free Data Structures

| Structure | Crate | Notes |
|---|---|---|
| **Queue (MPSC)** | `crossbeam::queue::SegQueue` | Multi-producer, single-consumer |
| **Queue (MPMC)** | `crossbeam::queue::ArrayQueue` | Bounded, lock-free |
| **Stack** | `crossbeam::epoch::Atomic` | Treiber stack, epoch-based GC |
| **HashMap** | `dashmap` | Sharded RwLock, mostly lock-free reads |
| **Slab** | `sharded-slab` | Lock-free object storage |

### The ABA Problem

The most famous lock-free bug. Thread 1 reads value A, gets preempted. Thread 2 changes A→B→A. Thread 1 resumes, CAS succeeds (value is "still A") but the object has changed.

**Solutions**:
- Tagged pointers: pack a generation counter into the pointer's unused bits
- Epoch-based reclamation (`crossbeam::epoch`): defer freeing until all threads have moved past a "safe point"
- Hazard pointers: each thread publishes what it's accessing; don't free while published
- RCU (Read-Copy-Update): readers never block; writers create new copies

**For ECS**: The entity generation counter is an ABA solution — recycling an entity slot bumps the generation, so a stale handle won't match.

### Practical Advice

1. **Start with locks** (`parking_lot::Mutex`). Measure. Only go lock-free if benchmarks show lock contention is the bottleneck.
2. **Use established crates** (`crossbeam`, `dashmap`). Lock-free data structures are extremely hard to get right.
3. **Test with `loom`**: `cargo test --test loom_tests` — model-checks concurrent code for all possible interleavings.
4. **Use `ThreadSanitizer`**: `RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test` — detects data races at runtime.

---

## Appendix C: Build-Time Optimization

Slow builds kill iteration speed. These techniques reduce compile times without affecting runtime performance.

### sccache — Shared Compilation Cache

```bash
cargo install sccache
# In .cargo/config.toml:
[build]
rustc-wrapper = "sccache"
```

Caches compiled artifacts across projects. Especially effective in CI (shared cache across builds) and when switching branches.

### `mold` Linker — Faster Linking

```bash
# Install mold (Linux only)
sudo apt install mold

# In .cargo/config.toml:
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

mold links 5-10× faster than GNU ld and 2-3× faster than lld. Most impactful with LTO (linking is the bottleneck with LTO).

On macOS: use `zld` (similar concept) or Apple's built-in linker (already fast).

### Workspace Organization

Split large crates into smaller ones. Unchanged crates don't recompile:

```
[workspace]
members = [
    "ecs_core",      # Core types, rarely changes
    "ecs_query",     # Query system
    "ecs_scheduler", # Scheduling
    "ecs_scripting", # Scripting support
    "ecs_app",       # Top-level application (binary)
]
```

**The rule**: Move stable code into separate crates. Only the crate you're actively editing recompiles.

### Profile Overrides

Fast iteration profile (debug info, some optimizations):

```toml
[profile.dev]
opt-level = 0        # Fast compile
debug = true         # Debug info
incremental = true   # Reuse previous compilation

[profile.dev-opt]
inherits = "dev"
opt-level = 1        # Moderate optimization
debug = true
# Compiles 2-3× slower than dev, runs 5-10× faster
```

Use `dev-opt` for development when you need closer-to-release performance but still want debug info and reasonable compile times.

### Dynamic Linking (Dev Only)

```toml
[profile.dev]
# Link system libraries dynamically (faster incremental linking)
# Not recommended for release (portability issues)
```

Or use `cargo-add-dynamic` for per-dependency dynamic linking.

### CI Caching

```yaml
# GitHub Actions
- uses: actions/cache@v3
  with:
    path: |
      ~/.cargo/registry/cache/
      ~/.cargo/git/db/
      target/
    key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
```

### `cargo-check` vs `cargo-build`

`cargo check` skips codegen — it only type-checks. 3-5× faster than `cargo build`:

```bash
cargo check   # For IDE-like feedback (errors only)
cargo build   # When you need to run
```

---

## Appendix D: Performance Regression Testing

Prevent performance regressions from reaching production by integrating benchmarks into CI.

### Setting Up CI Benchmarks

```yaml
# .github/workflows/benchmarks.yml
name: Benchmark Regression Test
on: [pull_request]

jobs:
  benchmark:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
      - name: Run benchmarks
        run: cargo bench --bench query_iteration -- --output-format bencher
      - name: Compare with baseline
        run: python3 scripts/compare_benchmarks.py
```

### GitHub Action: `benchmark-action`

Uses `github-action-benchmark` to track benchmarks over time and post results as PR comments:

```yaml
- uses: benchmark-action/github-action-benchmark@v1
  with:
    tool: 'cargo'
    output-file-path: output.txt
    github-token: ${{ secrets.GITHUB_TOKEN }}
    auto-push: true
    alert-threshold: '120%'  # Alert if >20% regression
    comment-on-alert: true
```

### `cargo-criterion` — Machine-Readable Output

```bash
cargo install cargo-criterion
cargo criterion --message-format json > results.json
```

Produces structured JSON that you can diff against a baseline:

```python
import json

def compare_benchmarks(baseline_json, current_json):
    baseline = json.load(open(baseline_json))
    current = json.load(open(current_json))

    for name, base_data in baseline.items():
        curr_data = current.get(name)
        if not curr_data:
            continue
        base_mean = base_data["mean"]["point_estimate"]
        curr_mean = curr_data["mean"]["point_estimate"]
        change_pct = (curr_mean - base_mean) / base_mean * 100

        if abs(change_pct) > 5:
            direction = "↑" if change_pct > 0 else "↓"
            print(f"⚠ {name}: {direction}{abs(change_pct):.1f}% ({base_mean:.0f} → {curr_mean:.0f} ns)")
```

### Thresholds for CI Alerts

| Change | Action |
|---|---|
| **> 20% slower** | Block PR — investigate immediately |
| **10-20% slower** | Warn — likely needs attention |
| **5-10% slower** | Comment — reviewer should consider |
| **< 5% change** | Within noise — ignore |
| **> 5% faster** | Nice! Record new baseline |

### CodSpeed — Specialized CI Benchmarking

[CodSpeed](https://codspeed.io) runs benchmarks in CI with hardware-level isolation (not affected by noisy neighbors in shared CI runners). Integrates with GitHub Actions and posts results on PRs.

### Local Regression Check Script

```bash
#!/bin/bash
# scripts/perf-check.sh
set -e

# Stash current changes
git stash

# Run benchmarks on base
cargo bench --bench query_iteration -- --save-baseline base

# Restore changes
git stash pop

# Run benchmarks on PR
cargo bench --bench query_iteration -- --baseline base

# Criterion will report "change: X%" for each benchmark
```

---

## Appendix E: Binary Size Optimization

Smaller binaries load faster, use less disk, and are important for WASM, embedded, and mobile.

### Why Binary Size Matters

- **WASM**: Every byte counts — smaller = faster download and parse
- **Embedded**: Flash storage is limited (often 256KB–1MB)
- **Mobile**: App store size limits, download over cellular
- **Desktop**: Less important, but still affects startup time (fewer pages to mmap)

### Size Optimization Profile

```toml
[profile.release-small]
inherits = "release"
opt-level = "s"       # or "z" for aggressive size reduction
lto = true            # Full LTO removes more dead code
codegen-units = 1
strip = true
panic = "abort"       # Remove unwinding tables
```

- `opt-level = "s"`: Optimize for size with moderate speed (usually 5-10% slower than `3`)
- `opt-level = "z"`: Aggressive size reduction (may be 10-20% slower than `3`)

### Analyzing Binary Size

```bash
# What's taking up space?
cargo bloat --release --crates
# Output: size per crate, sorted by size

# What about within a crate?
cargo bloat --release --bin my_binary -n 20
# Output: top 20 largest functions/symbols

# Deep analysis: what's in each section?
cargo bloat --release --bin my_binary --filter section
# .text = code, .rodata = constants, .data = globals, .bss = zeroed globals
```

### Common Size Culprits

| Culprit | Fix |
|---|---|
| **Generic monomorphization** | Use `dyn` trait objects for non-hot paths, extract non-generic inner functions |
| **Large `&'static str` arrays** | Compress/decompress at runtime, or load from file |
| **Deep dependency trees** | `cargo tree --duplicates` — find and deduplicate |
| **Debug symbols in release** | `strip = true` (already in profile) |
| **Panic strings (format args)** | `panic = "abort"` removes some, but `panic_immediate_abort` (nightly) removes more |
| **Serde derive macros** | Generated code can be large; consider `miniserde` or manual impls for hot types |

### Reducing Monomorphization Bloat

```rust
// BLOAT: generic function instantiated 50 times for 50 types
fn process<T: MyTrait>(items: &[T]) {
    for item in items {
        // 500 lines of code, monomorphized 50 times = 25K lines
    }
}

// SLIM: extract non-generic inner function
fn process_inner(item: &dyn MyTraitInner) {
    // 500 lines of code, compiled ONCE
}

fn process<T: MyTrait>(items: &[T]) {
    for item in items {
        process_inner(item);  // Thin wrapper, 1 line
    }
}
```

### `cargo-tree` for Dependency Analysis

```bash
# Find duplicate crate versions (common source of bloat)
cargo tree --duplicates

# Show all dependencies with features
cargo tree -e features

# Find why a heavy crate is being pulled in
cargo tree --invert -p regex
```

### WASM-Specific Optimizations

```toml
[profile.release]
opt-level = "s"
lto = true
codegen-units = 1
panic = "abort"
strip = true

# Additional WASM tools:
# wasm-opt -Oz input.wasm -o output.wasm  (Binaryen optimizer)
# twiggy top -n 20 output.wasm             (WASM size profiler)
```

---

## Appendix: Quick Reference Card

### Pre-Optimization Checklist

- [ ] Is there a benchmark for this code path?
- [ ] Have I run it and recorded the baseline?
- [ ] Is this code actually in a hot path (per-entity, per-frame)?
- [ ] Can this be solved algorithmically instead?

### Optimization Checklist

- [ ] Single change per pass
- [ ] Created PASS.md with hypothesis
- [ ] Ran benchmarks, captured before/after
- [ ] Checked variance — is the change real?
- [ ] If < 2% or > noise: revert
- [ ] If improved: update PASS.md with result
- [ ] All tests still pass

### Release Profile Checklist

```toml
[profile.release]
opt-level = 3
lto = "thin"          # or "fat" for max perf
codegen-units = 1
strip = true          # optional
```

### Key Criterion Commands

```bash
# Fast iteration
cargo bench --bench NAME -- --sample-size 25 --warm-up-time 0.5 --measurement-time 2

# Full run with baseline comparison
cargo bench --bench NAME

# List benchmarks without running
cargo bench --bench NAME -- --list
```
