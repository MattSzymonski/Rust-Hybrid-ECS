# Performance Testing Pipeline

## Overview

Two-tier testing approach:
1. **Criterion benchmarks** — micro-benchmarks for individual operations (query iteration, frame loop, archetype migration)
2. **Real-world frame tests** — run `tracy_live` example for thousands of frames, measure average FPS

---

## Quick Start

```powershell
# Run the full real-world test — 5×5 comparison
cd d:\Programming\Rust-Hybrid-ECS

# Old formula (hardcoded 6144 slice): 5 runs, 20K frames each
$env:ECS_SLICE_SIZE='6144'
for($i=1;$i-le5;$i++){
    python performance_benchmarks/performance/test_tracy_live.py 20000
}

# New formula (component-size clamping): 5 runs, 20K frames each
Remove-Item Env:ECS_SLICE_SIZE -EA 0
for($i=1;$i-le5;$i++){
    python performance_benchmarks/performance/test_tracy_live.py 20000
}
```

---

## Test Harness: `test_tracy_live.py`

Runs the `tracy_live` example for N frames and reports average FPS/frame time.

```
python performance_benchmarks/performance/test_tracy_live.py 20000
```

Output:
```
  2997 FPS  (total ~5994 frames)
  3045 FPS  (total ~12084 frames)
  ...

==================================================
Frames sampled: ~20000
FPS reports: 6
Average FPS: 2979.0
Average frame time: 335.7 us
FPS range: 2914 - 3045
==================================================
```

**How it works:**
- Builds `tracy_live` in release mode (cached after first build)
- Runs the example binary, parsing `"NNNN FPS | XXXXX entities"` lines
- Each report covers ~2 seconds of frames
- Terminates once total frames reach the target
- Reports mean FPS, mean frame time, and range

**Environment variables:**
- `ECS_SLICE_SIZE=N` — override the slice size (bypasses the auto formula)
- Unset → uses the default clamped formula

---

## Criterion Pipeline: `performance_measurement_pipeline.py`

For micro-benchmarks with statistical analysis and comparison reports.

### Key Commands

```powershell
# Check environment
python performance_benchmarks/performance/performance_measurement_pipeline.py doctor

# Create a baseline
$env:ECS_SLICE_SIZE='6144'
python performance_benchmarks/performance/performance_measurement_pipeline.py baseline -n my_baseline --bench frame_loop --bench-filter with_256B_component/500000 --force --repetitions 10

# Measure and compare
Remove-Item Env:ECS_SLICE_SIZE -EA 0
python performance_benchmarks/performance/performance_measurement_pipeline.py measure -n my_test -c my_baseline --bench frame_loop --bench-filter with_256B_component/500000 --force --repetitions 10

# Compare any two saved runs
python performance_benchmarks/performance/performance_measurement_pipeline.py compare --baseline my_baseline --candidate my_test

# List all saved runs
python performance_benchmarks/performance/performance_measurement_pipeline.py list
```

### Report Output

Each run saves to `performance_benchmarks/performance/artifacts/` with:
- `summary.json` — aggregated statistics
- `report.md` — human-readable report with raw sample data
- `metadata.json` — build info, git revision, CPU model
- `samples/*.json` — per-sample raw data

Example report:
```markdown
# Performance Measurement Report: my_test

**Timestamp:** 2026-07-19T13:23:30Z
**CPU:** Intel64 Family 6 Model 151 Stepping 2, GenuineIntel
**Rust:** rustc 1.95.0
**Profile:** bench
**Binary size:** 2194 KiB

## Timing
- Median: 7.0 ms | Min/Max: 6.0/7.0 ms | CV: 5.88%

## Raw Sample Data
| # | Wall Time | Success |
|---|---|---|
| 0 | 7.0 ms | OK |
| 1 | 7.0 ms | OK |
...

## Comparison: my_baseline -> my_test
**Verdict:** IMPROVED
| Metric | Delta |
|---|---|
| runtime_pct | -6.81% |
```

---

## Available Benchmarks

### `query_iteration` — Query micro-benchmarks

| Filter | What it tests |
|---|---|
| `256B/500000` | 256B MassiveData component, 500K entities |
| `256B/2000000` | 256B MassiveData component, 2M entities |
| `query_par_iter_unfiltered/500000` | Standard Position+Velocity, 500K entities |

### `frame_loop` — Full frame benchmarks

| Filter | What it tests |
|---|---|
| `standard/500000` | Standard components (8-16B), 500K entities, 3 systems |
| `with_256B_component/500000` | Mixed small+large, 500K entities, 4 systems |

### `tracy_live` — Real-world game loop

| Config | Entities | Systems | Components |
|---|---|---|---|
| Default | 30K | 7 | Position(8B), Velocity(8B), Health(4B), Mass(4B), GravityForce(8B), RenderData(256B), PhysicsData(128B) |

---

## Statistical Methodology

### Minimum Viable Test

1. Run each configuration **at least 5 times**
2. Report **mean FPS, range, and standard deviation**
3. Compare ranges — if they don't overlap, the result is significant

### Interpreting Results

| Δ FPS | Confidence | Action |
|---|---|---|
| < 2% | Within noise | No conclusion possible |
| 2–5% | Directional | Run more samples (10+) |
| 5–10% | Likely real | Verify on different machine |
| > 10% | Real | Confirmed with range non-overlap |

### Known Noise Sources (Windows)

- Background processes (antivirus, updates, browser)
- Thermal throttling (especially after multiple consecutive runs)
- OS scheduler (no core isolation)
- No hardware counters (`perf` unavailable)

### Reducing Noise

```powershell
# Kill distracting processes before testing
taskkill /F /IM tracy_live.exe 2>$null

# Wait between runs for thermal cooldown
Start-Sleep 5

# Use fixed CPU affinity (when taskset is available)
--cpu 0-7
```

---

## Adding a New Test Scenario

### Option A: Add to `tracy_live`

1. Edit `examples/tracy_live.rs` — add new components/systems
2. Build: `cargo build --example tracy_live --release`
3. Test: `python performance_benchmarks/performance/test_tracy_live.py 20000`

### Option B: Add a Criterion benchmark

1. Create `benches/my_benchmark.rs` with `criterion_group!` and `criterion_main!`
2. Add `[[bench]] name = "my_benchmark" harness = false` to `Cargo.toml`
3. Run: `python performance_benchmarks/performance/performance_measurement_pipeline.py baseline -n test --bench my_benchmark`

### Option C: Quick comparison

```powershell
# One-liner for quick A/B test
$env:ECS_SLICE_SIZE='6144'; python performance_benchmarks/performance/test_tracy_live.py 20000
# Note FPS, then:
Remove-Item Env:ECS_SLICE_SIZE -EA 0; python performance_benchmarks/performance/test_tracy_live.py 20000
```

---

## Current Test Matrix

Tests we've validated on this machine (i7-12700KF, 48 KiB L1D P-cores):

| Scenario | Old (6144) FPS | New (clamped) FPS | Δ |
|---|---|---|---|
| Small only (8-16B, 6 systems) | 2980 | 2854 | -4% (noise) |
| Mixed small+large, unclamped | 1680 | 1859 | +11% (outlier) |
| Mixed small+large, clamped floor/ceiling | 1583 | 1841 | +16% |
| Mixed, both 6144 fixed | 1713 | 1713 | 0% |
| Mixed, clamped with 6144 default | 1683 | 1876 | +12% |

**Current recommendation:** clamped formula with 6144 default — adapts to component size without external dependencies.

---

## Configuration Reference

The slice size formula (from `config.rs`):

```rust
pub fn default_entities_per_slice(bytes_per_entity: usize) -> usize {
    // ECS_SLICE_SIZE env var overrides everything
    if let Ok(n) = std::env::var("ECS_SLICE_SIZE")?.parse() { return n; }

    let default = ParallelProcessingConfig::DEFAULT_ITERATOR_SLICE_SIZE; // 6144
    let min = ParallelProcessingConfig::MINIMUM_SLICE_SIZE;              // 256

    // Scale inversely with component size, clamped between min and max
    (default * 8 / bytes_per_entity).clamp(min, default)
}
```

| Constant | Value | Meaning |
|---|---|---|
| `DEFAULT_ITERATOR_SLICE_SIZE` | 6144 | Ceiling — never bigger than this |
| `MINIMUM_SLICE_SIZE` | 256 | Floor — never smaller than this |
| `TARGET_ITERATOR_WORK_GROUP_DURATION` | 50_000 ns | Target work per rayon task |
| `SPLITTING_HINT_WINDOW` | 32 frames | EMA smoothing for timing feedback |
