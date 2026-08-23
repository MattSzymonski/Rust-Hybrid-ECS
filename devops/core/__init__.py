"""
Shared code for the devops tooling.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Everything the scripts under `devops/benchmarks/` and `devops/tests/`
    have in common. Nothing here executes a measurement or asserts anything;
    it is the layer they all import.

      * `paths`         - devops/repository layout resolution, plus
                          `ensure_devops_on_path()` which every standalone
                          script calls before importing this package.
      * `environment`   - git / OS / CPU / toolchain metadata collection.
      * `storage`       - measurement file naming, writing and the manifest.
      * `compare`       - baseline comparison behind `pill_lab.py compare`.
      * `criterion`     - Criterion output parsing (also used by the legacy
                          `gen_bench_report.py` HTML generator).
      * `cargo_timings` - cargo `--timings` HTML report -> structured data.
      * `suite_common`  - host process plumbing: log tokens, the output
                          monitor, atomic source edits, backup/restore. Shared
                          by the functional suites and the benchmark harness.
      * `common.sh`     - the shell equivalent, sourced by the ci_cd scripts.

    A standalone script reaches this package with:

        from pathlib import Path
        import sys
        sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
        from core.paths import ensure_devops_on_path

--- SCRIPT ---
"""

# The measurement JSON schema version. Bump when a stored field changes
# meaning; the frontend refuses to render a newer schema than it knows.
#
# v2 added `run_epoch` to each engine benchmark, so a comparison can tell a
# benchmark that was actually re-run from one whose Criterion output was merely
# carried over from an earlier invocation. Additive only: v1 files still load.
SCHEMA_VERSION = 2

# Pill Lab's own version, recorded in every measurement for traceability.
PILL_LAB_VERSION = "1.0.0"

CATEGORIES = ("engine", "hot_reload", "cold_start")

# Human-facing labels, kept next to the identifiers so the CLI and the
# frontend agree on the wording.
CATEGORY_LABELS = {
    "engine": "Engine Performance",
    "hot_reload": "Hot Reloading",
    "cold_start": "Cold Start",
}
