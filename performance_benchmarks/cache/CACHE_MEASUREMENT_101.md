# Cache Measurement 101 - A Developer Guide to CPU Cache Behavior in Rust

**Audience:** Rust developers who want to understand how CPU caches work, how
Rust code interacts with them, and how to measure cache behavior correctly.

**Scope:** This guide is repository-agnostic. Chapter 12 contains observations
specific to the `ecs_hybrid` crate (`d:\Programming\Rust-Hybrid-ECS`). All
other chapters apply to any Rust project.

---

## Table of Contents

1. [CPU Cache Fundamentals](#1-cpu-cache-fundamentals)
2. [Cache Terminology and Metrics](#2-cache-terminology-and-metrics)
3. [How Rust Code Influences Cache Behavior](#3-how-rust-code-influences-cache-behavior)
4. [Rust Iterators and Cache Behavior](#4-rust-iterators-and-cache-behavior)
5. [Common Cache Offenders](#5-common-cache-offenders)
6. [Measuring Cache Behavior](#6-measuring-cache-behavior)
7. [Analysis Tools](#7-analysis-tools)
8. [Measuring Iterator Pipelines](#8-measuring-iterator-pipelines)
9. [Experimental Methodology](#9-experimental-methodology)
10. [Interpreting Results](#10-interpreting-results)
11. [Cache Optimization Techniques](#11-cache-optimization-techniques)
12. [Repository-Specific Observations](#12-repository-specific-observations)
13. [Practical Checklists](#13-practical-checklists)

---

## 1. CPU Cache Fundamentals

### 1.1 Why CPUs use caches

Modern CPUs execute instructions at a rate far exceeding DRAM's ability to
deliver data. A single main-memory access can cost 100–300 cycles - during
which a superscalar core could have retired hundreds of instructions. Caches
are small, fast SRAM buffers placed between the execution units and main
memory. They exploit two empirical properties of real programs:

- **Temporal locality:** A memory location that was accessed recently is
  likely to be accessed again soon.
- **Spatial locality:** Memory locations near a recently accessed location
  are likely to be accessed soon.

Without caches, every load and store would stall the pipeline waiting for
DRAM, and CPU performance would collapse to roughly 1–5% of peak.

### 1.2 Cache lines

Caches do not operate on individual bytes. They fetch and store data in
fixed-size blocks called **cache lines** - typically 64 bytes on x86-64 and
most ARMv8+ processors (Apple M-series uses 128-byte lines; some embedded
CPUs use 32-byte lines).

When a program reads a single `u32` (4 bytes), the CPU loads the surrounding
60 bytes into cache as a side effect. This is the mechanism behind spatial
locality: iterating a `Vec<u32>` sequentially will hit the cache on 15 out
of every 16 accesses after the initial miss.

**Key implication:** A structure that spans two cache lines pays two misses
on first access. Aligning hot structures to cache-line boundaries (64 bytes
on x86-64) and keeping frequently accessed fields together within the same
cache line can reduce miss counts.

### 1.3 Spatial and temporal locality

| Property | Example | Cache benefit |
|---|---|---|
| **Spatial locality** | `for x in &vec { … }` | Sequential access loads entire cache lines; 15/16 hits after first miss |
| **Temporal locality** | Re-reading a config struct inside a hot loop | First access brings it into cache; subsequent accesses hit L1 |
| **No locality** | Walking a randomly ordered linked list | Every node may require a new cache-line fill; near-100% miss rate |

### 1.4 The cache hierarchy

Modern CPUs have three (sometimes four) levels of cache:

| Level | Typical size | Typical latency | Shared? |
|---|---|---|---|
| **L1d** (data) | 32 KiB per core | 4–5 cycles | Private to core |
| **L1i** (instruction) | 32–64 KiB per core | 4–5 cycles | Private to core |
| **L2** | 256–512 KiB per core (x86) or per cluster (Apple) | 10–14 cycles | Usually private |
| **L3 / LLC** | 8–128 MiB per chip/socket | 30–60 cycles | Shared across all cores |
| **DRAM** | Gigabytes | 100–300 cycles | Shared across all cores |

The **L1 data cache** is the smallest and fastest. It is virtually indexed
and physically tagged (VIPT) on most modern designs, meaning address
translation and cache lookup happen in parallel.

The **L1 instruction cache** feeds the front-end decoder. Code that exceeds
L1i capacity (e.g. enormous monomorphized generic functions, very large
match expressions) causes front-end stalls even if the data cache is fine.

The **last-level cache (LLC)** is typically L3. An LLC miss means the line
must be fetched from DRAM - the most expensive miss in the hierarchy.

### 1.5 Inclusive, exclusive, and non-inclusive caches

- **Inclusive:** L3 contains a superset of all lines in L1 and L2. Evicting
  a line from L3 requires back-invalidation of L1/L2 copies. Intel's server
  chips (Xeon) often use inclusive L3. Advantage: simpler coherence snooping.
  Disadvantage: L3 capacity wasted duplicating L1/L2 data.

- **Exclusive:** A line resides in exactly one cache level. When a line is
  promoted to L1, it is removed from L2. AMD Zen uses exclusive L2/L3.
  Advantage: effective capacity is sum of all levels. Disadvantage: an L1
  miss that hits L2 requires moving the line from L2 to L1.

- **Non-inclusive:** A line may or may not be present in multiple levels.
  Most modern designs (Intel client since Skylake, ARM Cortex-A7xx) use
  non-inclusive caches. They are simpler to implement and avoid the capacity
  waste of inclusive caches.

**Practical impact for developers:** You cannot simply add cache sizes to
compute "total effective cache." An exclusive hierarchy gives more effective
capacity for a given silicon budget. An inclusive hierarchy means L3
evictions harm L1/L2 hit rates.

### 1.6 Private versus shared caches

L1 and L2 are private to each core (or cluster, on Apple M-series). L3 is
shared. The shared LLC means that one core's memory-intensive work can
pollute the cache for all other cores. Conversely, two cores reading the
same shared data can each find it in L3 without going to DRAM.

### 1.7 Cache sets, ways, and associativity

A cache is organized as **S sets × W ways**. A memory address maps to
exactly one set (determined by bits of the physical address), but can be
stored in any of the W ways within that set.

- **Direct-mapped** (W = 1): Each address maps to exactly one slot. Simple
  and fast, but suffers from conflict misses when two frequently accessed
  addresses hash to the same set.

- **Fully associative** (S = 1): Any address can occupy any slot. No
  conflict misses, but prohibitively expensive to implement for more than a
  few dozen entries (used only for TLBs and small specialized buffers).

- **Set-associative** (typical L1d: 8-way; L2: 4–16-way; L3: 12–20-way):
  Compromise between flexibility and hardware cost. Most general-purpose
  CPUs use this design.

**Why associativity matters to developers:** If your hot working set
accesses more distinct addresses than W within the same set, you will see
conflict misses even though the cache as a whole has spare capacity. This
most commonly happens with power-of-two strides on large arrays (e.g.
accessing every 4096th byte on a 32 KiB 8-way cache with 64 sets).

### 1.8 Cache indexing and tags

A physical address is decomposed into:

```
|     tag      |  set index  |  block offset  |
```

- **Block offset:** Selects the byte within a cache line (bits 0–5 for
  64-byte lines).

- **Set index:** Selects the set (bits 6–N, where N depends on cache size
  and associativity).

- **Tag:** Stored alongside each cache line; compared on lookup to confirm
  the line contains the requested address.

The cache is physically tagged on all x86-64 and modern ARM cores, meaning
the TLB lookup (virtual → physical) must complete before the tag comparison.
Virtually indexed, physically tagged (VIPT) L1 caches exploit the fact that
the page offset bits are identical in virtual and physical addresses,
allowing the set-index decode to proceed in parallel with TLB lookup.

### 1.9 Cache-line fills and evictions

When a cache miss occurs:

1. The cache controller selects a victim line in the target set (typically
   via pseudo-LRU or a variant).

2. If the victim is **dirty** (modified), its contents are written back to
   the next cache level or to DRAM.

3. The requested line is fetched from the next level or DRAM.

4. For a write that misses (store miss), the behavior depends on the
   write-allocate policy (see §1.11).

Eviction policies are microarchitecture-specific. Pseudo-LRU (approximating
true LRU with fewer state bits) is common; some designs use RRIP
(Re-Reference Interval Prediction) or adaptive policies. Application
developers cannot control eviction, but they can design data structures and
access patterns that keep the working set within cache capacity.

### 1.10 Clean and dirty cache lines

- **Clean:** The cache line matches the copy in the next memory level.
  Can be silently dropped on eviction.

- **Dirty:** The cache line has been modified and must be written back on
  eviction. A dirty eviction costs a write to the next level.

Stores to read-only shared data turn clean cache lines dirty in the writing
core's private cache, triggering a coherence state transition (see §1.13).

### 1.11 Write-through versus write-back caches

- **Write-through:** Every store is written to both the cache and the next
  memory level simultaneously. Simple but bandwidth-intensive. Rarely used
  in modern CPU data caches (sometimes used for L1i on architectures that
  support self-modifying code).

- **Write-back:** Stores only update the cache line. The line is marked
  dirty. Write-back to the next level happens only on eviction. Almost all
  modern CPU data caches are write-back.

**Write-allocate versus no-write-allocate:** On a store miss, a
write-allocate cache fetches the line into cache first, then modifies it.
A no-write-allocate cache writes directly to the next level without
fetching the line. x86-64 data caches are write-allocate; some ARM designs
offer configurable behavior.

### 1.12 Hardware prefetching

Modern CPUs contain hardware prefetchers that detect access patterns and
speculatively fetch cache lines before the program requests them. Common
prefetcher types:

- **Next-line / adjacent-line prefetcher:** On access to line N, also fetch
  N+1 (or N+2). Effective for sequential access.

- **Stride prefetcher:** Detects regular strides (e.g. accessing every 256th
  byte) and prefetches ahead along that stride.

- **Spatial prefetcher:** On a miss to line N, prefetch nearby lines within
  the same physical page.

- **Stream prefetcher:** Tracks multiple independent sequential streams.

**Limitations:** Prefetchers can be confused by irregular patterns, pointer
chasing, or very large strides. They can also cause *cache pollution* by
fetching lines that are never used. Prefetching across page boundaries may
be suppressed because the physical next-page may not be contiguous.

**Practical note:** Sequential access to dense arrays benefits immensely
from hardware prefetching. Random access defeats it. Strided access may
work if the stride is regular and not too large.

### 1.13 Cache coherence

In multi-core systems, each core has private L1/L2 caches. The **cache
coherence protocol** (typically MESI, MOESI, or MESIF on x86-64) ensures
that all cores see a consistent view of memory.

Key states (MESI):

| State | Meaning |
|---|---|
| **M**odified | Only this cache has the line; it is dirty. Must write back on eviction. |
| **E**xclusive | Only this cache has the line; it is clean. Can silently drop. |
| **S**hared | Multiple caches may have the line; all copies are clean. |
| **I**nvalid | The line is not present in this cache. |

A read to a line in Modified state in another core triggers a **cache-to-
cache transfer**: the owning core sends the data directly rather than
going through DRAM. On modern Intel CPUs (since Nehalem), cache-to-cache
transfers via the shared L3 ring/mesh are faster than DRAM but still cost
tens of cycles.

### 1.14 False sharing

False sharing occurs when two cores write to different variables that
happen to reside on the same cache line. The coherence protocol sees the
line bouncing between cores, causing invalidations and cache misses, even
though the cores never access each other's data.

```rust
// BAD: False sharing
#[repr(C)]
struct Counters {
    counter_a: AtomicU64,  // Written by thread 1
    counter_b: AtomicU64,  // Written by thread 2 - same cache line!
}

// BETTER: Pad to cache-line boundary
#[repr(C, align(64))]
struct PaddedCounter {
    value: AtomicU64,
}
#[repr(C)]
struct Counters {
    counter_a: PaddedCounter,  // Own cache line
    counter_b: PaddedCounter,  // Own cache line
}
```

On x86-64, `#[repr(align(64))]` or explicit `[u8; 56]` padding fields are
the standard approaches. The `crossbeam` and `std::sync::atomic` types are
typically 8 bytes on 64-bit platforms; two adjacent atomics fit within the
same 64-byte line and will false-share if written from different threads.

### 1.15 Store buffers and load/store queues

Modern out-of-order CPUs do not perform stores directly to the cache.
Instead, stores are written to a **store buffer** - a small FIFO queue (e.g.
56 entries on Intel Golden Cove) that holds pending stores until they can
be committed to the cache. This allows the CPU to continue executing past
a store without waiting for cache access.

Loads consult the store buffer before the cache (store-to-load forwarding)
so that a load of a recently stored value can be satisfied without waiting
for the store to commit.

**Practical impact:** A sequence of stores followed by loads to the same
addresses is fast (forwarding). Scattered stores interleaved with loads
to unrelated addresses may fill the store buffer and stall the pipeline.

### 1.16 Translation lookaside buffers (TLBs)

Every memory access requires translating a virtual address to a physical
address. The **TLB** is a small, highly associative cache for page-table
entries. A TLB miss requires a **page walk** - reading multiple levels of
the page table (up to 4 levels on x86-64 with 4 KiB pages), each of which
is a memory access.

Typical TLB sizes:

| Level | x86-64 (Intel) | Apple M1/M2 |
|---|---|---|
| L1 D-TLB | 64–96 entries (4 KiB pages) | ~128 entries (16 KiB pages) |
| L1 I-TLB | 64–128 entries | ~96 entries |
| L2 unified TLB | 1024–2048 entries | ~3072 entries (16 KiB pages, shared) |

**Huge pages** (2 MiB or 1 GiB on x86-64) map much larger regions with a
single TLB entry. Applications that access large, contiguous buffers can
benefit from transparent huge pages (Linux) or explicit huge-page
allocation. The Rust standard library does not expose huge-page control
directly; the `memmap2` or `libc` crates provide access.

A program that randomly accesses a buffer larger than `TLB entries × page
size` will incur frequent TLB misses and page walks, adding tens of cycles
per access even if the data is in L1 cache.

### 1.17 Page walks and page faults

A **page walk** occurs on a TLB miss and is handled by the hardware page-
table walker on x86-64. It reads up to 4 levels of page tables from the
data cache (or DRAM if the page-table entries are not cached).

A **page fault** occurs when the page-table entry is not present (the OS
has not mapped the page, or it has been swapped out) or when access
permissions are violated. Page faults trap to the OS kernel and cost
thousands to tens of thousands of cycles - orders of magnitude worse than
a cache miss.

### 1.18 NUMA and remote-memory access

On multi-socket systems, each CPU socket has its own memory controller and
local DRAM. A core on socket 0 accessing memory attached to socket 1 pays
a **NUMA penalty** - typically 1.3×–2× higher latency and reduced
bandwidth.

Most single-socket systems (including consumer desktops and laptops) are
UMA (uniform memory access) and do not have NUMA concerns. Threadripper,
Epyc, Xeon Scalable, and multi-socket servers are NUMA.

Rust does not have built-in NUMA awareness. On NUMA systems, the OS
typically allocates memory on the node of the first-touching thread. For
applications that care about NUMA, the `hwloc` crate or `libc` bindings
provide allocation control.

### 1.19 Latency versus bandwidth

- **Latency:** The time to complete a single access (cycles or nanoseconds).
  L1d: ~4 cycles. L2: ~12 cycles. L3: ~40 cycles. DRAM: ~100–300 cycles.

- **Bandwidth:** The rate at which data can be transferred (bytes per second).
  Modern dual-channel DDR5: ~50–100 GiB/s. L1 cache: ~1–2 TiB/s per core.

Cache misses hurt primarily through **latency**. A single load that misses
all caches stalls dependent instructions. However, modern CPUs can hide
some of this latency via **memory-level parallelism (MLP)** - having
multiple outstanding cache misses in flight simultaneously. A linked-list
walk has poor MLP because each node's address depends on the previous node's
load result. Sequential array iteration has good MLP because the prefetcher
can issue multiple concurrent line fills.

---

## 2. Cache Terminology and Metrics

### 2.1 Core definitions

| Term | Definition |
|---|---|
| **Cache access** | A load or store that is presented to a cache level. |
| **Cache hit** | The requested line was found at that level. |
| **Cache miss** | The requested line was not found; must be fetched from a higher level. |
| **Hit rate** | `hits / accesses` (fraction of accesses satisfied at this level). |
| **Miss rate** | `misses / accesses` = `1 − hit rate`. |
| **Cache references** | Number of accesses to this cache level (hardware counter). |
| **LLC miss** | A miss in the last-level cache - the line must come from DRAM. |
| **Compulsory miss** | First access to a cache line; unavoidable without prefetching. |
| **Capacity miss** | The working set exceeds the cache size; a previously cached line is evicted and later re-accessed. |
| **Conflict miss** | The set is full due to associativity limits; a line is evicted despite spare capacity in other sets. |
| **Cache-to-cache transfer** | A line is fetched from another core's private cache rather than from DRAM (coherence intervention). |
| **TLB miss** | The virtual-to-physical translation is not in the TLB; a page walk is needed. |
| **IPC** | Instructions retired per cycle. Low IPC (< 1) often (but not always) indicates memory stalls. |
| **Stalled cycles** | Cycles where no instruction is retired. Includes memory stalls and front-end stalls. |

### 2.2 Why a cache miss at one level does not equal a DRAM access

- An L1d miss may hit L2 (no DRAM access).
- An L2 miss may hit L3 (no DRAM access).
- An L3 miss may be satisfied by a cache-to-cache transfer from another
  core's L2 (faster than DRAM, but still a few tens of cycles).
- Intel's `MEM_LOAD_RETIRED.L3_MISS` counts loads that miss L3. Even then,
  some may be satisfied by a snoop hit in another core's cache.

### 2.3 Derived metrics

```
miss_rate       = misses / accesses
misses_per_item  = misses / items_processed
cycles_per_item  = cycles / items_processed
instructions_per_item = instructions / items_processed
bandwidth       = bytes_processed / elapsed_time
```

**Limitations:**

- **Miss rate:** 5% miss rate with 1 million accesses = 50,000 misses
  (problematic). 5% miss rate with 100 accesses = 5 misses (irrelevant).
  Always consider absolute miss count alongside rate.

- **Misses per item:** Useful for comparing different algorithms processing
  the same data. Does not account for varying per-item costs (e.g. a
  "heavy" item may be worth more misses).

- **Cycles per item:** Confounds memory stalls, compute, and branch
  mispredictions. A high cycles-per-item may be caused by branch
  mispredictions, not cache misses.

- **Instructions per item:** Not a cache metric per se, but unexpected
  instruction counts can signal missed optimization opportunities (e.g.
  bounds checks not eliminated, iterator adapters not fused).

- **Bandwidth:** Averages hide burst behavior. High average bandwidth with
  low per-access latency suggests good prefetching and MLP. Low bandwidth
  with high latency suggests pointer chasing.

### 2.4 What "memory-level parallelism" means

MLP is the number of outstanding cache misses that can be in flight
simultaneously. An out-of-order core can continue executing independent
instructions while waiting for a miss. If the program's data-access
pattern permits overlapping misses, the effective latency can approach
`DRAM_latency / MLP`. Linked data structures (boxed trees, graphs,
linked lists) typically have MLP ≈ 1 because each node's address is
dynamically determined by the previous node's content. Contiguous arrays
achieve high MLP because the hardware prefetcher issues multiple
independent line fills ahead of the current access.

---

## 3. How Rust Code Influences Cache Behavior

### 3.1 Sequential versus random access

| Pattern | Example | Cache behavior |
|---|---|---|
| **Sequential** | `for x in slice { … }` | Near-perfect L1 hit rate; prefetcher keeps ahead. |
| **Strided** | `for i in (0..n).step_by(16) { v[i] }` | Stride ≤ cache line: good. Large power-of-two stride: conflict misses likely. |
| **Random** | Indexed by shuffled permutation | Miss rate ≈ 1; each access likely requires a DRAM fetch. |

### 3.2 Contiguous storage versus pointer chasing

```rust
// Cache-friendly: contiguous, predictable
let positions: Vec<[f32; 3]> = vec![...];
for pos in &positions {
    // Sequential access; 16 elements per cache line (64 B / 4 B)
    process(pos);
}

// Cache-unfriendly: pointer chasing
struct Node {
    position: [f32; 3],
    next: Option<Box<Node>>,
}
let mut current = &head;
while let Some(node) = current {
    process(&node.position);
    current = &node.next; // Each iteration dereferences a new heap address
}
```

The `Vec` version achieves high MLP and benefits from prefetching. The
linked-list version serializes on each pointer dereference (MLP ≈ 1) and
suffers a cache miss at every node unless the allocator happened to place
nodes contiguously (unlikely with a general-purpose allocator).

### 3.3 `Vec` versus `LinkedList`

`Vec<T>` stores elements contiguously. Iteration is sequential and
cache-friendly. Random access is O(1) and predictable. Cost: occasional
reallocation and copying.

`std::collections::LinkedList<T>` stores each element in a separate
heap-allocated node with two pointers (prev, next). Iteration chases
pointers. Memory overhead is 16 bytes per element on 64-bit (two `*mut
Node<T>`). There is virtually no scenario where `LinkedList` outperforms
`Vec` on modern hardware for iteration-heavy workloads. Its strengths are
O(1) push/pop at both ends and O(1) splitting/merging - operations that do
not require traversing element data.

### 3.4 Hash maps and tree-based maps

`HashMap<K, V>` (hashbrown-based, in std) stores entries in a contiguous
array, probed linearly. Cache behavior:

- **Keys/values are contiguous**, so iteration over all entries is
  cache-friendly.
- **Lookup** hashes the key, then probes the table. The probe sequence may
  touch multiple cache lines if the load factor is high or if hash
  collisions cause long probe chains.
- **Swisstable design** (hashbrown) uses SIMD to compare 16 bytes of
  metadata at once, touching fewer cache lines than traditional open
  addressing.

`BTreeMap<K, V>` stores entries in a B-tree (nodes contain multiple keys
values). Each node is a contiguous chunk (good), but traversal jumps
between nodes (pointer chasing). B-tree has better cache behavior than a
binary search tree but worse than a dense `Vec<(K, V)>` for ordered
iteration.

### 3.5 Array of Structures versus Structure of Arrays

```rust
// AoS: one Vec of structs
struct ParticleAoS {
    position: [f32; 3],
    velocity: [f32; 3],
    mass: f32,
}
let particles: Vec<ParticleAoS> = ...;

// SoA: separate Vecs per field
struct ParticlesSoA {
    positions: Vec<[f32; 3]>,
    velocities: Vec<[f32; 3]>,
    masses: Vec<f32>,
}
```

**AoS** is intuitive and keeps related fields together. If you always access
all fields of each particle, AoS is fine - you load one cache line that
contains position, velocity, and mass for 1–2 particles (28 bytes each;
~2 per 64-byte line).

**SoA** separates fields. If a system only processes positions, it streams
through a dense `Vec<[f32; 3]>` without loading unused velocity and mass
data into cache. SoA is the standard layout in high-performance ECS
frameworks for this reason.

**Trade-off:** SoA makes it harder to access all fields of a single entity
(requires indexing multiple Vecs). AoS makes it easier but wastes cache
space when only a subset of fields is needed. The choice depends on access
patterns.

### 3.6 Structure size, alignment, and padding

```rust
#[repr(C)] // prevent field reordering
struct BadLayout {
    flag: bool,    // 1 byte
    // 7 bytes padding (x86-64, align 8 for u64)
    counter: u64,  // 8 bytes
    value: f32,    // 4 bytes
    // 4 bytes padding (to align struct to 8)
}
// Size: 24 bytes. Effective data: 13 bytes. Waste: 11 bytes.
```

Rust's default (`repr(Rust)`) layout reorders fields to minimize size
(subject to alignment constraints). `repr(C)` preserves declaration order
and matches C ABI. When building FFI structs or laying out cache-sensitive
data, field ordering matters. Group fields by access frequency: hot fields
together, cold fields toward the end.

### 3.7 Enums and discriminants

A Rust enum is a discriminated union: a discriminant (tag) plus the largest
variant's data, padded to alignment. Large enum variants inflate the
structure's size. If one variant is rare and large, consider boxing it:

```rust
// 48 bytes: largest variant is [u8; 40] + discriminant/padding
enum Large {
    A(u32),
    B([u8; 40]),
}

// 16 bytes: rare variant is boxed, only discriminant + pointer + u32 live inline
enum Compact {
    A(u32),
    B(Box<[u8; 40]>),
}
```

The `Compact` version reduces cache pressure for the common case at the
cost of an allocation (and an extra indirection) when the rare variant
is constructed.

### 3.8 Indirection and reference counting

Every `Box<T>`, `Rc<T>`, `Arc<T>`, or `&T` accessed through a chain adds
a potential cache miss. Shallow indirection (single `Box`) is often fine
if the boxed value is hot and fits in cache. Deep indirection (linked
structures, trees of `Arc`-wrapped nodes) multiplies miss probability.

`Arc<T>` carries additional cost: the strong/weak counters live in a
separate allocation. Every clone, drop, and `get_mut` touches that
allocation, potentially causing a cache miss. For read-heavy shared data,
consider `Arc<[T]>` (contiguous slice) rather than `Arc<Vec<Arc<T>>>`.

### 3.9 Copying versus borrowing

Rust's ownership model encourages borrowing (`&T`, `&mut T`) rather than
copying. From a cache perspective:

- Borrowing avoids the memory traffic of copying large structures.
- But a `&T` is an indirection (8-byte pointer) that may point anywhere
  in memory. A copy of a small struct (≤ cache line) may be cheaper than
  chasing a pointer.

**Rule of thumb:** If `T` fits in 1–2 registers (≤ 16 bytes), copying is
usually faster than borrowing. If `T` is large or heap-allocated,
borrowing is preferred. Profile to be sure.

### 3.10 Small-buffer optimization

Rust's `String` and `Vec` do not have small-buffer optimization (SBO).
They always allocate on the heap. `Box<str>` and `Box<[T]>` likewise
allocate. Crates like `smallvec`, `tinyvec`, and `compact_str` provide
inline storage for small capacities, eliminating heap allocation and
pointer indirection for small collections.

### 3.11 Compact indices versus pointers

```rust
// Pointer-based (64 bits per reference, potential cache miss)
struct Node {
    children: Vec<Box<Node>>,
}

// Index-based (32-bit or smaller index into a central Vec)
struct Node {
    children: Vec<u32>, // indices into nodes: Vec<Node>
}
```

Index-based graphs store data in contiguous arrays and reference entries by
position. This saves memory (32-bit indices vs 64-bit pointers), keeps
related data together, and enables predictable traversal. The cost is an
extra array lookup to dereference an index. Many ECS frameworks (including
Bevy and Flecs) use entity indices rather than pointers for these reasons.

### 3.12 Batching, chunking, and loop tiling

Processing data in chunks that fit within a cache level improves reuse:

```rust
// Cache-oblivious: process entire array at once
for item in &data {
    expensive_transform(item); // may evict useful data
}

// Cache-aware: process in L1-sized chunks
const CHUNK: usize = 4096; // 32 KiB L1 / 8 bytes per element
for chunk in data.chunks(CHUNK) {
    for item in chunk {
        expensive_transform(item);
    }
}
```

**Loop tiling (blocking)** extends this to nested loops, e.g. matrix
multiplication: tile the inner loops so that each tile's data stays in L1
while it is fully consumed.

### 3.13 Sorting data before processing

If an algorithm's output order does not matter, sorting input by access
location can convert random access into sequential access:

```rust
// Random: entity IDs arrive in arbitrary order
for &entity in &entities {
    let pos = world.get_component::<Position>(entity);
    process(pos);
}

// Sorted by archetype (cache-friendly)
let mut entities = entities.clone();
entities.sort_by_key(|&e| world.archetype_for(e)); // group by storage location
for &entity in &entities {
    let pos = world.get_component::<Position>(entity);
    process(pos);
}
```

### 3.14 False sharing between threads

See §1.14. In Rust, crossbeam's `CachePadded<T>` or manual `#[repr(align(64))]`
structs are the primary mitigations. Thread-local storage (`thread_local!`)
also avoids false sharing by giving each thread its own copy of data.

---

## 4. Rust Iterators and Cache Behavior

### 4.1 The iterator abstraction does not determine cache behavior

An iterator adapter such as `map`, `filter`, or `fold` describes *what* to
do, not *how* to access memory. The cache behavior of an iterator chain is
determined primarily by:

1. **The data source:** Is it a `&[T]` (contiguous, sequential), a
   `HashMap::iter()` (contiguous table scan), or a `BTreeMap::iter()`
   (in-order tree walk)?

2. **The access pattern:** Does the closure traverse additional
   heap-allocated structures? Does it index into random locations?

3. **The working set:** How much data is touched per iteration? Does it fit
   in L1/L2/L3?

4. **Generated machine code:** Has LLVM fused adapters, eliminated bounds
   checks, and autovectorized the loop?

### 4.2 Lazy evaluation and iterator fusion

Rust iterators are lazy. A chain like `v.iter().map(|x| x * 2).filter(|x| *x > 10).sum()`
does not create intermediate `Vec`s. The compiler (via LLVM) often **fuses**
adapters into a single loop, producing code equivalent to:

```rust
let mut sum = 0;
for &x in v {
    let doubled = x * 2;
    if doubled > 10 {
        sum += doubled;
    }
}
```

From a cache perspective, fusion is beneficial: data is loaded once from
memory and processed through the entire pipeline before moving to the next
element. This maximizes temporal locality.

### 4.3 Monomorphization and inlining

Each unique combination of iterator types and closure types produces a
separate monomorphized function. LLVM inlines the closure body into the
loop, eliminating function-call overhead. This is generally good for
performance, but excessive monomorphization can increase code size
(instruction-cache pressure). This is rarely a problem outside of very
large generic libraries.

### 4.4 Bounds-check elimination

Iterators over slices (`iter()`, `iter_mut()`) and `Vec` provide LLVM with
length information that enables bounds-check elimination in the hot loop.
The generated code typically contains a single bounds check (or none if the
entire slice length is proven non-zero) rather than per-access checks.

### 4.5 Autovectorization and loop unrolling

Simple iterator chains over `f32`/`f64` slices are frequently
autovectorized by LLVM, using SSE/AVX instructions to process 4–16 elements
per instruction. The cache benefit is indirect: faster processing means the
prefetcher must stay farther ahead, but the access pattern is unchanged.

### 4.6 Collector adapters that materialize

Some adapters force materialization:

| Adapter | Behavior | Cache impact |
|---|---|---|
| `collect::<Vec<_>>()` | Allocates and fills a new Vec | Allocates fresh memory; sequential write, good cache behavior |
| `collect::<HashMap<_, _>>()` | Hashes each item and inserts | Random access into hash table; may be cache-intensive |
| `sort()` / `sorted()` | Collects into Vec, sorts, yields | Sorting is inherently cache-intensive (O(n log n) with irregular access) |
| `partition()` | Allocates two new Vecs | Sequential writes, good cache behavior |
| `fold()` / `reduce()` | In-place accumulation | Best case: single accumulator, no allocation |
| `for_each()` | Side effects only | No allocation beyond what the closure does |

### 4.7 Sequential and non-sequential iterator sources

| Source | Cache-friendly? | Notes |
|---|---|---|
| `&[T]` | Yes | Contiguous, sequential, predictable |
| `Vec<T>` | Yes | Same as slice |
| `array::IntoIter` | Yes | Stack-allocated, contiguous |
| `Range<usize>` | Yes | No memory access at all |
| `HashMap::iter()` | Yes (scan), No (probe) | Scanning all entries is sequential; random lookups are not |
| `BTreeMap::iter()` | Moderate | In-order tree walk; each node is contiguous but nodes are scattered |
| `LinkedList::iter()` | Poor | Pointer chasing per element |
| `Chars` (string) | Yes | UTF-8 bytes are contiguous |
| `Lines` (BufRead) | N/A | Disk/network I/O dominates |

### 4.8 Parallel iterators (rayon)

Rayon's `par_iter()` and `par_iter_mut()` split a slice into chunks and
process each chunk on a different thread. Cache behavior:

- **Good:** Each thread accesses a contiguous sub-slice, maximizing spatial
  locality and prefetcher effectiveness.

- **Potential problem - false sharing:** If multiple threads write to
  different elements within the same cache line, the line bounces between
  cores. This is uncommon with `par_iter_mut()` because rayon splits at
  indices, and the split points are typically cache-line-aligned by
  accident (elements are > 1 byte). For `AtomicU64` counters or other
  small shared state, false sharing can be significant.

- **Potential problem - work stealing:** If some chunks are much more
  expensive than others, idle threads steal work. The stolen work may be
  on a different memory region, causing the thief's cache to warm up from
  scratch. Rayon's work-stealing scheduler generally makes this rare.

- **Thread migration:** The OS may migrate a Rayon worker thread to a
  different core mid-iteration, invalidating its L1/L2 cache contents.
  Pinning threads to cores (`taskset` on Linux) can help in benchmarking
  but is rarely necessary in production.

### 4.9 Iterator chains that traverse data multiple times

```rust
// Two passes: one for filter, one for map
let result: Vec<_> = data.iter()
    .filter(|x| condition(x))
    .map(|x| transform(x))
    .collect();

// Single pass - better cache behavior
let result: Vec<_> = data.iter()
    .filter_map(|x| condition(x).then(|| transform(x)))
    .collect();
```

The single-pass version loads each element once. The two-pass version
(were it implemented as `filter` → collect → `map` → collect) would touch
each surviving element twice, potentially doubling cache misses. However,
iterator fusion typically prevents this - the example above would be fused
anyway. The risk is materializing an intermediate `Vec` (e.g. for
`sort()`), then iterating it again.

### 4.10 Iterators over pointer-based structures

Iterating over `Vec<Box<T>>` accesses `T` through a pointer. If the `T`
values are scattered in memory (allocated one at a time), the iteration
may miss cache on every element even though the `Vec` itself is
contiguous. Prefer `Vec<T>` (owning directly) or a bump allocator that
co-locates allocations.

### 4.11 Nested iterators

```rust
for chunk in data.chunks(256) {
    for item in chunk {
        // Access pattern: sequential within chunk, sequential across chunks
        process(item);
    }
}
```

Nested iteration over the same data (chunk → inner) is fine - it is still
sequential. The cache concern with nesting is when the inner loop accesses
a different, large data structure on every iteration of the outer loop,
potentially evicting the outer loop's working set.

### 4.12 Moving versus borrowing iterator items

`v.into_iter()` moves elements out of the collection (consumes it).
`v.iter()` borrows. `v.iter_mut()` mutably borrows. From a cache
perspective, the access pattern is identical for all three. The
difference is ownership: `into_iter()` gives `T`, `iter()` gives `&T`,
`iter_mut()` gives `&mut T`.

Binding `&T` in a loop avoids copying large items. For small `Copy` types
(`f32`, `u64`, small arrays), the compiler often optimizes through the
reference anyway.

---

## 5. Common Cache Offenders

### 5.1 Pointer chasing

**Symptom:** High LLC miss rate, low IPC, poor scaling with data size.
**Typical cause:** Linked lists, boxed trees, graphs stored as `Vec<Box<Node>>`
with random traversal order.

**Measurement:** Compare `perf stat -e cycles,instructions,LLC-load-misses`
between the suspect code and a flattened equivalent. High `LLC-load-misses
per instruction` (> 0.01) outside of known allocation-heavy code is
suspicious.

**Mitigation:** Replace linked structures with index-based or contiguous
storage. Flatten trees into sorted arrays where possible. Use `Vec` instead
of `LinkedList`. For graphs, consider CSR (Compressed Sparse Row) layout.

**Trade-off:** Contiguous storage complicates insertion/deletion (O(n) vs
O(1)). Choose based on the dominant access pattern. If reads dominate,
contiguous is almost always better.

### 5.2 Random memory access

**Symptom:** Near-100% cache miss rate. Runtime scales linearly with data
size, and the constant factor is high (DRAM latency dominates).

**Measurement:** A simple diagnostic is timing the same operation on a
sorted vs randomly ordered index array. If sorted is 3–10× faster, random
access is the bottleneck.

**Mitigation:** Sort data before processing, use hash-based bucketing to
group accesses by location, or restructure the algorithm to use sequential
access.

### 5.3 Large working sets

**Symptom:** Hit rate drops when data size exceeds a threshold matching
L1/L2/L3 sizes. Performance cliff rather than gradual degradation.

**Measurement:** Benchmark at multiple data sizes (e.g. 16 KiB, 32 KiB,
256 KiB, 1 MiB, 8 MiB, 64 MiB). Plot runtime and LLC misses vs size. Look
for elbows at cache-size boundaries.

**Mitigation:** Process in cache-sized chunks (tiling), compress data,
use smaller representations (f32 instead of f64, u32 indices instead of
usize pointers).

### 5.4 Repeated full scans

**Symptom:** Multiple passes over the same large array, each pass touching
all elements. The first pass loads data into cache; the last pass may find
it still there if the array fits, but if the array exceeds cache capacity,
each pass reloads from DRAM.

**Measurement:** Count the number of times the same data array is traversed
in a single frame or operation.

**Mitigation:** Fuse passes into a single traversal. If fusion is
impractical (different pass logic is maintained separately), at least
ensure passes execute back-to-back so data is reused before eviction.

### 5.5 Sparse access and large strides

**Symptom:** Accessing every Nth element (large N) causes cache-line
underutilization - only 1 of 16 `u32`s in a cache line is used; the
other 15 are wasted bandwidth.

**Measurement:** Compare the same operation at small strides (1) and large
strides (> 16). Strided access is often worse than random because the
prefetcher may learn the stride but the cache-line utilization remains poor.

**Mitigation:** Reorder data so that accessed elements are contiguous
(Structure of Arrays). If a particular subset of fields in a large struct
is always accessed together, split them into a separate array.

### 5.6 Hash-table probing

**Symptom:** High cache miss rate in `HashMap` lookups, especially when the
table is near capacity.

**Measurement:** `perf stat -e cache-misses` on the hash-table workload.
Compare with `BTreeMap` or a sorted `Vec` + binary search for the same
key set.

**Mitigation:** Use a specialized hash function for the key domain. Use
`hashbrown`'s raw entry API to avoid double lookups. For integer-keyed
maps, consider `Vec<Option<V>>` as a direct-index lookup table. For
small maps (< ~20 entries), linear scan over a `Vec<(K, V)>` often
outperforms `HashMap`.

### 5.7 Excessive indirection

**Symptom:** Chains of `Arc<Mutex<Vec<Box<dyn Trait>>>>` or equivalent.
Each access follows multiple pointers, each potentially missing cache.

**Measurement:** Design review - count pointer dereferences between the
start of a hot function and the data it actually operates on.

**Mitigation:** Flatten the ownership hierarchy. Use concrete types instead
of `dyn Trait` in hot paths. Use `enum` dispatch instead of trait objects
for small, fixed sets of variants.

### 5.8 Cache-line contention and false sharing

**Symptom:** Parallel code scales poorly despite disjoint data access.
Perf shows high `L1-dcache-load-misses` or coherence traffic.

**Measurement:** Compare single-threaded vs multi-threaded runtime for the
same total work. If speedup << N, suspect false sharing or synchronization
overhead. The `perf c2c` (cache-to-cache) tool on Linux can identify
specific cache lines with high contention.

**Mitigation:** `#[repr(align(64))]` on hot per-thread data, or
`CachePadded<T>` from crossbeam. Ensure per-thread accumulators are
separated by at least one cache line.

### 5.9 Branch-heavy code and prefetching

**Symptom:** Good cache hit rate but low IPC. Branch misprediction rate
is high (> 2–3%).

**Impact on cache:** Branch mispredictions cause pipeline flushes, which
discard in-flight loads. The prefetcher may have fetched lines that are
now useless (wasted bandwidth). Data-dependent branches inside a loop also
prevent the prefetcher from predicting future access addresses.

**Mitigation:** Use branchless programming techniques (`cmov`, SIMD
min/max, boolean-to-integer conversion) in hot loops. Sort data to make
branches predictable. Use `likely`/`unlikely` hints sparingly (they are
usually ignored by modern branch predictors).

### 5.10 Large code footprints (instruction-cache pressure)

**Symptom:** Front-end stalls even though data cache is fine. Low IPC but
few data-cache misses.

**Measurement:** `perf stat -e icache_64B.IFTAG_STALL,L1-icache-load-misses`.

**Mitigation:** Reduce monomorphization (use `dyn Trait` for cold paths).
Outline cold code with `#[cold]` and `#[inline(never)]`. Avoid very large
generic functions instantiated for many types.

### 5.11 Virtual dispatch and unpredictable call targets

**Symptom:** `dyn Trait` calls in hot loops show high branch misprediction
and potentially i-cache misses from scattered function bodies.

**Measurement:** Compare a `dyn Trait` loop with an equivalent `enum` +
match loop for the same operation.

**Mitigation:** Use `enum` dispatch for closed sets of types. If virtual
dispatch is necessary, sort work by concrete type so that the indirect
branch predictor can learn the pattern.

---

## 6. Measuring Cache Behavior

### 6.1 Hardware performance counters

Modern CPUs contain **Performance Monitoring Units (PMUs)** that count
microarchitectural events: cycles, instructions retired, cache misses at
various levels, branch mispredictions, and hundreds of other events.

#### Counting versus sampling

- **Counting** (`perf stat`): Aggregates event counts over the entire
  program or a specified duration. Best for: getting total misses, IPC,
  and bandwidth for a benchmark.

- **Sampling** (`perf record` / `perf report`): Periodically records the
  instruction pointer and call stack. Attributes events to specific
  functions and source lines. Best for: finding which code is responsible
  for cache misses.

#### Process-level, thread-level, and system-wide

- **Process-level:** `perf stat ./my_binary` measures only the target
  process and its children. Most common for application benchmarking.

- **Thread-level:** `perf stat -t <tid>` measures a specific thread. Useful
  for isolating a worker thread in a multi-threaded program.

- **System-wide:** `perf stat -a` measures all CPUs. Useful for
  understanding system-level interference.

#### Measuring a specific code region

The `perf_event_open` syscall (exposed via the `perf-event` or `perfctl`
crates in Rust) allows starting/stopping counters around a specific code
region. This is the most precise way to measure a particular function or
loop body:

```rust
// Conceptual example using perf-event crate
let mut counter = PerfCounter::new(/* PERF_TYPE_HARDWARE, PERF_COUNT_HW_CACHE_MISSES */)?;
counter.start()?;
// ... code to measure ...
counter.stop()?;
let misses = counter.read()?;
```

#### Event multiplexing and scaled counts

PMUs have a limited number of hardware counters (typically 4–8 per core).
If you request more events than available counters, the kernel
**multiplexes** - it rotates events onto the counters and scales the
results to estimate what the full-period count would have been. Scaled
counts are marked as such and are less reliable. To avoid multiplexing,
limit events to the available counter count, or use a tool that groups
events efficiently.

#### CPU-specific events

`cache-references` and `cache-misses` are generic Linux `perf` events, but
their exact meaning varies by CPU:

- On Intel: `cache-references` typically counts L1 data-cache fills
  (demand + prefetch). `cache-misses` counts last-level cache misses.

- On AMD: The mapping differs by generation.

- On ARM: Different event encodings entirely.

Prefer CPU-specific events when possible (e.g. Intel's
`MEM_LOAD_RETIRED.L3_MISS` for precise LLC miss counting). Use `perf list`
to discover available events on your machine. When portability is needed,
fall back to generic events and document the measuring CPU.

#### Kernel permissions

Reading hardware counters typically requires:
- Linux: `perf_event_paranoid` ≤ 2 (or running as root)
- macOS: No standard PMU access from userspace (Instruments works)
- Windows: Admin privileges for some counters

#### Virtual-machine and container limitations

VMs may not expose PMU passthrough to guests. Containers share the host
kernel and PMU, but may have restricted `perf_event_open` access depending
on the container runtime and seccomp profile.

#### Measurement overhead

Hardware counters have negligible overhead (~1–3% for counting mode).
Sampling mode has overhead proportional to sampling frequency. At the
default 4000 Hz on Linux, overhead is usually < 5%.

### 6.2 Simulation (Cachegrind)

Valgrind's Cachegrind tool (`valgrind --tool=cachegrind`) simulates a
simplified cache hierarchy and counts simulated hits/misses. It runs the
program on a synthetic CPU, so the instruction count is deterministic but
cache behavior may not match real hardware:

- **Cachegrind models a fixed cache geometry** (typically L1: 64 KiB D,
  32 KiB I; LL: 8 MiB unified). Real CPU caches differ in size,
  associativity, and replacement policy.

- **No prefetching simulation** - Cachegrind does not model hardware
  prefetchers, which can dramatically change real-world cache behavior.

- **Deterministic** - Same input always produces same result. Useful for
  regression testing and comparing algorithmic cache behavior.

- **5–50× slowdown** - Not suitable for large benchmarks, but useful for
  small isolated functions.

Cachegrind is valuable for understanding algorithmic cache behavior (how
many unique cache lines an operation touches) even though the absolute
numbers do not predict hardware performance.

### 6.3 Sampling profilers

Profilers like `perf record`, Intel VTune, and Instruments periodically
sample the instruction pointer. They attribute events statistically to
functions and source lines.

- **Function-level attribution:** Shows which functions account for the
  most samples. Hot functions with high cache-miss samples are candidates
  for investigation.

- **Instruction-level attribution:** VTune and `perf annotate` can
  attribute events to individual instructions, showing exactly which load
  or store caused misses.

- **Call stacks:** `perf record -g` captures call graphs, enabling
  identification of the call chain that led to cache misses.

- **Inlining and symbolization:** Inlined functions appear as part of
  their caller. Debug info (`debug = true` in release Cargo profile, or
  DWARF/dSYM preservation) is needed for source-level attribution.

### 6.4 Timing-only benchmarks

Elapsed (wall-clock) time alone cannot distinguish cache misses from
compute, branch mispredictions, or I/O. However, wall-clock time is
ultimately what users experience, and it is the metric that optimization
efforts must eventually improve. A strategy of "measure timing first, then
diagnose with PMU counters if timing is puzzling" is pragmatic.

---

## 7. Analysis Tools

### 7.1 Linux `perf`

`perf` is the standard Linux profiling tool, part of the kernel source.
Install via your package manager: `linux-tools-common` / `perf`.

#### `perf stat` - aggregate counters

```bash
# Basic cycle + instruction counting
perf stat ./target/release/my_benchmark

# Cache-specific events (Intel CPU - verify with `perf list`)
perf stat \
  -e cycles,instructions,\
  L1-dcache-loads,L1-dcache-load-misses,\
  LLC-loads,LLC-load-misses,\
  branches,branch-misses \
  ./target/release/my_benchmark

# Per-task mode: measures only the specified command
# (default behavior of `perf stat` without `-a`)

# Repeat 5 times for stability
perf stat -r 5 ./target/release/my_benchmark
```

On Intel CPUs, `LLC-loads` and `LLC-load-misses` are often aliases for
`MEM_INST_RETIRED.ALL_LOADS` and `MEM_LOAD_RETIRED.L3_MISS` respectively
- check your CPU's event list.

On AMD Zen, use `l3_lookup_state.l3_miss` and `l3_request_g1.all_rd_blk`.
The generic `cache-misses` works but maps to different underlying events.

#### `perf record` / `perf report` - sampling

```bash
# Record with call stacks and sample at 999 Hz
perf record -g -F 999 ./target/release/my_benchmark

# Interactive report
perf report

# By cache misses (Intel-specific event)
perf record -e LLC-load-misses -g ./target/release/my_benchmark
perf report
```

#### `perf annotate` - instruction-level attribution

```bash
perf annotate function_name
```

Shows disassembly with percentage of samples attributed to each instruction.
Look for load instructions with high sample counts - these indicate cache
miss hot spots.

#### Discovering events

```bash
# List all events
perf list

# Filter for cache events
perf list cache

# Show events for a specific PMU
perf list --details
```

### 7.2 Valgrind Cachegrind

```bash
# Run with Cachegrind
valgrind --tool=cachegrind ./target/release/my_benchmark

# Annotate source
cg_annotate cachegrind.out.<pid> --auto=yes
```

Use `--D1=<size>,<assoc>,<line_size>` and `--LL=...` to adjust simulated
cache parameters. This is useful for "what if" experiments: how would
cache behavior change if L1 were twice as large?

### 7.3 `iai-callgrind` (Rust crate)

`iai-callgrind` is a Rust benchmarking harness that runs benchmarks under
Cachegrind and collects deterministic instruction counts, L1/L2 hits/misses,
and branch prediction results. It is designed for CI regression testing:

```rust
use iai_callgrind::{library_benchmark, library_benchmark_group, main};

#[library_benchmark]
#[bench::small(100)]
#[bench::large(10000)]
fn bench_process(n: usize) {
    // ... generate data, process it ...
}

library_benchmark_group!(name = my_group; benchmarks = bench_process);
main!(library_benchmark_groups = my_group);
```

Run with:
```bash
cargo bench --bench iai_benchmarks
```

### 7.4 Criterion.rs

Criterion is the standard Rust statistical benchmarking library. It does
not directly measure cache events, but it provides robust timing with
warm-up, outlier detection, and confidence intervals. Use it as the first
layer of measurement; if timing shows a problem, then drill into cache
counters.

### 7.5 PAPI (Performance API)

PAPI provides a cross-platform library for accessing hardware performance
counters. The `papi` crate provides Rust bindings. PAPI can read counters
programmatically from within a benchmark, similar to `perf_event_open` but
with a higher-level API.

### 7.6 Intel VTune Profiler

VTune is Intel's graphical profiler for Windows and Linux. It provides:

- **Microarchitecture Exploration:** Hotspot analysis with cache-miss and
  branch-misprediction attribution.
- **Memory Access analysis:** Shows which data structures cause cache
  misses, with source-line granularity.
- **HPC Performance Characterization:** Detailed breakdown of cycles by
  category (retiring, bad speculation, front-end bound, back-end bound
  with sub-categories for memory vs core bound).

VTune is free (no license cost) but requires an Intel CPU for full
functionality.

### 7.7 AMD uProf

AMD's equivalent of VTune. Provides similar microarchitectural analysis
for AMD Zen CPUs. Free.

### 7.8 macOS Instruments

Part of Xcode. Provides "Counter" template for PMU events, and "System
Trace" for context-switch and scheduling analysis. Instruments does not
expose arbitrary PMU events as flexibly as Linux `perf`, but the built-in
templates cover common cache and memory metrics.

### 7.9 Windows Performance Recorder and Analyzer

WPR captures ETW (Event Profiling for Windows) traces including hardware
counter data. WPA provides graphical analysis. Requires admin privileges.
More complex to use than `perf` on Linux but provides rich system-wide
data.

### 7.10 Compiler-generated assembly

Inspecting generated code is essential when you suspect:

- Bounds checks are not being eliminated.
- Iterator adapters are not being fused.
- Autovectorization is not triggering.
- Code size is unexpectedly large (i-cache concern).

Tools:

```bash
# cargo-asm - show assembly for a specific function
cargo asm --release --bin my_binary my_module::my_function

# Compiler Explorer (godbolt.org) - interactive
# Use `--target` and `-C opt-level=3` for representative output

# llvm-objdump
llvm-objdump -d target/release/my_binary | less

# Generate assembly alongside binary
RUSTFLAGS="--emit asm" cargo build --release
# Output: target/release/deps/my_crate-<hash>.s
```

### 7.11 Limitations of generic events

The Linux `perf` generic events `cache-references` and `cache-misses` are
**CPU-specific in their definition**. On some Intel CPUs, `cache-references`
counts L2 references; on others, LLC references. On some ARM CPUs,
`cache-references` is not even implemented. Always verify your CPU's
mapping by checking `perf list --details` or the processor's optimization
manual.

---

## 8. Measuring Iterator Pipelines

### 8.1 Create an isolated benchmark

Extract the iterator chain into a standalone benchmark function. Use
representative, deterministic input. The benchmark should measure only the
iterator execution, not data generation or allocation.

### 8.2 Move setup outside the measured region

```rust
// RIGHT: setup outside measurement
let data: Vec<u32> = (0..1_000_000).collect();
b.iter(|| {
    let sum: u64 = data.iter().map(|&x| x as u64 * 2).sum();
    black_box(sum);
});

// WRONG: setup inside measurement (but data-gen cost may dominate)
b.iter(|| {
    let data: Vec<u32> = (0..1_000_000).collect(); // allocation + fill
    let sum: u64 = data.iter().map(|&x| x as u64 * 2).sum();
    black_box(sum);
});
```

### 8.3 Prevent dead-code elimination

Use `std::hint::black_box()` to prevent the compiler from eliminating
computations whose results are not used:

```rust
b.iter(|| {
    let result = data.iter().fold(0u64, |acc, &x| acc.wrapping_add(x as u64));
    black_box(result); // Compiler cannot optimize this away
});
```

### 8.4 Compare iterator with explicit loop

Benchmark both styles to determine if the iterator abstraction has any
runtime cost:

```rust
fn bench_iterator(data: &[u32]) -> u64 {
    data.iter().map(|&x| x as u64 * 2).sum()
}

fn bench_explicit_loop(data: &[u32]) -> u64 {
    let mut sum = 0u64;
    for &x in data {
        sum += x as u64 * 2;
    }
    sum
}
```

When both produce identical assembly (check with `cargo asm`), any
difference is measurement noise. When they differ, inspect the assembly
to understand why.

### 8.5 Vary working-set size

Run the same iterator pipeline at different input sizes: 1K, 4K, 16K, 64K,
256K, 1M, 4M, 16M, 64M elements. Plot runtime and cache misses vs size.
Look for elbows at cache-size boundaries.

### 8.6 Test different access patterns

For a given data structure, benchmark three access patterns:

1. **Sequential:** `(0..len).map(|i| data[i])`
2. **Strided:** `(0..len).step_by(stride).map(|i| data[i])`
3. **Random:** Pre-shuffle an index array, then `indices.iter().map(|&i| data[i])`

The ratio of random to sequential runtime reveals how much of the
algorithm's cost is memory access versus compute.

### 8.7 Normalize by items processed

Report both absolute numbers and per-item metrics:

```
Total cycles:      2.1 × 10⁹
Items processed:   1.0 × 10⁶
Cycles per item:   2,100
LLC misses:        1,500
Misses per item:   0.0015
```

Per-item metrics make it easier to reason about scaling and to compare
different-size benchmarks.

### 8.8 Collect both timing and counter data

Timing answers "is it fast enough?" Counters answer "why is it fast or
slow?" Both are needed for diagnosis.

### 8.9 Noise sources in process-wide counters

Process-wide `perf stat` counts events for the entire process lifetime,
including:

- Dynamic linker/loader startup
- `main()` setup and teardown
- Allocator overhead
- I/O and system calls
- Other threads
- Rayon thread-pool creation and teardown

**Mitigations:**
- Use a dedicated benchmark binary that does no I/O.
- Use `perf_event_open` or the `perf-event` crate to measure only a
  specific code region.
- Warm up the Rayon pool before the measured region (as `ecs_hybrid` does
  with `rayon::broadcast` in `Engine::new`).
- Use `perf stat --delay 1000` to skip the first second of execution.

---

## 9. Experimental Methodology

### 9.1 Build configuration

Always use a release build:
```bash
cargo build --release
```

The `ecs_hybrid` crate uses:
```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
```

- `opt-level = 3` enables aggressive optimizations including
  autovectorization.
- `lto = "thin"` enables cross-crate inlining without the compile-time
  cost of full LTO.
- `codegen-units = 1` prevents parallel codegen from inhibiting
  optimization.

### 9.2 Record the environment

Document:
- CPU model, microarchitecture, core count, base/max frequency
- Memory configuration (channels, speed, size)
- OS, kernel version
- Rust version (`rustc --version`)
- LLVM version (`rustc --version --verbose | grep LLVM`)
- Cargo features enabled
- `RUSTFLAGS` and target CPU (`-C target-cpu=native` enables CPU-specific
  instructions and tuning)

### 9.3 Input data

- Use fixed, representative inputs.
- For random data, use a deterministic seed (`rand::SeedableRng` seeding
  with a constant).
- Pre-generate data and reuse it across benchmark runs.
- Ensure the data size matches production expectations.

### 9.4 Warm-up

Modern CPUs need time to reach steady-state frequency (turbo boost) and
to warm caches. Criterion automatically warms up for 3 seconds. When
using raw `perf stat`, run the benchmark in a loop and discard the first
few iterations.

### 9.5 Multiple samples

Run at least 5–10 samples. Report the distribution (median, min, max,
95% confidence interval) rather than just the best result. The best result
is useful for understanding the theoretical minimum; the distribution is
useful for understanding variability.

### 9.6 Frequency scaling and turbo

CPU frequency scaling introduces variance:

```bash
# Linux: set performance governor
sudo cpupower frequency-set -g performance

# Check current governor
cpupower frequency-info

# Disable turbo (optional, reduces variance at cost of peak speed)
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo  # Intel
```

On macOS, frequency scaling is opaque but generally well-behaved for
sustained workloads. On Windows, use "High Performance" power plan.

### 9.7 Pin to a CPU

```bash
# Linux: pin to core 3
taskset -c 3 ./target/release/my_benchmark

# In combination with perf
perf stat taskset -c 3 ./target/release/my_benchmark
```

Pinning prevents OS scheduler migration, which invalidates L1/L2 cache
contents. It also reduces variance in multi-core CPUs with non-uniform
L3 latency.

### 9.8 Record migrations and context switches

```bash
perf stat -e cpu-migrations,context-switches ./target/release/my_benchmark
```

High context-switch counts indicate system interference and may explain
noisy benchmark results.

### 9.9 Distinguish warm-cache and cold-cache experiments

- **Warm cache:** Run the benchmark once to load data into cache, then run
  it again (or run the first iteration inside a loop and discard it).

- **Cold cache:** The first access loads from DRAM. Simulating cold cache
  is difficult because flushing caches from userspace is not generally
  possible. Reading a large buffer (> LLC size) between runs is a common
  approximation but does not guarantee a clean cache (prefetchers, cache
  replacement policy, and OS activity may still leave hot lines resident).

### 9.10 NUMA placement

```bash
# Linux: allocate and bind to NUMA node 0
numactl --membind=0 --cpunodebind=0 ./target/release/my_benchmark
```

On single-socket systems, NUMA is not a concern.

### 9.11 Avoid cross-machine comparison

A benchmark result from an Intel i7-12700H (big.LITTLE, DDR5) does not
generalize to an AMD Ryzen 9 5950X (uniform cores, large L3) or an Apple
M2 (wide cores, 128-byte cache lines, unified memory). Report results with
the specific hardware configuration and avoid saying "X is faster than Y"
without qualifying the measurement platform.

---

## 10. Interpreting Results

### 10.1 High miss rate but low total miss count

If miss rate is 20% but you only have 100 accesses total, the 20 misses
cost less than 1 µs. Miss *rate* without miss *count* is incomplete.

### 10.2 Low miss rate but huge access volume

If miss rate is 0.1% but you have 1 billion accesses, 1 million LLC misses
still cost ~30 million cycles (at ~30 cycles/LLC miss). Large absolute
miss counts, even at low rates, may dominate runtime.

### 10.3 More misses but faster runtime

A "cache-friendly" algorithm (contiguous storage, sequential access) might
show *more* total cache misses than a "cache-unfriendly" one simply because
it processes data faster. More misses per second is not a problem if more
work is being done. Compare misses per unit of work, not total misses.

### 10.4 Fewer misses but slower runtime

Replacing a hash table with a sorted array + binary search reduces
cache-miss footprint but replaces O(1) hashing + probing with O(log n)
comparisons. The compute cost may outweigh the cache benefit. Measure both.

### 10.5 High LLC misses with high memory bandwidth

A streaming workload (e.g. `memcpy`, vector addition) will show near-100%
LLC miss rate and high memory bandwidth. This is expected and optimal -
the data is too large for cache and is being processed sequentially. The
prefetcher keeps the pipeline full. LLC misses alone are not a problem
for streaming workloads.

### 10.6 High cycles with few cache misses

If IPC is low but cache miss counts are low, suspect:
- Branch mispredictions (`perf stat -e branch-misses`)
- Data dependencies (long chains of dependent instructions)
- Division/square root operations (high latency even with L1 hits)
- Front-end stalls (i-cache misses, decoder limitations)
- Synchronization (lock contention, atomic operations)

### 10.7 Multiplexed or scaled counter results

If `perf stat` shows "(scaled)" or multiplexing occurred, the counter
values are estimates, not exact counts. Reduce the number of simultaneous
events or run with fewer events per invocation (multiple runs).

### 10.8 Correlation versus causation

A change that both improves runtime and reduces cache misses does not
prove that the cache-miss reduction *caused* the improvement. The change
might have also simplified control flow (fewer branches), enabled better
autovectorization, or reduced allocations. Controlled experiments (changing
only one variable) and assembly inspection are needed for causal claims.

---

## 11. Cache Optimization Techniques

### 11.1 Improve locality

Arrange data so that items accessed together are stored together. This is
the single most effective cache optimization.

### 11.2 Use contiguous storage

Replace `Box<Node>` chains with `Vec<Node>` + indices. Replace `HashMap`
with `Vec` + sort + binary search (if reads dominate inserts).

### 11.3 Reduce structure size

Smaller structures mean more elements per cache line. Use `u32` instead of
`usize` for indices (on 64-bit). Use `f32` instead of `f64` if precision
allows. Eliminate padding by reordering fields.

### 11.4 Reorder fields

Group hot (frequently accessed) fields together at the start of a struct.
This increases the probability they share a cache line.

### 11.5 Split hot and cold fields

Place rarely accessed fields in a separate allocation (or at the end of the
struct, with a comment). The hot path then touches fewer cache lines.

### 11.6 Use Structure of Arrays (SoA)

Separate each field into its own contiguous array. This is the standard
layout in high-performance ECS and particle systems.

### 11.7 Process data in chunks

Break large datasets into chunks that fit in L1 (32 KiB) or L2 (256 KiB).
Process each chunk completely before moving to the next.

### 11.8 Fuse processing stages

Combine multiple passes over the same data into a single traversal.

### 11.9 Avoid unnecessary intermediate collections

Prefer iterator chains that avoid `collect()`. When a collection is needed,
pre-allocate with `Vec::with_capacity()`.

### 11.10 Reuse buffers

Allocate working buffers once and reuse them across iterations, rather than
allocating fresh buffers each time. This also avoids allocator overhead.

### 11.11 Replace pointers with compact indices

`u32` indices into a central array instead of `*const T` or `Box<T>`.
Saves 4 bytes per reference (on 64-bit) and keeps references dense.

### 11.12 Sort or group by access location

If processing order is flexible, sort entities by archetype (or by the
memory address of their backing storage) before processing.

### 11.13 Separate per-thread writable state

Use thread-local accumulators or per-thread buffers. Merge results after
parallel processing completes.

### 11.14 Add padding to prevent false sharing

`#[repr(align(64))]` on per-thread hot data structures.

### 11.15 Use software prefetching only when justified

Rust exposes `std::intrinsics::prefetch_read_data` and
`std::intrinsics::prefetch_write_data` (nightly only, unstable).
Software prefetching is rarely beneficial - hardware prefetchers are good
at sequential and strided patterns. Only consider it when you have
measured a specific miss pattern that the hardware cannot predict and
shown that prefetch intrinsics improve it.

### 11.16 Reduce code size

If i-cache pressure is measured, use `#[inline(never)]` on cold paths,
`#[cold]` on error paths, and consider dynamic dispatch (`dyn Trait`) for
rarely executed code paths.

### Trade-off summary

Every cache optimization has costs:

| Technique | Potential cost |
|---|---|
| SoA layout | Code complexity; harder to access all fields of one entity |
| Compact indices | Extra indirection per access; debugging difficulty |
| Chunking | Increased code complexity; boundary handling |
| Padding | Increased memory usage |
| Field reordering | Maintainability (need comments explaining layout) |
| Buffer reuse | Risk of stale data; careful lifetime management |
| Inline control | Abi boundary may prevent optimization |

Measure before and after. Document the trade-off.

---

## 12. Repository-Specific Observations

*This section describes observations specific to the `ecs_hybrid` crate at
`d:\Programming\Rust-Hybrid-ECS`. These are observations and hypotheses,
not confirmed performance problems.*

### 12.1 Storage layout and cache design

The ECS uses **Structure of Arrays (SoA)** via archetype storage
(`src/archetype.rs`). Each component type in an archetype is stored in its
own contiguous `Vec<T>`, accessed through a `TraitTypeMap`. This is a
cache-friendly design: when a system iterates `(&mut Position, &Velocity)`,
both component arrays are traversed sequentially, maximizing spatial
locality and enabling hardware prefetching.

The `Archetype` struct itself stores `component_types: Vec<ComponentId>`,
`component_mask: ComponentMask`, `component_storages: TraitTypeMap<…>`,
`entities: Vec<Entity>`, and `component_ticks: HashMap<ComponentId, Vec<ComponentTicks>>`.
The tick vectors are maintained in lockstep with component data (same
index = same entity), so iteration with change detection (`Changed<T>`)
still benefits from sequential access to tick arrays.

### 12.2 Iterator-heavy code paths

The primary iterator code lives in `src/query/iter.rs`:

- **Sequential path** (`QueryIterMut`): A classical per-archetype iterator.
  Hot loop in `Iterator::next()` advances an index, checks the filter, and
  calls `Q::fetch_with_state()`. The hot path is designed for branch
  prediction: filter matches are checked with a fast-path skip when
  `F::ACCEPTS_ALL` is true.

- **Parallel path** (`ParQueryIter`): Builds flat work slices
  (`Vec<(arch_idx, start, end)>`), divides them into groups based on
  timing feedback (`TARGET_GROUP_DURATION_NS = 50_000`), and spawns
  each group as a `rayon::scope` task. Each thread processes all its
  assigned slices contiguously - no per-slice queue contention.

Key constants affecting cache behavior:
- `DEFAULT_SLICE_ENTITIES = 4096` - sized to half-fill L1 data cache for
  8-byte components (32 KiB / 8 B = 4096).
- `TARGET_GROUP_DURATION_NS = 50_000` - targets 50 µs per parallel group,
  balancing OS wake-up latency (~10 µs) against parallelism.

The timing-feedback loop stores per-label EMAs in
`IteratorTimings.per_iterator_label_average_duration` on `World`, keyed
by user-provided `.label("system_name")` strings. This allows different
iterators in the same system to learn independent group counts.

### 12.3 Existing benchmarks relevant to cache study

The project has five Criterion benchmark suites:

| Suite | File | Cache-relevant aspects |
|---|---|---|
| `query_iteration` | `benches/query_iteration.rs` | Seq vs parallel iteration at 1K–1M entities; batch-size scaling; crossover point where parallel beats sequential (~20K entities) |
| `entity_lifecycle` | `benches/entity_lifecycle.rs` | Entity create/destroy at 100–10K scale; allocation and HashMap behavior |
| `archetype_migration` | `benches/archetype_migration.rs` | Component add/remove throughput at 1K–10K; archetype transitions touch multiple `Vec`s |
| `scheduler_graph` | `benches/scheduler_graph.rs` | Scheduler build time at 10–200 systems; O(n²) conflict checking |
| `frame_loop` | `benches/frame_loop.rs` | End-to-end `process_frame()` at 1K–100K entities with 3 systems |

The **query crossover** benchmark (`benches/query_iteration.rs`:
`bench_crossover`) is directly useful for cache analysis: it compares
sequential and parallel iteration across entity counts from 1K to 100K.
The crossover point (~20K entities) is where parallel overhead is
amortized. Cache behavior differs below and above this point.

The **batch-size scaling** benchmark varies batch size from 1 to 1024
entities at a fixed 10K total. This directly tests the cache impact of
work granularity.

### 12.4 Potential cache-sensitive areas worth measuring

1. **Archetype transition cost during iteration:** When systems queue
   `add_component` / `remove_component` via `Commands`, the actual
   migration happens at the end of the frame (`execute_queued_commands`).
   This is outside iteration, but the clone + `Vec` reallocation in
   `move_entity_to_archetype` involves touching multiple discontiguous
   allocations.

2. **Filter evaluation with `Changed<T>`:** The `Changed<T>` filter reads
   `component_ticks` for each entity. In the hot loop, this is an
   additional memory access per entity. The tick vector is contiguous and
   accessed sequentially (same index as component data), so spatial
   locality is preserved, but the extra cache-line touch may push the
   working set past L1 capacity for wide queries (many components
   accessed per entity).

3. **`TraitTypeMap` dispatch:** The `get_storage::<T>()` call in
   `Q::fetch_with_state` involves a dynamic lookup in the trait-type map.
   If this is not inlined and devirtualized, it could cause i-cache
   misses in the hot loop. Inspecting generated assembly for
   `QueryIterMut::next` would clarify whether the dispatch is
   monomorphized away.

4. **`rayon::scope` task distribution:** Each `scope.spawn()` sends a task
   to Rayon's work queue. For very small entity counts (below the adaptive
   fallback threshold of `num_threads × 256`), the sequential path is
   taken. Above the threshold, the number of spawned groups is determined
   by the timing-feedback loop. It would be instructive to measure
   whether the default `num_threads` groups (when timing hint is zero)
   provide optimal cache reuse for common workloads.

5. **`IteratorTimings` lock contention:** The `Mutex<IteratorTimings>` is
   locked twice per `for_each()` call (once to read the EMA hint, once to
   write the result). The critical section is a few HashMap operations
   (nanoseconds). In a frame with many parallel iterators, lock contention
   is unlikely to be measurable, but it is worth confirming with a
   contention profiler for systems with many small iterators.

### 12.5 Factors making attribution difficult

- **Rayon thread-pool reuse:** Rayon keeps threads alive across frames,
  so L1/L2 cache warmth carries over. A cold-cache frame is rare after
  the first one.

- **`profile_scope!` zones:** When Tracy is enabled, every iterator
  construction and every parallel group creates a Tracy zone. The
  overhead of Tracy instrumentation (string formatting, client
  communication) may add noise to timing measurements. Tracy-disabled
  builds (no `tracy` feature) eliminate this.

- **`IteratorTimings` EMA smoothing:** The 32-frame EMA means that the
  number of parallel groups adapts gradually. The first frame always uses
  `num_threads` groups; subsequent frames converge toward an optimal
  group count. A single-frame benchmark does not capture this adaptation.
  Multi-frame benchmarks (like `frame_loop`) amortize it.

- **Component count per entity:** The benchmarks use 2–4 components per
  entity. Production workloads may have 5–15 components per entity,
  widening the per-entity cache footprint and potentially changing the
  optimal batch size and group count.

---

## 13. Practical Checklists

### 13.1 Reviewing cache-sensitive Rust code

- [ ] Are hot data structures stored contiguously (`Vec`, arrays, slices)?
- [ ] Is pointer chasing minimized in hot loops?
- [ ] Are fields ordered so hot fields are grouped together?
- [ ] Does the working set of the hot loop fit in L1/L2?
- [ ] Are parallel per-thread data structures padded against false sharing?
- [ ] Is `Box<dyn Trait>` dispatch in hot paths? Can it be `enum` instead?
- [ ] Are intermediate `Vec` allocations in iterator chains eliminated?
- [ ] Does the access pattern allow hardware prefetching (sequential or
      regular stride)?
- [ ] Is `HashMap` used where `Vec<(K, V)>` + linear scan or binary search
      would be faster for small N?
- [ ] Does any code sort work by access location before processing?

### 13.2 Creating a cache benchmark

- [ ] Build in release mode with `lto = "thin"` and `codegen-units = 1`.
- [ ] Use representative, deterministic input data.
- [ ] Move setup and allocation outside the measured region.
- [ ] Use `black_box()` on inputs and outputs.
- [ ] Warm up (Criterion does this automatically; for manual benchmarks,
      run a few iterations before timing).
- [ ] Vary working-set size across L1/L2/L3 boundaries.
- [ ] Record CPU model, memory configuration, and Rust version.
- [ ] Record `RUSTFLAGS` and enabled features.

### 13.3 Running hardware-counter measurements

- [ ] Ensure `perf_event_paranoid` allows user access (≤ 2 on Linux).
- [ ] Pin to one CPU core: `taskset -c 3`.
- [ ] Set CPU governor to performance: `cpupower frequency-set -g performance`.
- [ ] Limit simultaneous events to avoid multiplexing.
- [ ] Run at least 5 samples and report distribution.
- [ ] Record `cycles`, `instructions`, `L1-dcache-load-misses`,
      `LLC-load-misses`, `branches`, `branch-misses`.
- [ ] Check for `cpu-migrations` and `context-switches` (should be near zero).
- [ ] Document which events are generic vs CPU-specific.

### 13.4 Comparing two implementations

- [ ] Measure both with identical input data and hardware configuration.
- [ ] Normalize by items processed (cycles/item, misses/item).
- [ ] Compare both timing and hardware counter data.
- [ ] Inspect generated assembly for both (`cargo asm`).
- [ ] If differences are small (< 3%), run more samples or accept that
      the difference is within noise.
- [ ] Check whether the improvement is consistent across data sizes.
- [ ] Report the distribution (median, min, max), not just the best run.

### 13.5 Investigating unexpected results

- [ ] Check for multiplexed counter values.
- [ ] Check `cpu-migrations` and `context-switches`.
- [ ] Verify release build (`--release`).
- [ ] Verify no debug assertions remain active.
- [ ] Inspect assembly for the hot loop.
- [ ] Verify that `black_box()` is preventing dead-code elimination.
- [ ] Run with `perf record -g` and check where time is actually spent.
- [ ] Check whether the benchmark is measuring what you think (is the
      measured region including allocation or I/O?).
- [ ] Try on a different CPU microarchitecture (Intel vs AMD vs Apple
      Silicon) to see if the effect is consistent.

### 13.6 Deciding whether a cache optimization is worthwhile

- [ ] Is the code path hot? (Confirmed by profiling, not intuition.)
- [ ] Is the measured improvement > 5% and statistically significant?
- [ ] Does the improvement hold across relevant data sizes?
- [ ] What is the cost in code complexity, readability, and
      maintainability?
- [ ] Does the optimization help or hurt other access patterns?
- [ ] Is there a simpler way to achieve the same gain?
- [ ] Have you documented the trade-off in a code comment?
- [ ] Does the optimization introduce platform-specific assumptions?
- [ ] Will the optimization survive a compiler upgrade (or is it working
      around a specific LLVM version's behavior)?

---

## References

- Drepper, U. "What Every Programmer Should Know About Memory." 2007.
  (Classic reference; some details are dated but fundamentals remain correct.)
- Intel 64 and IA-32 Architectures Optimization Reference Manual.
- AMD Processor Programming Reference for Family 19h/1Ah (Zen 3/4/5).
- Agner Fog. "The microarchitecture of Intel, AMD and VIA CPUs." 2024.
- `perf` wiki: <https://perf.wiki.kernel.org/>
- Criterion.rs: <https://github.com/bheisler/criterion.rs>
- Valgrind Cachegrind: <https://valgrind.org/docs/manual/cg-manual.html>
- The Rust Performance Book: <https://nnethercote.github.io/perf-book/>
