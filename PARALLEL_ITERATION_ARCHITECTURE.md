# Parallel Iteration Architecture — Slices, Work Groups & Cache Lines

## 0. CORES vs THREADS vs CACHE — THE PHYSICAL REALITY

Before diving into slices and work groups, you need to understand what
your CPU actually looks like on the silicon.

### Your i7-12700KF, physically:

```
┌──────────────────────────────────────────────────────────────────┐
│                     CPU PACKAGE (one chip)                        │
│                                                                   │
│  ┌─────────────────────┐  ┌─────────────────────┐                │
│  │     P-CORE #0        │  │     P-CORE #1        │  ... ×8      │
│  │  (Golden Cove)       │  │  (Golden Cove)       │              │
│  │  ┌────┐ ┌────┐      │  │  ┌────┐ ┌────┐      │              │
│  │  │T0  │ │T1  │      │  │  │T0  │ │T1  │      │              │
│  │  │regs│ │regs│      │  │  │regs│ │regs│      │              │
│  │  └──┬─┘ └──┬─┘      │  │  └──┬─┘ └──┬─┘      │              │
│  │     └──┬──┘         │  │     └──┬──┘         │              │
│  │    ┌───┴───┐        │  │    ┌───┴───┐        │              │
│  │    │ L1 D$ │ 48 KiB │  │    │ L1 D$ │ 48 KiB │              │
│  │    │(shared│        │  │    │(shared│        │              │
│  │    │by T0  │        │  │    │by T0  │        │              │
│  │    │& T1)  │        │  │    │& T1)  │        │              │
│  │    └───┬───┘        │  │    └───┬───┘        │              │
│  │    ┌───┴───┐        │  │    ┌───┴───┐        │              │
│  │    │ L2 $  │1.25 MiB│  │    │ L2 $  │1.25 MiB│              │
│  │    │(shared│        │  │    │(shared│        │              │
│  │    │by both│        │  │    │by both│        │              │
│  │    │threads│        │  │    │threads│        │              │
│  │    └───┬───┘        │  │    └───┬───┘        │              │
│  └───────┼─────────────┘  └───────┼─────────────┘              │
│          │                        │                              │
│  ┌───────┼────────────────────────┼──────────────────┐          │
│  │       └──────────┬─────────────┘                   │          │
│  │  ┌───────────────┴───────────────┐                 │          │
│  │  │         L3 CACHE (LLC)        │  25 MiB        │          │
│  │  │       SHARED BY ALL CORES     │                 │          │
│  │  └───────────────┬───────────────┘                 │          │
│  └──────────────────┼─────────────────────────────────┘          │
│                     │                                            │
│  ┌──────────────────┼─────────────────────────────────┐          │
│  │     E-CORE #0    │         E-CORE #1    ... ×4     │          │
│  │  (Gracemont)     │        (Gracemont)              │          │
│  │  ┌────┐          │        ┌────┐                   │          │
│  │  │T0  │ (1 thr)  │        │T0  │ (1 thread)       │          │
│  │  └──┬─┘          │        └──┬─┘                   │          │
│  │ ┌──┴──┐          │       ┌──┴──┐                   │          │
│  │ │L1 D$│ 32 KiB   │       │L1 D$│ 32 KiB            │          │
│  │ └──┬──┘          │       └──┬──┘                   │          │
│  │ ┌──┴──┐          │       ┌──┴──┐                   │          │
│  │ │L2 $ │ cluster  │       │L2 $ │ cluster           │          │
│  │ │     │ 2 MiB    │       │     │ 2 MiB             │          │
│  │ └──┬──┘          │       └──┬──┘                   │          │
│  └────┼─────────────┘  └───────┼──────────────────────┘          │
│       └────────────────────────┘                                  │
│                     │                                             │
│            ┌────────┴────────┐                                    │
│            │   MEMORY CTRL   │                                    │
│            └────────┬────────┘                                    │
└─────────────────────┼────────────────────────────────────────────┘
                      │
              ┌───────┴───────┐
              │   DDR5 RAM    │  32 GiB
              │  (off-chip)   │
              └───────────────┘
```

### The three levels, in plain English:

| Level | Location | Size | Shared by |
|---|---|---|---|
| **L1 data cache** | Inside each core | 32–64 KiB | All threads **on that core** |
| **L2 cache** | Inside each core (or cluster) | 256 KiB–2 MiB | All threads **on that core** |
| **L3 cache** | Separate slice on the chip | 8–36 MiB total | **Every core** on the chip |
| **RAM** | Separate sticks (DIMMs) | 8–64 GiB | **Everything** |

### Core ≠ Thread

A **core** is a physical processor on the silicon — it has its own L1, L2,
execution units, and register file. A **thread** (logical processor) is what
the OS creates to schedule work.

**Hyperthreading (Intel) / SMT (AMD):** one physical core pretends to be
two logical threads. The OS sees 20 "CPUs" but there are only 12 physical
cores. When the OS schedules two workloads on threads 0 and 1 of the same
P-core:

```
Thread 0 (OS CPU 0)  ──┐
                        ├── Same physical core ──┐
Thread 1 (OS CPU 1)  ──┘                        │
                                          ┌──────┴──────┐
                                          │  L1 D$ 48K  │ ← BOTH threads
                                          │  (SHARED!)  │    fight for this
                                          └─────────────┘
```

This is why we **half-fill** L1 — if both hyperthreads run ECS slices
simultaneously, each gets ~half the cache without evicting the other's data.

**⚠️⚠️⚠️⚠️⚠️ But this is a heuristic, not a guarantee.** ⚠️⚠️⚠️⚠️⚠️

Here's why: 

### The cache is NOT partitioned — the CPU decides what stays

L1 data cache has **no explicit partitioning mechanism**. Intel's Cache
Allocation Technology (CAT) works on L2 and L3, but not L1. The CPU uses a
**replacement policy** (typically pseudo-LRU or an adaptive variant) to decide
which cache line gets evicted when new data arrives.

```
Thread 0 processing slice [0..6144]:     Thread 1 processing slice [6144..12288]:
  reads pos[0] → cache line 0              reads pos[6144] → cache line 48
  reads pos[1] → same line, hit!           reads pos[6145] → same line, hit!
  ...                                       ...
  reads pos[64] → cache line 1             reads pos[6208] → cache line 49

Both threads stream linearly through memory.
Each new cache line evicts the OLDEST line (LRU), not the other thread's.
```

### Why linear iteration is cache-friendly

Our access pattern — sequential, forward-only, one-pass — is the **best case**
for cache sharing:

1. **Predictable:** the hardware prefetcher sees the stride and pulls upcoming
   cache lines before they're needed.
2. **Non-overlapping ranges:** Thread 0 touches addresses `[0x0000..0xC000]`,
   Thread 1 touches `[0xC000..0x18000]`. Different physical addresses map to
   different cache sets (L1 is 8-way or 12-way set-associative), so they
   don't compete for the same set.
3. **Use-once:** After processing `pos[42]`, we never revisit it within the
   same frame. The evicted line was going to be useless anyway.

### What COULD go wrong (and how to measure it)

| Scenario | Risk | Detection |
|---|---|---|
| Both threads access the exact same addresses | False sharing → cache line ping-pong | `perf stat -e cache-misses` |
| Working sets > L1, same cache sets | Thrashing → constant evictions | `cachegrind` (valgrind) D1mr |
| Random-access pattern (unlikely in ECS) | No prefetch, high miss rate | `perf stat -e l1d_pend_miss` |
| E-core (32K L1) given a 48K-sized slice | Guaranteed thrash | CPUID detection (we handle this) |

### The honest summary

We **cannot** partition L1 between threads. We rely on:
- **Half-fill** to leave headroom (statistically good enough)
- **Linear access** to make prefetching effective
- **Non-overlapping address ranges** to avoid set-conflict thrashing
- **Cachegrind benchmarks** to validate real-world miss rates

The slice size formula (`l1_bytes / 2 / component_size`) is an engineering
compromise, not a mathematical proof. It works well in practice, but the only
way to be certain on a specific CPU is to measure with hardware counters.

### What the OS reports vs reality

```
$ lscpu (Linux) / Task Manager (Windows)
  CPU(s):              20   ← "I see 20 logical processors"
  Thread(s) per core:   2   ← "P-cores have 2 threads each"
  Core(s) per socket:  12   ← "But only 12 physical cores!"
  8 P-cores + 4 E-cores = 12 physical, 20 logical
```

---

## 1. WHY CACHE LINES MATTER FOR AN ECS

Every entity is backed by component data stored linearly in archetype arrays.
When the CPU reads `velocity[42]`, it pulls an entire **cache line** (64 bytes
on x86_64) into L1 — so it also reads `velocity[43..49]` for free.

The goal: size each parallel slice so its **working set fits in L1 data cache**.
If a slice overflows L1, the CPU thrashes between L1 ↔ L2, adding ~12 cycles
per access (L2 latency) instead of ~4 (L1 latency). At 3000 FPS × 30000 entities,
that's millions of avoidable stalls.

```
┌─────────────────────────────────────────────────────┐
│                    CPU CORE                          │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │  L1 I$   │  │  L1 D$   │  │  Registers       │  │
│  │  32-64K  │  │  32-64K  │  │                  │  │
│  └──────────┘  └──────────┘  └──────────────────┘  │
│  ┌──────────────────────────────────────────────┐   │
│  │              L2 Cache (256K–2M)               │   │
│  └──────────────────────────────────────────────┘   │
│  ┌──────────────────────────────────────────────┐   │
│  │              L3 Cache (shared, 8–36M)         │   │
│  └──────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

**Cache size varies by CPU microarchitecture:**

| Microarchitecture | L1D per core | Entities per slice (8B comp.) |
|---|---|---|
| Intel Alder Lake P-core | 48 KiB | 6144 |
| Intel Alder Lake E-core | 32 KiB | 4096 |
| AMD Zen 4 | 32 KiB | 4096 |
| Apple M2 P-core | 64 KiB | 8192 |
| Intel Haswell | 32 KiB | 4096 |

The engine detects L1D at startup via `CPUID` and computes the slice size
automatically: `l1_bytes / component_size / 2` (half-fill for safety).

### Concrete benefit: L1 hits vs L1 misses

Here's what cache-aware slicing actually buys you, in real numbers:

```
Processing 30 000 entities, each with Position (8B) + Velocity (8B) = 16B:

  WITHOUT cache-aware sizing (e.g., 50 000 entities per slice):
  ┌─────────────────────────────────────────────────────────┐
  │ Working set: 50 000 × 16B = 800 KiB                     │
  │ L1D can hold: 48 KiB (P-core)                           │
  │ → Only 6% of data fits in L1                            │
  │ → 94% of accesses miss L1, go to L2 (12 cycles each)    │
  │ → Prefetcher helps, but still ~8 cycles avg per access  │
  └─────────────────────────────────────────────────────────┘

  WITH cache-aware sizing (6144 entities per slice):
  ┌─────────────────────────────────────────────────────────┐
  │ Working set: 6144 × 16B = 98 KiB                        │
  │ L1D can hold: 48 KiB (P-core)                           │
  │ → ~49% of data fits in L1 (half-fill leaves slack)      │
  │ → Most accesses hit L1 (4 cycles)                        │
  │ → Prefetcher pulls next slice while current runs         │
  │ → Effective avg: ~4-5 cycles per access                 │
  └─────────────────────────────────────────────────────────┘
```

**The latency difference per component access:**

| Cache level | Latency (cycles) | Latency (ns @ 4 GHz) | If you miss... |
|---|---|---|---|
| L1 hit | ~4 | ~1 ns | — |
| L2 hit | ~12 | ~3 ns | 3× slower |
| L3 hit | ~40 | ~10 ns | 10× slower |
| RAM | ~120+ | ~30+ ns | 30× slower |

**Scaling up: 30 000 entities, 2 components, 1 system per frame:**

| Slice strategy | Miss rate (est.) | Time per entity | Frame time | FPS |
|---|---|---|---|---|
| No sizing (50K slice) | ~90% L1 miss | ~40 ns | ~1.2 ms | ~833 |
| Static 4096 | ~50% L1 miss | ~20 ns | ~0.6 ms | ~1667 |
| **L1-aware 6144** | ~30% L1 miss | ~14 ns | ~0.42 ms | **~2380** |
| L1-aware + prefetch | ~15% L1 miss | ~10 ns | ~0.3 ms | ~3333 |

*These are ballpark estimates; real numbers depend on component size, filter
complexity, and system logic inside the closure. Measure on your hardware.*

### Why it matters more at higher FPS

Cache misses hurt **proportionally more** at high frame rates. At 60 FPS you
have 16.6 ms per frame — a 1 ms cache-miss penalty is only 6% of budget.
At 3000 FPS you have just 0.33 ms — that same 1 ms penalty is 3× your entire
budget. Cache-aware sizing is what makes 3000 FPS possible.

```
Frame budget at different FPS targets:

  60 FPS:  ████████████████░░░░░░░░░░░░░░░░  16.6 ms  (cache doesn't matter much)
 500 FPS:  ████░░░░░░░░░░░░░░░░░░░░░░░░░░░░   2.0 ms  (cache starts to matter)
3000 FPS:  █░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░   0.33 ms (cache is EVERYTHING)
```

At 3000 FPS, you're racing the cache hierarchy on every single access. A slice
that overflows L1 by even 10% can drop you from 3000 → 1800 FPS.

---

## 2. SLICES — THE BUILDING BLOCKS

A **slice** is a contiguous range of entity indices within a single archetype,
sized to fit in L1D cache. Slices are **not** assigned to threads directly —
they are the atomic units that get packed into work groups.

### The exact formula

```rust
// From config.rs — runs once at startup, cached via OnceLock:
fn default_entities_per_slice() -> usize {
    detect_l1_data_cache_size()        // e.g., 49152 (48 KiB)
        .map(|l1_bytes| l1_bytes / 8)  // ÷ 8 = 6144 entities
        .unwrap_or(4096)               // fallback if detection fails
}
```

**`l1_bytes / 8`** — three assumptions baked in:

| Assumption | Value | Why |
|---|---|---|
| Component size | 8 bytes | Conservative: one `f32×2` pair, one `u64`, or one pointer |
| Multi-component | ignored | Streaming access means only ~current cache line of each component array is live |
| Filter state | ignored | Filter metadata (ticks, change flags) is tiny relative to component data |

### Does the exact slice size matter? (Spoiler: not much)

The formula gives 6144. Would 5000 be worse? 8000? The honest answer: **barely.**

Cache lines are 64 bytes — the CPU always pulls a full line, never a partial
one. When you access `position[4999]`, the CPU loads the line containing
`position[4992..4999]` regardless of whether your slice ends at 5000 or 6144:

```
Slice of 5000 entities (not a multiple of 8):
  pos[0..7]    → line 0   ✓ processed
  pos[8..15]   → line 1   ✓ processed
  ...
  pos[4992..4999] → line 624  ✓ processed (8 entities, we only use pos[4992..4999])
  pos[5000..5007] → line 625  ✗ NOT processed (beyond slice boundary)
                               → This line is never touched. No waste.

  The "waste" is zero — we simply stop iterating. The last line we touched
  (line 624) was fully utilized (we processed all 8 entities in it).
```

There is no misalignment penalty for sequential access. The prefetcher doesn't
care whether you stop at entity 5000 or 6144 — it just sees sequential strides
and keeps pulling lines ahead. When you stop iterating, the prefetched-but-unused
lines sit in L1 until evicted by the next slice's data. Zero cost.

**What ACTUALLY matters — the rough order of magnitude:**

| Slice size | ~Cache lines per component | Problem? |
|---|---|---|
| 500 | 63 | Way too small — spawn overhead dominates |
| 2000 | 250 | Fine for tiny systems, wasteful spawn overhead |
| 4000 | 500 | Good — enough work per slice |
| 6144 (auto) | 768 | Good — matches L1 capacity |
| 8000 | 1000 | Still fine — streaming makes L1 overflow painless |
| 12000 | 1500 | Starting to push it — more L2 traffic |
| 50000 | 6250 | Bad — constant L1 thrashing, L2/L3 bound |

The formula exists to keep you out of the red zone (<2000 or >20000), not to
hit a magic number. The gap between 4000 and 8000 is ~5% performance at most,
dwarfed by the 3× difference between 4000 and 50000.

**The formula is a starting point, not a tuning target.** Use `with_batch_size()`
if you profile and find a better value for your specific component sizes and
query patterns. The auto-detected value is "good enough" for 95% of workloads.

### Is it really "half-fill"?

The code comment says half-fill, but the math is actually **full-fill**:
48 KiB ÷ 8 B = 6144 entities → exactly 48 KiB of component data.

It works because of **streaming access**: we iterate linearly, use each entity
once, and never revisit. The CPU's LRU eviction naturally discards old cache
lines as new ones stream in. Unlike a random-access workload (where you'd
want slack to avoid thrashing), sequential streaming can fill L1 to the brim
without penalty — the prefetcher stays ahead, and evicted lines were dead anyway.

### How many entities fit in ONE cache line?

A cache line is always **64 bytes** on x86_64. The number of entities per line
depends on the component's size:

```
Cache line (64 bytes):

  f32 (4 B):  ┌────┬────┬────┬────┬────┬────┬────┬────┬───┬───┬───┬───┬───┬───┬───┬───┐
              │ e0 │ e1 │ e2 │ e3 │ e4 │ e5 │ e6 │ e7 │e8 │e9 │e10│e11│e12│e13│e14│e15│
              └────┴────┴────┴────┴────┴────┴────┴────┴───┴───┴───┴───┴───┴───┴───┴───┘
              ← 16 entities in one cache line →

  f64 / Vec2  ┌────────┬────────┬────────┬────────┬────────┬────────┬────────┬────────┐
  (8 B):      │   e0   │   e1   │   e2   │   e3   │   e4   │   e5   │   e6   │   e7   │
              └────────┴────────┴────────┴────────┴────────┴────────┴────────┴────────┘
              ← 8 entities in one cache line →

  Vec4 / Mat2 ┌───────────────┬───────────────┬───────────────┬───────────────┐
  (16 B):     │      e0       │      e1       │      e2       │      e3       │
              └───────────────┴───────────────┴───────────────┴───────────────┘
              ← 4 entities in one cache line →
```

| Component type | Size | Entities per cache line | Cache lines for 6144-entity slice |
|---|---|---|---|
| `f32`, `u32`, `i32` | 4 B | 16 | 384 |
| `f64`, `u64`, `glam::Vec2` | 8 B | 8 | 768 |
| `glam::Vec4`, `glam::Mat2` | 16 B | 4 | 1536 |
| `glam::Mat4`, `[f32; 16]` | 64 B | 1 | 6144 |
| `Transform` (large) | 128 B | ½ (spans 2 lines) | 12288 |

**Smaller components = fewer cache lines per component array.** The 8-byte
assumption is conservative precisely because real ECS components (f32, u32)
are usually 4 bytes — they fit *twice as many* entities per line.

But the "spare" capacity doesn't sit idle. In a real query, the remaining
L1 space fills with:

```
Query<(&Position, &Velocity)> — both are f32×2 (8 B each):

  L1D (768 cache lines total, 48 KiB):

  ┌─────────────────────────────────────────────┐
  │ Position cache lines   (~384 lines, 24 KiB) │ ← current window of pos[]
  │ Velocity cache lines   (~384 lines, 24 KiB) │ ← current window of vel[]
  │ Prefetcher lookahead   (scattered across     │ ← lines pulled ahead by HW
  │                         both arrays)          │    prefetcher
  │ Filter ticks / metadata (few lines)          │ ← change detection state
  └─────────────────────────────────────────────┘

  Position and Velocity map to DIFFERENT sets (separate Vec allocations),
  so their 384+384 lines don't fight each other. The 12-way associativity
  per set easily handles the few lines from each array that land in the
  same set.
```

For a single-component query like `Query<&Position>` (just f32, 4 B):

```
  L1D (768 cache lines):

  ┌─────────────────────────────────────────────┐
  │ Position cache lines   (~192 lines, 12 KiB) │ ← 4 B components: 16/line
  │ Prefetcher lookahead   (~192 lines)          │ ← 8-16 lines ahead
  │ Unused / cold lines    (~384 lines)          │ ← available for other data
  └─────────────────────────────────────────────┘

  Single-component at 4 B/entity is the best case — the L1 is only ~25% full.
  You could triple the slice size and still not overflow. But the conservative
  8-byte formula keeps it safe for the multi-component common case.
```

The key insight: **"50% free" means free for the OTHER component's streaming
data** (which maps to different sets) and for prefetcher lookahead. It doesn't
mean wasted capacity — it means headroom that prevents thrashing when two
hyperthreads share the same L1 or when a query touches 2-3 components.

### The hardware prefetcher — your silent co-pilot

Modern CPUs don't wait for a cache miss to fetch data. They have **hardware
prefetchers** that detect sequential access patterns and pull upcoming cache
lines into L1 *before* your code asks for them:

```
Your code:        reads pos[0]  →  pos[1]  →  pos[2]  →  pos[3]  →  ...
                     │              │           │           │
CPU sees:         "stride = 4B,  "I'll fetch  "I'll fetch  "I'll fetch
                   sequential"    pos[8..15]   pos[16..23]  pos[24..31]
                                  now"          now"          now"
                                  │             │             │
Prefetcher:       ───────────────┼─────────────┼─────────────┼──►
                   pulls lines    │   lines     │   lines     │  lines
                   pos[0..7]      │  waiting    │  waiting    │ waiting
                                  │  in L1      │  in L1      │ in L1

Result: by the time your code reaches pos[15], pos[16..23] are already in L1.
You never wait for RAM — you're always hitting L1 (4 cycles) or L2 (12 cycles).
```

**What the prefetcher needs to work:**
- **Sequential, forward-only access** — we have this (ECS iteration is linear)
- **Predictable stride** — we have this (same component size every iteration)
- **No random jumps** — we have this (no pointer chasing, no hash maps)

**What kills the prefetcher:**
- Linked lists, trees, hash maps — pointer-chasing confuses stride detection
- Random shuffle iteration — if you iterate entities in random order, the
  prefetcher gives up and every access becomes a cache miss
- Gather/scatter patterns — `entity[random_index]` is death for prefetching

**Our ECS iteration is the ideal workload for prefetching.** Linear arrays,
fixed stride, no branches in the hot loop. The prefetcher stays 8-16 cache
lines ahead of the current position, effectively hiding L2/L3 latency.

### What about mixed component sizes? (e.g., 4 B + 64 B)

When a query touches components of different sizes, like `Query<(&Health_f32,
&Transform_Mat4)>`, the access pattern alternates:

```
Loop body:
  read health[0]   → 4 B, stride = 4 B
  read transform[0] → 64 B (spans 2 cache lines)
  read health[1]   → 4 B
  read transform[1] → 64 B
  ...
```

The CPU sees **two interleaved access streams** with different strides. Modern
prefetchers (Intel since Sandy Bridge, AMD since Zen) handle this fine — they
detect each stream independently:

```
Prefetcher stream 0: health array    stride = 4 B  → pulls lines ahead
Prefetcher stream 1: transform array stride = 64 B → pulls lines ahead (2 per entity)

  Time ──────────────────────────────────────────────────────►
  health:    [h0..h15]  [h16..h31]  [h32..h47]  [h48..h63]
              ──pull──   ──pull──    ──pull──    ──pull──
  transform: [t0..t0]   [t1..t1]    [t2..t2]    [t3..t3]
              ──2 lines─ ──2 lines── ──2 lines── ──2 lines──
              both streams prefetched independently and simultaneously
```

**The practical concern isn't the prefetcher — it's L1 residency:**

```
Query<(&f32, &Mat4)>, one slice = 6144 entities:

  f32 array:   6144 × 4 B = 24 KiB  → 384 cache lines (16 entities/line)
  Mat4 array:  6144 × 64 B = 384 KiB → 12 288 cache lines (½ entity/line!)

  L1D:         only 768 lines total
  f32 needs:   384 lines (fits easily in its sets)
  Mat4 needs:  12 288 lines → 16× more than L1 capacity

  Result: f32 data stays in L1 comfortably (small, dense).
          Mat4 data streams through L1 constantly — each line used once,
          evicted immediately by the next 2 Mat4 lines.
          Prefetcher works hard pulling Mat4 lines from L2/L3.
```

The small component is fine. The large component always overflows L1 regardless
of slice size — it's bandwidth-bound, not capacity-bound. But a **smaller slice
actually helps** by tightening the prefetch window:

```
Large slice (6144 entities, Mat4 component = 384 KiB total):

  Prefetcher sees: "I need to pull lines from a 384 KiB range."
  It pulls lines from 100+ KiB ahead of current position.
  
  ┌─ L1D (768 lines) ──────────────────────────────────────┐
  │ Current:  [Mat4 line N] [Mat4 line N+1]  ← processing   │
  │ Prefetch: [Mat4 N+8] [Mat4 N+9] ... [Mat4 N+128]       │
  │            ↑ lines pulled from WAY ahead                 │
  │                                                          │
  │ Problem: by the time we reach N+128, those prefetched    │
  │ lines were evicted by N+1..N+127 streaming through.      │
  │ The prefetcher DID work, but its work was WASTED —       │
  │ the lines didn't survive long enough in L1.              │
  │                                                          │
  │ Result: prefetch turns into "demand fetch" — stalls.     │
  └──────────────────────────────────────────────────────────┘

Smaller slice (2048 entities, Mat4 component = 128 KiB total):

  Prefetcher sees: "I need to pull lines from a 128 KiB range."
  It pulls lines from 30-50 KiB ahead of current position.

  ┌─ L1D (768 lines) ──────────────────────────────────────┐
  │ Current:  [Mat4 line N] [Mat4 line N+1]  ← processing   │
  │ Prefetch: [Mat4 N+8] [Mat4 N+9] ... [Mat4 N+32]        │
  │            ↑ closer to current, higher survival chance   │
  │                                                          │
  │ The prefetched lines only need to survive ~30 KiB of     │
  │ streaming data before they're used. At 64 B/line, that's │
  │ ~480 lines — fits in L1 alongside current window.        │
  │                                                          │
  │ Result: prefetched lines survive → fewer stalls.         │
  └──────────────────────────────────────────────────────────┘
```

### Timeline: why smaller slices keep prefetching effective

```
Time ───────────────────────────────────────────────────────────────►
       │          │          │          │          │          │
CPU:   process    process    process    NEED       NEED       STALL
       entity N   N+1..N+7   N+8..N+15  N+128      N+129      (wait
                                         │                     for L2)
                                         ▼
LARGE SLICE (6144 entities, prefetch distance spans 100+ KiB):

  Prefetcher:  [pull N+128]  ·  ·  ·  ·  ·  ·  ·  [pull N+256]  ·  ·  ·  ·  ·
  L1 contents: ┌─N──N+8─┐ → evicted → ┌─N+80─N+88─┐ → evicted → ┌─N+200─N+208─┐
               │ (alive) │   by N+1   │ (alive)    │   by N+81  │ (alive)     │
               └─────────┘  ..N+79    └────────────┘  ..N+199   └────────────┘
               ↑ N+128 was here                   ↑ GONE by the time
               but got pushed out!                  CPU needs it!

  Result: prefetcher worked, but every prefetched line gets evicted
  before use → CPU stalls on L2 (12 cycles) for EVERY cache line.

SMALL SLICE (2048 entities, prefetch distance spans ~30 KiB):

  Prefetcher:  [pull N+32] ·· [pull N+64] ·· [pull N+96] ·· [pull N+128] ··
  L1 contents: ┌─N──N+32─┐  ┌─N+32─N+64─┐  ┌─N+64─N+96─┐  ┌─N+96─N+128─┐
               │ (alive) │  │  (alive)  │  │  (alive)  │  │  (alive)   │
               └─────────┘  └───────────┘  └───────────┘  └───────────┘
               ↑ N+32 survives           ↑ N+64 survives  ↑ all survive
               until CPU needs it         until needed     until needed

  Result: prefetched lines survive the shorter trip through L1 →
  CPU finds them waiting → nearly zero stalls.

The critical ratio:  (prefetch distance in bytes) / (data per entity)
  Large slice: 100 000 / 64 = 1562 lines must survive → impossible in 768-line L1
  Small slice:  30 000 / 64 =  468 lines must survive → fits with room to spare
```

**In short:** the prefetcher has no concept of L1 capacity. It blindly pulls lines
at a fixed distance ahead (128-256 bytes on Intel, configurable in BIOS). If your
data range is huge, the distance in "L1 turnovers" is large — prefetched lines
get evicted by intermediate data before use. A smaller slice compresses the range,
so prefetched lines survive the shorter trip through L1.

**The trade-off:** smaller slice = fewer entities per rayon task = more groups =
more spawn overhead. There's a sweet spot. For a 64B component, try
`with_batch_size(L1_bytes / 64 / 2)` — roughly 384 entities — and benchmark
vs the default.

```
A good rule of thumb for mixed-size queries:

```
A good rule of thumb for mixed-size queries:

  If avg_component_size > 32 B → consider with_batch_size(L1 / avg_size / 2)
  If one component dominates total bytes → that component IS the bottleneck
  The prefetcher handles both streams fine — the issue is total bandwidth
```

### Why the prefetcher makes full-fill safe

Without prefetching, full-fill would be dangerous: when you finish cache line N
and move to line N+1, you'd stall waiting for it to arrive from L2. But the
prefetcher already pulled line N+1 (and N+2, N+3...) while you were processing
line N. By the time you need them, they're already in L1. The "evicted because
full" line was already consumed — its data is dead, and its slot is now occupied
by the next line the prefetcher pulled.

### Wait — doesn't a query touch MULTIPLE components?

Yes. Consider `Query<(&Position, &Velocity)>` on an archetype with both components.
Each entity access reads 16 bytes (8B + 8B), not 8. The slice contains 6144
entities → total data touched = 6144 × 16B = 96 KiB. But L1D is only 48 KiB.

Doesn't this overflow? **Yes, but it doesn't matter.** Here's why:

```
Archetype storage is SoA (Structure of Arrays), not AoS (Array of Structures):

MEMORY LAYOUT (each █ = 8 bytes = one f32×2 or one u64):

  Address      Position array (48 KiB)       Velocity array (48 KiB)
  ────────     ─────────────────────────     ─────────────────────────
  0x0000       ┌──────────────────────┐      ┌──────────────────────┐
  0x0008       │ pos[0]  ████████      │      │ vel[0]  ████████      │
  0x0010       │ pos[1]  ████████      │      │ vel[1]  ████████      │
  0x0018       │ pos[2]  ████████      │      │ vel[2]  ████████      │
  0x0020  ┌─── │ pos[3]  ████████      │      │ vel[3]  ████████      │
  0x0028  │ C  │ pos[4]  ████████      │      │ vel[4]  ████████      │
  0x0030  │ A  │ pos[5]  ████████      │      │ vel[5]  ████████      │
  0x0038  │ C  │ pos[6]  ████████      │      │ vel[6]  ████████      │
  0x0040  │ H  │ pos[7]  ████████ ─────┼──┐   │ vel[7]  ████████      │
          │ E  │ pos[8]  ████████      │  │   │ vel[8]  ████████      │
          │    │ pos[9]  ████████      │  │   │ vel[9]  ████████      │
          │ L  │ ...                   │  │ C │ ...                   │
          │ I  │                       │  │ A │                       │
          │ N  │ pos[6143] ████████    │  │ C │ vel[6143] ████████    │
          │ E  └──────────────────────┘  │ H └──────────────────────┘
          │                              │ E
          │    Health array (24 KiB)     │
          │   ┌──────────────────────┐   │
          │   │ hlth[0] ████████      │   │
          │   │ hlth[1] ████████      │   │
          │   │ ...                   │   │
          │   │ hlth[6143] ████████   │   │
          │   └──────────────────────┘   │
          └──────────────────────────────┘
               ↑ Separate arrays, separate memory regions, separate cache lines

ONE CACHE LINE (64 bytes) when CPU reads position[42]:
  ┌──────────────────────────────────────────────────────────┐
  │ pos[40] │ pos[41] │ pos[42] │ pos[43] │ ... │ pos[47]   │
  │  8 B    │  8 B    │  8 B ←  │  8 B    │     │  8 B      │
  └──────────────────────────────────────────────────────────┘
  Contains: 8 Position values. Does NOT contain Velocity or Health.

DIFFERENT cache line when CPU reads velocity[42] (different address):
  ┌──────────────────────────────────────────────────────────┐
  │ vel[40] │ vel[41] │ vel[42] │ vel[43] │ ... │ vel[47]   │
  │  8 B    │  8 B    │  8 B ←  │  8 B    │     │  8 B      │
  └──────────────────────────────────────────────────────────┘
  Maps to a DIFFERENT L1 cache set — no conflict with Position line.

CONTRAST — if this were AoS (which we DON'T use):
  ┌─────────────┬─────────────┬─────────────┐
  │  Entity 0   │  Entity 1   │  Entity 2   │
  │ pos[0]|vel[0]│ pos[1]|vel[1]│ pos[2]|vel[2]│  ← one cache line = 4 entities
  └─────────────┴─────────────┴─────────────┘
  BAD: Position+Velocity share cache lines → thrashing
  BAD: querying only Position still pulls Velocity (wasted bandwidth)

So 96 KiB of total data touched, but at any instant the L1 holds:
  ┌────────────────────────────────────────────┐
  │ ~5 lines from Position array (320 B)       │  ← current iteration window
  │ ~5 lines from Velocity array (320 B)       │  ← different L1 sets
  │ ~prefetched lines ahead of both            │
  │ ~filter state, ticks, metadata             │
  └────────────────────────────────────────────┘
  Total live: ~4-8 KiB — well under 48 KiB L1D
```

The **total** data touched is 96 KiB, but the **instantaneous working set**
(the data the CPU needs *right now*) is just a handful of cache lines from
each component array. The prefetcher streams new lines in as the iteration
advances, while old lines (already processed) get evicted via LRU.

### Inside the L1 — sets, ways, and why SoA avoids thrashing

The L1 data cache isn't a flat pool of 768 cache lines. It's organized as
**sets × ways**:

```
L1D Cache on this P-core (48 KiB):

  SET 0       SET 1       SET 2       ...       SET 63
  ┌─────┐    ┌─────┐    ┌─────┐                ┌─────┐
  │way 0│    │way 0│    │way 0│                │way 0│
  │way 1│    │way 1│    │way 1│                │way 1│
  │way 2│    │way 2│    │way 2│                │way 2│
  │ ... │    │ ... │    │ ... │                │ ... │
  │way11│    │way11│    │way11│                │way11│
  └─────┘    └─────┘    └─────┘                └─────┘
   ↑ 12 lines max per set                          ↑
     (evicts oldest if all 12 are occupied)

  Total: 64 sets × 12 ways = 768 cache lines
  Line size: 64 bytes → 48 KiB total capacity
```

**How an address maps to a set — the critical insight:**

Every memory address is split into three parts by the CPU:

```
64-bit virtual address (e.g., &position[42] = 0x00007FF8_A3C00150):

  ┌─────────────────────┬────────────┬──────────┬──────────┐
  │   tag (high bits)   │ set index  │   word   │   byte   │
  │   "who am I?"       │  "which    │  offset  │  offset  │
  │                     │  drawer?"  │  (6 bits)│ (unused) │
  └─────────────────────┴────────────┴──────────┴──────────┘
                               │
                               ▼
  set_index = (address / 64) % 64    ← divides by line size, then wraps
                                       around using modulo of set count
```

The **set index** determines which of the 64 sets the cache line goes into.
The **tag** identifies which specific memory region this line represents.
The CPU can check all 12 ways of a set in parallel (associative lookup).

**Why SoA naturally spreads across sets:**

```
&Position[0]  = 0x0000_0100  →  set = (0x100 / 64) % 64 = 4 % 64  = set 4
&Position[1]  = 0x0000_0108  →  set = (0x108 / 64) % 64 = 4 % 64  = set 4  (same line!)
&Position[8]  = 0x0000_0140  →  set = (0x140 / 64) % 64 = 5 % 64  = set 5
...
&Position[64] = 0x0000_0300  →  set = (0x300 / 64) % 64 = 12 % 64 = set 12

&Velocity[0]  = 0x0100_C000  →  set = (0x100C000/64) % 64 = ... = set 0
                                    ↑ completely different base address
                                      → different set from Position!
```

Position and Velocity live in **different memory allocations** (different
`Vec`s), so their base addresses differ by megabytes. The division by 64
and modulo 64 operation maps them to entirely different sets. They don't
compete for the same 12-way slots.

**What WOULD cause thrashing — the AoS nightmare:**

```
If we stored interleaved (AoS) — which we DON'T:

  struct Entity { pos: f32x2, vel: f32x2, health: f32 }
  // sizeof(Entity) = 20 bytes

  &entities[0] = 0x0000_0000  →  set 0
  &entities[1] = 0x0000_0014  →  set 0   (20 bytes later, modulo 64)
  &entities[2] = 0x0000_0028  →  set 0   (still set 0!)
  &entities[3] = 0x0000_003C  →  set 0   (STILL set 0 — 4 entities, same set!)
  &entities[4] = 0x0000_0050  →  set 1   (finally a different set)

  → 4 consecutive entities fight for the SAME 12-way set
  → every 4th entity maps to the same set as the 1st
  → with 6144 entities per slice, each set sees 6144/64 = 96 entities
    competing for just 12 slots → CONSTANT EVICTION
```

**The bottom line:**

SoA (separate arrays) = Position and Velocity map to **different sets** →
768 lines spread across 64 sets → no contention → 12-way per set is plenty.

AoS (interleaved) = Position and Velocity share **same address range** →
same sets fight each other → 12-way is a bottleneck → thrashing.

This is why cache-aware slice sizing works in practice even though total
data exceeds L1 capacity: the data is spread across independent cache
sets that don't interfere.

### So the 8-byte assumption is...

...a **throughput** estimate, not a working-set-size estimate. It says "we
touch ~8 bytes of component data per entity per iteration step." For a
2-component query you touch 16 bytes/entity, so the total bandwidth is 2×
higher — but the cache residency at any instant doesn't double, because
you're only ever "at" one entity index at a time.

Think of it like a conveyor belt, not a bathtub:
- **L1 capacity** limits how wide the belt can be (how many cache lines fit)
- **Throughput** limits how fast the belt moves (how many bytes/sec we stream)
- **Our slice size** sizes the belt for one pass — total data on the belt
  over time, not simultaneous data in the belt at one instant

If your query touches 3+ components per entity, the throughput pressure
increases and you may want a smaller slice. Use `with_batch_size()` to dial
it down. But for 1-2 components (the common case), the 8-byte assumption
holds up well in practice.

```
30000 entities across 3 archetypes, 6 KiB slices:

Archetype 0               Archetype 1            Archetype 2
(Position+Velocity, 18K)  (Health+Position, 8K)  (Transform, 4K)
════════════════════════   ════════════════════    ═══════════════
slice 0: [   0..6144]     slice 3: [0..6144]     slice 4: [0..4096]
slice 1: [6144..12288]    slice 2: [6144..8000]
slice 2: [12288..18000]
```

Key properties:
- Each slice is from **one archetype** — never straddles archetype boundaries.
- The last slice of each archetype may be smaller (leftover).
- All slices are stored flat in a single `Vec<(arch_index, start, end)>`.

---

## 3. WORK GROUPS — ASSIGNING SLICES TO RAYON TASKS

A **work group** is a contiguous run of slices assigned to a single
`rayon::scope` task. Multiple groups are spawned simultaneously so all threads
pull work at the same time (unlike `par_iter()` work-stealing, where later
threads arrive late).

```
8 slices → 4 work groups → 4 rayon tasks:

  iterator_slices: [s0][s1][s2][s3][s4][s5][s6][s7]
                    │    │    │    │    │    │    │
  work groups:     └─g0─┘   └─g1─┘   └─g2─┘   └─g3─┘
                      │        │        │        │
  rayon tasks:      task0    task1    task2    task3
                      │        │        │        │
  OS threads:       CPU0     CPU1     CPU2     CPU3
```

### How many work groups?

The number of groups is determined by the **splitting hint** (timing feedback):

```rust
// From iter.rs — compute group count from timing feedback
let num_groups = if hint_ns > 0 {
    // hint_ns = system's average execution time (EMA over ~32 frames)
    // TARGET_WORK_GROUP_DURATION = 50 µs
    // → "how many 50 µs chunks fit in the system's runtime?"
    let target = (hint_ns / TARGET_WORK_GROUP_DURATION)
        .clamp(1, num_threads);
    target.min(num_slices).max(1)
} else {
    // No timing data yet → one group per thread (up to slice count)
    num_threads.min(num_slices).max(1)
};
```

**For example:** if a system takes 200 µs on average, and the target is 50 µs
per group, we get 200/50 = 4 groups. With 20 logical threads, only 4 get used —
the rest stay idle because there isn't enough work to justify waking them.

---

## 4. THE SPLITTING HINT — TIMING FEEDBACK LOOP

The engine tracks an **exponential moving average** of each system's execution
time. This average feeds the group-count decision above, damping frame-to-frame
jitter.

```
Frame-by-frame timing:

  Frame N:   198 µs  →  EMA updates toward 198
  Frame N+1: 203 µs  →  EMA updates toward 203
  Frame N+2: 195 µs  →  EMA updates toward 195
  ...

  After ~32 frames:  EMA ≈ 200 µs
  → work groups = 200 / 50 = 4 groups
```

Key config:
- `SPLITTING_HINT_WINDOW = 32` — EMA smoothing window (1/32 per frame)
- `TARGET_WORK_GROUP_DURATION = 50_000 ns` — desired work per group
- The EMA excludes profiling overhead (timing starts *after* Tracy zone init)

---

## 5. HYPERTHREADING & L1 CONTENTION

Hyperthreading (SMT) runs **two logical threads per physical core**. Both share
the same L1D cache. This matters for slice sizing:

```
P-core with Hyperthreading (48 KiB L1D):

  ┌──────────────────────────────┐
  │  Logical Thread 0            │
  │  ┌────────┐                  │
  │  │ slice 0│  24 KiB working  │  ──┐  48 KiB L1D
  │  └────────┘                  │    │   (shared)
  │  Logical Thread 1            │    │
  │  ┌────────┐                  │  ──┘
  │  │ slice 1│  24 KiB working  │
  │  └────────┘                  │
  └──────────────────────────────┘
```

**Our half-fill formula** (`l1_bytes / 2 / component_size`) already accounts for
this — two hyperthreads running slices from the same L1 each get ~half the cache.

On hybrid architectures (P-cores + E-cores), the smallest L1D is used as the
baseline. On your i7-12700KF: P-cores have 48 KiB, E-cores have 32 KiB. The
current implementation uses the **first** L1D reported by CPUID, which is usually
the P-core's. A future improvement could detect per-core-type cache sizes.

---

## 6. THE PARALLEL THRESHOLD — WHEN TO STAY SEQUENTIAL

Not all workloads benefit from parallelism. Spawning rayon tasks has overhead
(thread wake-up, OS scheduling). For tiny workloads, a sequential loop is faster.

```rust
let threshold = num_threads * MINIMUM_SLICE_SIZE;  // 20 × 256 = 5120

if total_entities < threshold {
    // Sequential fallback — no rayon overhead
    for (_, q_state, f_state, len) in &self.archetype_ranges {
        for index in 0..*len {
            if filter_matches(f_state, index) {
                func(query_fetch(q_state, index));
            }
        }
    }
    return;
}
// ... parallel path ...
```

With `MINIMUM_SLICE_SIZE = 256` and 20 threads, workloads under 5120 entities
stay sequential — Rayon's ~10 µs task-spawn overhead would dominate a 50 µs
iteration anyway.

---

## 7. FULL DATA FLOW

```
Frame
 └─ Scheduler BATCH ("run systems batch 1/2")
     ├─ System: movement   ──┐
     ├─ System: health_decay ─┤ run concurrently (no data conflicts)
     └─ System: cleanup    ──┘
          │
          └─ par_iter_mut().for_each(|(pos, vel)| { ... })
               │
               ├─ 1. Check threshold: 30000 entities > 5120 → PARALLEL
               │
               ├─ 2. Build slices (size = L1D/8, computed at startup)
               │      slice 0: archetype 0, entities 0..6144
               │      slice 1: archetype 0, entities 6144..12288
               │      slice 2: archetype 0, entities 12288..18000
               │      slice 3: archetype 1, entities 0..6144
               │      slice 4: archetype 1, entities 6144..8000
               │      slice 5: archetype 2, entities 0..4096
               │
               ├─ 3. Compute work group count from EMA hint
               │      hint_ns = 200000  (system avg from timing feedback)
               │      groups  = 200000 / 50000 = 4
               │
               ├─ 4. Pack slices into 4 work groups
               │      group 0: slices [0, 1]     → 2 slices, 12288 entities
               │      group 1: slices [2, 3]     → 2 slices, 11856 entities
               │      group 2: slices [4]        → 1 slice,  1856 entities
               │      group 3: slices [5]        → 1 slice,  4096 entities
               │
               └─ 5. Spawn all groups via rayon::scope
                      │         │         │         │
                   rayon      rayon     rayon     rayon
                   task 0     task 1    task 2    task 3
                   (any       (any      (any      (any
                   free       free      free      free
                   OS         OS        OS        OS
                   thread)    thread)   thread)   thread)
```

### ⚠️ Rayon tasks ≠ CPU cores

**This is the most misunderstood part.** Rayon does NOT pin tasks to specific
cores. When we spawn 4 rayon tasks, the OS thread scheduler decides which
physical core runs each one. The mapping might be:

```
Frame N:                          Frame N+1 (completely different):
  task 0 → OS thread 3 → P-core 1   task 0 → OS thread 7 → P-core 3
  task 1 → OS thread 0 → P-core 0   task 1 → OS thread 2 → E-core 1  ← surprise!
  task 2 → OS thread 5 → P-core 2   task 2 → OS thread 0 → P-core 0
  task 3 → OS thread 1 → E-core 0   task 3 → OS thread 4 → P-core 2
```

**The OS scheduler does whatever it wants.** It may migrate a task mid-execution
to a different core. It may place a task on an E-core with a 32K L1 instead of
a P-core with a 48K L1. It may schedule two tasks as hyperthreads on the same
physical core.

### What "Core 0 gets two slices" actually means

A work group is just a **contiguous range of indices** into a flat Vec of slices.
It says "process `slices[0]` and `slices[1]` back-to-back." It does NOT say
"run this on Core 0." Whichever OS thread picks up that rayon task will
process those slices on whichever core the OS placed it.

```
One work group (task) executing:

  Task grabs group: "I own slices[0] and slices[1]"
  → for slice 0:     iterate archetype 0, entities 0..6144
  → for slice 1:     iterate archetype 0, entities 6144..12288
  → done. Task returns to rayon pool.

All work happens on ONE core (whichever one the OS chose).
That core's L1 sees the entire 12288-entity working set,
but only one slice at a time — by the time slice 1 starts,
slice 0's data has already been evicted (use-once pattern).

### A group with 2 slices, step by step

```
Group 0: slices [0, 1] → 12288 entities total → ONE rayon task → ONE core

Time ──────────────────────────────────────────────────────────────►

  ┌─────────────────────────┐    ┌─────────────────────────┐
  │ PROCESSING SLICE 0       │    │ PROCESSING SLICE 1       │
  │ entities 0..6144         │    │ entities 6144..12288      │
  │                          │    │                          │
  │ L1 contents:             │    │ L1 contents:             │
  │ ┌──────────────────┐    │    │ ┌──────────────────┐    │
  │ │ pos[0..6144]      │    │    │ │ pos[6144..12288]  │    │
  │ │ vel[0..6144]      │    │    │ │ vel[6144..12288]  │    │
  │ │ ≈ 98 KiB          │    │    │ │ ≈ 98 KiB          │    │
  │ │                    │    │    │ │                    │    │
  │ └──────────────────┘    │    │ └──────────────────┘    │
  │  48 KiB L1D ✓           │    │  48 KiB L1D ✓           │
  │  (fits, evicts old       │    │  (slice 0 data already  │
  │   lines via LRU)         │    │   evicted — we never    │
  │                          │    │   revisit it!)          │
  └─────────────────────────┘    └─────────────────────────┘

  L1 NEVER holds both slices simultaneously.
  The working set at any instant = 1 slice, not the whole group.
```

The group just says "do A, then B, on the same thread" — it avoids the
overhead of spawning a second rayon task for slice 1. But cache-wise,
slice 1 is a clean slate: all of slice 0's cache lines have been marked
LRU and evicted by the time slice 1's data starts streaming in.

### Why not make every group exactly 1 slice?

If we gave each slice its own rayon task, we'd have 6 tasks instead of 4.
That means more `rayon::scope` spawn overhead, more OS context switches,
and more idle threads at the end waiting for stragglers. Packing 2 small
slices into 1 group is **cheaper than spawning a new task** when the
per-slice work is small relative to spawn overhead.

The rule of thumb: spawn overhead ≈ 10 µs. If a slice takes 25 µs,
packing two into one group (50 µs) is worth it. If a slice takes 200 µs,
give it its own group — the overhead is negligible.
```

### Why we don't pin to cores (and why it's OK)

We could use `core_affinity` to pin tasks, but we don't, because:

- **OS schedulers are smart.** They know which cores are idle, thermal-throttled,
  or have warm caches from previous work.
- **Our access pattern is use-once.** Since we don't revisit data, having a
  "warm" cache from a previous frame on the same core doesn't help much.
- **Slice size already handles the worst case.** A 6K-entity slice fits even in
  a small 32K E-core L1. If the OS puts a task on an E-core, it still runs
  correctly — just slightly slower than on a P-core.
- **Pinning creates worse problems.** If we pin 4 tasks to specific cores but
  another process is using one of those cores, our task waits while idle cores
  sit unused.

### The actual guarantee

The only guarantee is: **one work group = one OS thread = one CPU core at a time.**
Slices within a group run sequentially on whatever core the OS chose. Different
groups MAY run on different cores (that's the whole point of parallelism), but
they MAY also end up hyperthread-siblings sharing L1. The half-fill slice size
handles both cases.

---

## 8. CONFIGURATION REFERENCE

All values live in `crate::config::ParallelProcessingConfig` and are printed
at `Engine::new()`:

```
Parallel execution config
├─ Rayon threads: 20                       # OS thread pool size
├─ Target work-group duration: 50 µs       # TARGET_WORK_GROUP_DURATION
├─ Splitting-hint averaging window: 32     # SPLITTING_HINT_WINDOW
├─ Default entities per slice: 6144        # computed from L1D at startup
└─ Minimum slice size: 256                 # MINIMUM_SLICE_SIZE
```

| Knob | Default | What it does |
|---|---|---|
| `default_entities_per_slice()` | auto | Entities per slice (from L1D ÷ component_size) |
| `TARGET_WORK_GROUP_DURATION` | 50 µs | Target work per rayon task |
| `SPLITTING_HINT_WINDOW` | 32 frames | EMA smoothing for timing feedback |
| `MINIMUM_SLICE_SIZE` | 256 | Threshold per thread before sequential fallback |

Overrides:
- `par_iter_mut().with_batch_size(N)` — forces a specific slice size
- The const `DEFAULT_ITERATOR_SLICE_SIZE = 4096` is a **fallback** only (non-x86
  or CPUID failure). The runtime value from L1 detection takes priority.

---

## 9. KEY TRADE-OFFS

| Decision | Pro | Con |
|---|---|---|
| Half-fill L1 (not full) | Room for filter state, adjacent cache lines | Under-utilizes L1 on single-thread workloads |
| Work groups (not work-stealing) | All threads start simultaneously | Uneven work distribution if slices vary wildly |
| EMA timing (not raw per-frame) | Damped, stable group counts | Slower to react to phase changes |
| Sequential fallback under threshold | No rayon overhead for tiny workloads | Abrupt transition at boundary |

---

## 10. WORKED EXAMPLE — A COMPLETE PASS, BY THE NUMBERS

Let's trace what actually happens when the engine processes one system on
one frame, with real numbers from the i7-12700KF startup banner.

### Setup

```
System:     movement_system — par_iter_mut over (&Position, &Velocity)
Entities:   30 000, split across 2 archetypes:
              Archetype 0: [Position, Velocity, Health] — 20 000 entities
              Archetype 1: [Position, Velocity]         — 10 000 entities

Hardware:   i7-12700KF, 20 logical threads, 48 KiB L1D
Detected:   L1D = 48 KiB → slice size = 48 152 / 8 = 6144 entities
Splitting:  hint_ns = 180 000 (system avg from prior frames)
            target   = 180 000 / 50 000 = 3 groups
```

### Step 1 — Threshold check

```
total_entities = 30 000
threshold = 20 threads × 256 = 5120
30 000 > 5120 → PARALLEL path
```

### Step 2 — Build slices

```
Archetype 0 (20K entities):        Archetype 1 (10K entities):
  slice 0: arch 0, [0..6144]         slice 3: arch 1, [0..6144]
  slice 1: arch 0, [6144..12288]     slice 4: arch 1, [6144..10000]
  slice 2: arch 0, [12288..18000]
  (leftover) slice tail: [18000..20000]  → also slice 5

Total: 6 slices
```

### Step 3 — Compute work groups

```
hint_ns = 180 000 ns (EMA over ~32 frames)
groups = 180 000 / 50 000 = 3 (clamped to thread count)
```

### Step 4 — Pack slices into 3 groups

```
6 slices ÷ 3 groups = 2 per group (balanced):

  group 0: slices [0, 1] → 12 288 entities → task A
  group 1: slices [2, 3] → 11 856 entities → task B
  group 2: slices [4, 5] →  5 952 entities → task C
```

### Step 5 — Execute on whatever cores the OS chooses

```
Task A lands on OS thread 3 → P-core 1 (48 KiB L1D)
Task B lands on OS thread 7 → P-core 3 (48 KiB L1D)
Task C lands on OS thread 1 → E-core 0 (32 KiB L1D)  ← smaller L1, still fine
```

### Cache-line accounting for Task A (2 slices = 12 288 entities)

Task A processes slice 0, then slice 1, sequentially on P-core 1.
Only ONE slice is in L1 at any instant.

```
SLICE 0 (entities 0..6144):
  Query touches: &Position (8 B) + &Velocity (8 B) = 16 B per entity
  Total data:    6144 × 16 B = 98 304 bytes = 96 KiB
  Cache lines:   Position: 6144 / 8 per line  = 768 lines (48 KiB)
                 Velocity: 6144 / 8 per line  = 768 lines (48 KiB)

  BUT: L1 only sees the CURRENT ITERATION WINDOW + PREFETCH LOOKAHEAD:

  ┌──────────────────────────────────────────────────────┐
  │  Position lines live in L1 right now:                │
  │    pos[40..47]  →  set 4, way 3   (1 line)          │
  │    pos[48..55]  →  set 5, way 7   (1 line)          │
  │    pos[56..63]  →  set 6, way 2   (1 line)          │
  │    pos[64..71]  →  set 7, way 0   (prefetched)      │
  │    pos[72..79]  →  set 8, way 5   (prefetched)      │
  │                                                      │
  │  Velocity lines live in L1 right now:                 │
  │    vel[40..47]  →  set 20, way 1  (1 line)           │
  │    vel[48..55]  →  set 21, way 9  (1 line)           │
  │    vel[56..63]  →  set 22, way 4  (1 line)           │
  │    vel[64..71]  →  set 23, way 2  (prefetched)      │
  │    vel[72..79]  →  set 24, way 8  (prefetched)      │
  │                                                      │
  │  Total LIVE lines: ~10 lines (~640 bytes)            │
  │  Position and Velocity → DIFFERENT sets → no conflict│
  │  Old lines (pos[0..39], vel[0..39]) → already LRU,  │
  │    evicted by newer data                              │
  └──────────────────────────────────────────────────────┘

  768 cache lines available, only ~10 live at any instant.
  The other 758 lines are either dead (LRU, about to be evicted)
  or holding prefetched data for the next few hundred entities.

SLICE 1 (entities 6144..12288):
  Same pattern, different addresses.
  Slice 0's data is completely gone from L1 — we never revisit it.
  Fresh cache lines stream in for pos[6144..] and vel[6144..].
```

### Prefetching in action

```
While CPU executes:                Prefetcher simultaneously:
──────────────────────────────────────────────────────────────
reads pos[6144] → L1 hit          pulling pos[6208..6271] into L1
reads vel[6144] → L1 hit          pulling vel[6208..6271] into L1
reads pos[6145] → L1 hit          (no action, already in same line)
reads vel[6145] → L1 hit          (no action, already in same line)
...                               ...
reads pos[6152] → L1 hit          pulling pos[6272..6335] into L1
  (prefetcher stays 8-16 lines ahead, hiding L2 latency)
```

### Why 3 groups, not 20?

```
20 threads available, but only 3 groups spawned.
Why? The system takes ~180 µs. At 50 µs target per group:

  180 / 50 = 3 groups

Spawning 20 groups would mean each group gets ~9 µs of work.
At 10 µs of spawn overhead per task, you'd spend MORE time
spawning than computing. 3 groups × ~60 µs each = efficient.

The remaining 17 OS threads sit idle — and that's correct.
Using them would create more overhead than value.
```

### The net result

```
6 slices → 3 work groups → 3 rayon tasks → 3 CPU cores
Each core processes 2 slices sequentially (~60 µs per core)
Total wall time: ~60 µs (parallel) vs ~180 µs (sequential)
Speedup: ~3× (limited by work granularity, not thread count)

Cache misses: <5% L1 miss rate (prefetcher hides L2 latency)
Effective bandwidth: ~10 GB/s streaming through L1
```

| What | Value | Why |
|---|---|---|
| Slice size | 6144 entities | L1D size ÷ 8 B assumption |
| Groups | 3 | 180 µs system ÷ 50 µs target |
| Slices per group | 2 | balanced distribution of 6 slices |
| L1 lines live at instant | ~10 | streaming + prefetch + SoA = low residency |
| L1 miss rate (est.) | ~5% | prefetcher hides most misses |
| Speedup vs sequential | ~3× | limited by work size, not cores |
