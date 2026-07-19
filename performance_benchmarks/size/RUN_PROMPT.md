You are an AI coding agent performing iterative binary-size optimization on a Rust project.

## Your task

Identify, measure, and reduce binary size in the release build of this project.
Work iteratively: each optimization must be measured before and after using the
binary size measurement pipeline. Do not land an optimization without evidence.

## Workflow (repeat for each optimization)

### Phase 1 — Establish baseline

```bash
python performance_benchmarks/size/binary_size_measurement_pipeline.py doctor

python performance_benchmarks/size/binary_size_measurement_pipeline.py baseline \
  -n before \
  --profile release
```

Capture the report:

```bash
type performance_benchmarks\cache\artifacts\binary_size\baselines\before\report.md
```

Also collect supplementary data:

```bash
# Full symbol list sorted by size
cargo bloat --release -n 50 > before_bloat.txt

# Per-crate breakdown with more detail
cargo bloat --release --crates -n 30 > before_crates.txt

# Dependency tree with features
cargo tree -e features > before_features.txt

# Duplicate dependency check
cargo tree -d > before_duplicates.txt

# LLVM IR lines (monomorphization analysis) — requires cargo-llvm-lines
cargo llvm-lines --release > before_llvm_lines.txt 2>&1

# Linker map file (Linux)
# RUSTFLAGS="-C link-arg=-Wl,-Map=before_map.txt" cargo build --release
```

### Phase 2 — Profile and identify offenders

Inspect the baseline:

```bash
python performance_benchmarks/size/binary_size_measurement_pipeline.py analyze --run before
```

Read the generated report:

```bash
type performance_benchmarks\cache\artifacts\binary_size\baselines\before\report.md
```

**What to look for in the data:**

- **`binary_size_bytes`** — total on-disk size. The headline number.
- **`compressed_size_bytes`** — gzip-compressed size. Matters for distribution.
- **Sections (`.text`, `.data`, `.bss`)** — which section dominates? `.text` > 80% is normal for Rust; large `.data` suggests embedded blobs or many static strings.
- **Per-crate breakdown** — which crate (after `std` and your own code) contributes the most? Is it pulling its weight?
- **Largest symbols** — single functions over 15 KB are worth inspecting. Are they example code, format machinery, or legitimate hot paths?
- **`dependency_count` / `duplicate_dep_count`** — duplicate dependencies double the size contribution. Fix them first.
- **Compression ratio** — low compression ratio (<30%) suggests the binary is already dense code; high ratio (>60%) suggests lots of repetitive or uncompressed data (strings, debug info if unstripped).
- **Compare with BINARY_SIZE_101.md benchmarks** — a small-to-medium Rust CLI/library should be 200–800 KB after tuning. Above 2 MB nearly always has low-hanging fruit.

**Classify your findings honestly:**

| Classification | Meaning |
|---|---|
| **Measured** | Directly observed in binary size data |
| **Derived** | Computed from measurements (e.g., compression ratio) |
| **Static analysis** | Observed in source or Cargo.toml without runtime data |
| **Hypothesis** | Plausible explanation requiring an experiment to confirm |

Do not declare that a crate is a size offender merely because it appears in
the dependency list. Use `cargo bloat --crates` to see actual byte contribution
and `cargo tree -i <crate>` to understand why it is included.

### Phase 3 — Formulate a hypothesis

Based on the measurements, state exactly:

- **What you believe is happening** (e.g. "the `clap` derive macro generates
  200 KB of argument-parsing code; switching to `lexopt` would save ~190 KB")
- **Which metric would change and by how much** (e.g. "`.text` section should
  decrease from 850 KB to ~660 KB")
- **What code change would test this** (specific Cargo.toml line, source file, or feature flag)
- **What trade-off the change introduces** (e.g. "loses automatic `--help`
  generation" or "adds 3 seconds to link time")
- **Which BINARY_SIZE_101.md principle this relates to** (reference the section)

### Phase 4 — Implement and measure

Make the minimal change. Then:

```bash
python performance_benchmarks/size/binary_size_measurement_pipeline.py measure \
  -n after_<descriptive_suffix> \
  -c before \
  --profile release
```

Read the comparison verdict:

```bash
python performance_benchmarks/size/binary_size_measurement_pipeline.py compare \
  --baseline before --candidate after_<suffix>
```

Also compare detailed breakdowns:

```bash
# Compare per-crate contributions
diff <(python -c "import json; d=json.load(open('performance_benchmarks/size/artifacts/binary_size/baselines/before/summary.json')); [print(f'{k}: {v}') for k,v in sorted(d['measurement']['per_crate_bytes'].items())]") \
     <(python -c "import json; d=json.load(open('performance_benchmarks/size/artifacts/binary_size/runs/after_<suffix>/summary.json')); [print(f'{k}: {v}') for k,v in sorted(d['measurement']['per_crate_bytes'].items())]")
```

If applicable, also run the cache measurement pipeline to check for performance regressions:

```bash
python performance_benchmarks/size/cache_measurement_pipeline.py measure \
  -n after_<suffix>_cache \
  -c before \
  --bench query_iteration --bench-filter query_iter_unfiltered/100000
```

### Phase 5 — Interpret and decide

- **If verdict is IMPROVED** and the improvement matches your hypothesis:
  keep the change, set a new baseline, move to the next optimization area.

- **If verdict is UNCHANGED** but you expected improvement: your hypothesis
  was wrong. Re-examine with `cargo bloat` and `cargo tree`. The compiler or
  linker may have already optimised what you attempted. Revert and move on.

- **If verdict is INCONCLUSIVE** (small change < 1%): the change may be
  below the noise floor. Check if the binary SHA-256 changed (it should
  have). If the binary is unchanged, the build may not be picking up your
  change. If slightly changed but below 1%, decide whether the code
  simplification is worth it anyway.

- **If verdict is REGRESSED**: revert immediately. The change made the binary
  larger. Understand why before attempting a different approach.

- **If verdict is NOT_COMPARABLE**: you changed the build configuration,
  compiler version, or target between runs. Do not compare incomparable data.

### Phase 6 — Document

For each optimization that lands, add a concise comment near the relevant
configuration or source:

```toml
# Binary-size optimization: disabled regex Unicode support.
# Before: 850 KB (.text 720 KB) — rustc 1.95, x86_64-unknown-linux-gnu
# After:  650 KB (.text 530 KB)
# Trade-off: ASCII-only pattern matching. Acceptable for our use case.
```

And record the pass in a running log at
`performance_benchmarks/size/BINARY_SIZE_OPTIMIZATION_LOG.md`.

## Areas to investigate (in priority order)

These are starting points based on BINARY_SIZE_101.md principles. Measure
before changing anything.

### Profile-level changes (lowest risk, often biggest wins)

1. **`opt-level = "s"` or `"z"`** — typically 5–25% size reduction with
   minimal (2–5%) or moderate (5–15%) speed impact. Start with `"s"` and
   measure.

2. **`lto = "fat"`** — additional 5–15% over thin LTO. Significantly longer
   link times but better dead-code elimination and identical code folding.

3. **`panic = "abort"`** — saves 5–15% by removing unwind tables. Safe for
   most applications unless you use `catch_unwind`.

4. **`strip = true`** — removes debug info and symbol table. Essential for
   distribution. Use a separate profile for profiling builds.

### Dependency-level changes

5. **Audit default features** — `cargo tree -e features` shows active features.
   Disable `unicode` in `regex`, `full` in `tokio`, `derive` in `serde` if
   hand-writing impls.

6. **Replace heavy dependencies with lighter alternatives**:
   - `clap` (50–200 KB) → `bpaf` (20–50 KB) or `lexopt` (5–10 KB)
   - `reqwest` (500 KB+) → `ureq` (50–100 KB) for simple HTTP
   - `serde_json` → `serde_json_core` (no_std) or `nanoserde`
   - `jemalloc`/`mimalloc` → system allocator (if you don't need custom)

7. **Remove duplicate dependency versions** — `cargo tree -d`. Fix with
   `[patch]` or by updating transitive deps.

### Code-level changes

8. **Reduce monomorphization** — `cargo llvm-lines` shows which generic
   functions generate the most IR. Extract non-generic inner functions
   from heavily-instantiated generics.

9. **De-duplicate identical closures** — extract repeated closure bodies
   into named functions. Each closure has a unique type.

10. **Move example/demo code from `main.rs` to `examples/`** — the binary
    only needs production code. `cargo bloat` will show if `main` is
    unexpectedly large.

11. **Replace `format!`/`println!` chains in cold paths** with simpler
    alternatives or feature-gate diagnostic output.

12. **Audit `#[inline(always)]` usage** — over-inlining duplicates code.
    Try removing the attribute and letting LLVM decide.

13. **Use `dyn Trait` instead of generics for cold code paths** — dynamic
    dispatch is smaller but slower. Appropriate for error handling,
    configuration loading, and rare edge cases.

## Rules

- **Never optimize without measuring first.** Static analysis is not evidence.
- **Change one thing at a time.** If you change two things and the result
  improves, you don't know which change helped.
- **Revert immediately on regression.** Do not try to "fix" a regression by
  adding more changes on top.
- **Prefer simpler code.** If two approaches have equal binary size, choose
  the simpler one.
- **Document every landed optimization** with before/after numbers and the
  trade-off.
- **Stop when improvements are within noise** (< 1% binary size change).
- **Do not modify the measurement pipeline itself** as part of optimization
  work. The pipeline is the measuring stick — changing it invalidates
  comparisons.
- **Measure compressed size too.** Some optimisations (string deduplication,
  identical code folding) show more benefit in compressed size than
  uncompressed.
- **Check performance impact.** A smaller binary that runs 2× slower is a bad
  trade-off for most applications. Use the cache measurement pipeline or
  Criterion benchmarks to verify.
- **The goal is a smaller binary without unacceptable trade-offs.** Sometimes
  the smallest-possible binary requires nightly features or obscure
  configurations that harm portability.

## Quick-reference: size-optimization checklist

```
□ Establish baseline                  binary_size_measurement_pipeline.py baseline -n before
□ Read the report                     type ...\baselines\before\report.md
□ Check per-crate breakdown           cargo bloat --release --crates -n 30
□ Check largest symbols               cargo bloat --release -n 30
□ Check dependency features           cargo tree -e features
□ Check duplicate deps                cargo tree -d
□ Identify top 3 offenders            (largest crate, largest symbol, most surprising dep)
□ Form hypothesis                     "If I do X, metric Y should change by Z"
□ Implement ONE change                (edit Cargo.toml or one source file)
□ Measure                            binary_size_measurement_pipeline.py measure -n after_X -c before
□ Compare                            binary_size_measurement_pipeline.py compare --baseline before --candidate after_X
□ Check performance                   (run benchmarks if hot-path code changed)
□ Interpret verdict                   (IMPROVED → keep | UNCHANGED → revert | REGRESSED → revert immediately)
□ Document                           (add comment + update optimization log)
```

## Deliverables expected

For each successfully landed optimization:

1. The configuration or code change (in `Cargo.toml` or `src/`)
2. The `before` baseline name and `after_<suffix>` candidate name
3. The comparison output showing the improvement
4. A source comment with before/after metrics
5. An entry in `performance_benchmarks/size/BINARY_SIZE_OPTIMIZATION_LOG.md`

For each rejected hypothesis:

1. A brief note explaining what was attempted, what was expected, what was
   measured, and why it was reverted
2. An entry in the optimization log so future work does not repeat the attempt

Begin by running `doctor` and creating the `before` baseline.
