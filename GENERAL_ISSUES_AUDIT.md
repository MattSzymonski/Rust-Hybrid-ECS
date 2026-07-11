# Rust Hybrid ECS - Audit v4

**Date:** July 12, 2026
**Fresh analysis:** All 27 `.rs` files re-reviewed

---

## 1. Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Critical Issues](#2-critical-issues)
3. [Medium Issues](#3-medium-issues)
4. [Low Issues - from audit v3](#4-low-issues--from-audit-v3)
5. [Low Issues - new in v4](#5-low-issues--new-in-v4)
6. [Performance Considerations](#6-performance-considerations)
7. [Summary Table](#7-summary-table)
8. [Prioritized Action Items](#8-prioritized-action-items)

---

## 2. Critical Issues

### 2.1 [CRITICAL] Lifetime transmutation in SystemParam - NONTIVIAL TO FIX, MARKED IN CODE

**File:** `src/system.rs`

**Category:** Safety / Undefined Behavior

Every `SystemParam` implementation uses `std::mem::transmute` to extend a
local borrow to `'static`. The pattern is used for `Commands<'static>`,
`Query<'static, Q, F>`, `Res<'static, T>`, and `ResMut<'static, T>`. Each
site now has a `// SAFETY:` comment, but the underlying unsafety remains:
if any parameter escapes the system function, it's UB.

**Recommendation:** Explore a token-based pattern (`&'sys SystemToken`)
that makes escape a compile error.

---

### 2.2 [CRITICAL] `ScriptContext::get_component_mut` aliasing `&mut` - NONTIVIAL TO FIX, MARKED IN CODE

**File:** `src/scripting.rs`

**Category:** Safety / Undefined Behavior

A script's `&mut self` and the returned `&mut T` can both be the same type
`T` when a script calls `get_component_mut::<Self>(own_entity)` - two
`&mut T` references to the same data. The doc comment acknowledges this.

**Recommendation:** Return `*mut T` instead of `&mut T`, forcing callers
to use raw pointer operations. Or use `UnsafeCell` for script storage.

---

## 3. Medium Issues

### 3.1 [MEDIUM] `EntityLocation` indirection via HashMap - REQUIRES MESSY REFACTOR

**File:** `src/world.rs`

**Category:** Performance

Each `get_component` needs two HashMap lookups: `entity_locations` then
`archetypes`. Both involve hashing + bucket probing. Replacing with a
flat `Vec<Archetype>` + generational index in `EntityLocation` would
eliminate one cache miss per component access.

**Recommendation:** Switch to a `Vec`-based archetype store with a
SlotMap-style generational index. EntityLocation stores `archetype_index: u32` instead of `ArchetypeId`.

---

## 4. Low Issues - IGNORE, NOT IMPORTANT

### 4.1 [LOW] `Tick` wrap-around not documented

**File:** `src/component.rs`

`change_tick` uses `wrapping_add(1)` - after ~828 days at 60 FPS, ticks
wrap to 0, breaking change detection.

**Recommendation:** Document the wrap-around. Add debug-mode warning near `u32::MAX`.

### 4.2 [LOW] No Rust benchmarks - IGNORE, NOT IMPORTANT

**Files:** N/A

Only `stress_test_benchmark.py` exists. No `criterion` benchmarks.

**Recommendation:** Add benchmarks for entity lifecycle, component iteration
(seq/par), archetype migration, and scheduler graph building.

### 4.3 [LOW] `Or` filter only for tuples

**File:** `src/query/filter.rs`

`Or` wraps a tuple (`Or<(A, B)>`) - users cannot write `Or<Or<(A, B)>, C>`.

**Recommendation:** Implement recursive `Or<A, B>` or extend macro arities to 8+.

### 4.4 [LOW] `main.rs` CLI picker - IGNORE, NOT IMPORTANT

**File:** `src/main.rs`

Examples embedded as modules with CLI picker prevent `cargo run --example`.

**Recommendation:** Move examples to `examples/` directory.

### 4.5 [LOW] `run_systems_sequential` scans disabled systems - IGNORE, NOT IMPORTANT

**File:** `src/engine.rs`

Iterates all registered systems, skipping disabled ones via `continue`.

**Recommendation:** Maintain a separate `Vec<usize>` of enabled indices.

### 4.7 [LOW] No `Cargo.lock` committed 

**Files:** `.gitignore` / missing `Cargo.lock`

Project has a `[[bin]]` target - `Cargo.lock` should be versioned.

**Recommendation:** Commit `Cargo.lock`.

---

## 5. Low Issues

### 5.1 [LOW] Missing `#[must_use]` on query/resource methods

**Files:** `src/query/query.rs`, `src/query/resource.rs`, `src/resource.rs`, `src/archetype.rs`

Methods returning `Option`, `bool`, or `usize` that should warn if ignored:

| Method | File | Returns |
|--------|------|---------|
| `Query::first()` | query.rs | `Option` |
| `Query::is_empty()` | query.rs | `bool` |
| `Query::entity_count()` | query.rs | `usize` |
| `Res::get()` | query/resource.rs | `Option` |
| `ResMut::get()` | query/resource.rs | `Option` |
| `ResMut::get_mut()` | query/resource.rs | `Option` |
| `ResHandle::get()` | resource.rs | `Option` |
| `ResHandle::get_mut()` | resource.rs | `Option` |
| `ResHandle::exists()` | resource.rs | `bool` |
| `Archetype::len()` | archetype.rs | `usize` |
| `Archetype::is_empty()` | archetype.rs | `bool` |
| `ParQueryIter::entity_count()` | query/iter.rs | `usize` |

### 5.2 [LOW] Duplicate `len()` / `entity_count()` on `Archetype`

**File:** `src/archetype.rs`

Both methods do the same thing - `len()` returns `self.entities.len()` and
`entity_count()` is an alias for `len()`. Keep one, mark the other deprecated.

### 5.5 [LOW] `ParQueryIter` methods missing doc comments

**File:** `src/query/iter.rs`

`num_threads()`, `entity_count()`, `tracked()` have no doc comments.

### 5.6 [LOW] `CommandQueue::is_empty()` missing doc comment

**File:** `src/commands.rs`

Public method with no documentation.

### 5.7 [LOW] Potential Cartesian-product blowup in filter AND logic

**File:** `src/query/filter.rs`

`and_filter_pairs()` builds a Cartesian product of inner filter pairs.
Deeply nested `Or<Or<...>>` filters could theoretically produce many pairs,
though this is rare in practice.

### 5.8 [LOW] No unified `EcsError` enum

**Files:** various

`CommandError`, `AddComponentError`, `RemoveComponentError`, `BuildError`
are separate types. A unified error type would simplify error handling.

### 5.9 [LOW] No CI, linting config, or miri tests

**Files:** N/A

No `.cargo/config.toml`, no `[lints.clippy]` in `Cargo.toml`, no GitHub
Actions workflow, no miri CI job.

### 5.10 [LOW] No `GUIDE.md` or getting-started walkthrough

**Files:** N/A

The codebase has `ARCHITECTURE.md` but no user-facing guide.

### 5.11 [LOW] `TickFilterState::ticks_at()` SAFETY comment could be stronger

**File:** `src/query/filter.rs`

The `unsafe fn ticks_at()` uses `unwrap_unchecked()` and has a `debug_assert!`
but the SAFETY comment only says "Caller must ensure `is_present()` returned
true" - it doesn't explain WHY the caller can guarantee this.

---

## 6. Performance Considerations

- **SoA layout** - well-implemented, components contiguous by type within archetypes
- **Query mask caching** - `target_mask` and `filter_pairs` computed once per `Query::new()`
- **ComponentMask conflict detection** - O(1) via bitwise AND
- **Hot-path allocations eliminated** - most clones removed (5.2-5.5); remaining:
  unavoidable `new_component_ids` allocation in `add_component`
- **Parallel iteration** - adaptive fallback for small workloads (`num_threads × 256`)
- **Two-level par_iter** - archetypes × rows; acceptable with adaptive threshold
- **HashMap archetype store** - see 3.1; switching to `Vec` would improve cache locality
- **Change-detection ticks** - `Mut<T>` bumps on every write (Bevy-compatible)

---
