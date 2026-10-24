# devops

All development tooling for Rust-Hybrid-ECS.

```
devops/
├── benchmarks/   things that measure   - each runs standalone
├── tests/        things that assert    - each runs standalone
├── core/         shared code both import
├── pill_lab/     the web app and the CLI that drives the scripts
├── ci_cd/        the CI gate (verify.py), release builds and shell entry points
└── docs/         the rustdoc -> Markdown documentation pipeline
```

`ci_cd/verify.py` is the CI gate: CI runs its steps, and a local run is the
same gate. The shell scripts in `ci_cd/` hold no check logic of their own; each
locates the repository root and runs Python scripts in `tests/` or `docs/`.

The dependency direction is one-way: `benchmarks/` and `tests/` import
`core/`, and `pill_lab/` imports both. Nothing in `core/` imports upward.

## benchmarks/

Performance measurement. Every script reports numbers; none of them assert.
Each is directly runnable, from any working directory:

```powershell
python devops/benchmarks/engine.py --bench minimal --quick
python devops/benchmarks/hot_reload.py --iterations 5
python devops/benchmarks/cold_start.py --clean-scope none
python devops/benchmarks/hot_reload_harness.py --iterations 5 --csv perf.csv
```

Each writes a JSON measurement into `pill_lab/measurements/` and accepts
`--json` for a machine-readable result line. `hot_reload_harness.py` is the
host-driving harness `hot_reload.py` uses; it stays separately runnable
because it exposes raw flags (`--max-wall-ms`, `--csv`) the wrapper does not.

Every script also appears as a `pill_lab.py` subcommand. That is not a second
implementation: the CLI mounts the script's own argument parser and calls its
`execute()`, so both paths accept identical flags and run identical code.

## tests/

Pass/fail suites for the hot-reload and persistence architecture, plus the
`project/` Rust fixture crate they hot-reload against. Each runs standalone:

```powershell
# Host-driving regression suites
python devops/tests/test_hot_reload_suite.py
python devops/tests/test_hot_reload_migration.py --cycles 2
python devops/tests/test_csharp_bridge.py
python devops/tests/test_shipping_smoke.py
python devops/tests/test_web_smoke.py          # browser build in headless Chrome/Edge

# Static, build and CI checks (no host launch)
python devops/tests/test_coding_standards.py
python devops/tests/test_examples.py
python devops/ci_cd/verify.py --quick           # the CI gate's inner loop
```

`test_coding_standards.py` lints every `.rs` file against the Pill comment and
layout rules, and `test_examples.py` builds each example in release and reports
artifact sizes. Formatting, clippy and the cargo test lanes are `verify.py`
steps; those two lanes run through `cargo-nextest` (`modules/.config/nextest.toml`)
when it is installed and through `cargo test` when it is not, and always run
the doctests with `cargo test --doc`. See `tests/README.md` for what each suite
pins down.

`tools/build_web.py` builds a Rust project for the browser (bundle and web app
generation, `wasm-pack`, the page from `tools/web/index.html`); the web smoke
test drives it. `tools/wasm_size_report.py` explains a web build's size: the
shipped module's sections, its gzip size, the embedded asset pack, and (with
`twiggy` on PATH) the code by crate - engine, project, standard library and
third-party families - from the binary wasm-opt has not stripped yet.

## core/

Shared code, imported by everything above. Nothing here executes a measurement
or asserts anything.

| Module             | Role                                                                 |
| ------------------ | -------------------------------------------------------------------- |
| `paths.py`         | devops/repository layout, plus `ensure_devops_on_path()`             |
| `environment.py`   | git / OS / CPU / toolchain metadata                                  |
| `storage.py`       | measurement files and the `index.json` manifest                      |
| `compare.py`       | baseline comparison behind `pill_lab.py compare`                     |
| `criterion.py`     | Criterion output parsing                                             |
| `cargo_timings.py` | cargo `--timings` report parsing                                     |
| `cli.py`           | the argument-parser and store/report contract benchmarks share       |
| `test_report.py`   | pass/fail/skip tally, summary block, size reports, cargo test totals |
| `suite_common.py`  | host process plumbing: log tokens, output monitor, backup/restore    |

A standalone script reaches it by putting `devops/` on `sys.path` first:

```python
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from core import criterion
```

## pill_lab/

The web app for browsing and comparing stored measurements, plus `pill_lab.py`,
the CLI that orchestrates the scripts above. It holds no measurement logic of
its own. See `pill_lab/README.md`.

```powershell
python devops/pill_lab/pill_lab.py all       # run every benchmark
python devops/pill_lab/pill_lab.py compare   # what changed
python devops/pill_lab/pill_lab.py serve     # browse the history
```

## ci_cd/

The CI gate, the release build and the shell entry points.

| File                        | What it is                                                                       |
| --------------------------- | -------------------------------------------------------------------------------- |
| `verify.py`                 | the CI gate: every check CI runs, as named steps (`--list`, `--only`, `--quick`) |
| `check_relevant.py`         | runs only the checks a change set needs, by repository area                      |
| `build_release.py` / `.sh`  | release builds (they clear `RUSTFLAGS`; a plain `cargo build --release` fails)   |
| `run_hot_reload_tests.sh`   | the hot-reload regression net: a fixed set of suites in `tests/`                 |
| `generate_documentation.sh` | `docs/generate_documentation_markdown.py`                                        |

`check_relevant.py` is the loop to use after a change: it reads the working
tree's change set (or `--paths`), matches every file against a per-area rule
table, and runs just that area's checks - formatting and the coding-standards
lint for Rust changes, the crate's unit tests, the named `tests/` suites and
the few `verify.py` steps that guard the area. An editor suite gets the
editor build (the uplift) prepended; a file no rule recognizes falls back to
`verify.py --quick`, so nothing passes unchecked by accident. The full gate
and the tier runs stay for stage boundaries and milestones.

The gate itself carries the editor end to end: its `editor` step runs the
host-tier revision suite, builds `pill_editor` (the uplift), and then
runs `test_editor_egui.py` - that order is required, because the revision
suite's narrowed build re-splits the shared DLLs and the editor launches a
pre-built binary. It is the slowest step - about fifteen minutes on a warm
cache - and it never runs under `--quick`.

```powershell
python devops/ci_cd/check_relevant.py --dry-run   # what would run, and why
python devops/ci_cd/check_relevant.py             # run only that
python devops/ci_cd/check_relevant.py --list-rules
python devops/ci_cd/check_relevant.py --base origin/main
```

```powershell
python devops/ci_cd/verify.py --list
bash devops/ci_cd/run_hot_reload_tests.sh
bash devops/ci_cd/generate_documentation.sh
```
