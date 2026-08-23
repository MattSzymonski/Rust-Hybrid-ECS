# devops

All development tooling for Rust-Hybrid-ECS.

```
devops/
├── benchmarks/   things that measure   - each runs standalone
├── tests/        things that assert    - each runs standalone
├── core/         shared code both import
├── pill_lab/     the web app and the CLI that drives the scripts
├── ci_cd/        container plus the shell entry points CI calls
└── docs/         the rustdoc -> Markdown documentation pipeline
```

The shell scripts in `ci_cd/` hold no logic of their own: each one locates the
repository root and forwards its arguments to a Python script in `tests/`,
`benchmarks/` or `docs/`. That way a check behaves the same whether CI runs
the wrapper or a developer runs the Python directly.

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
python devops/tests/test_module_project_auto_reload.py
python devops/tests/test_csharp_bridge.py

# Static, build and CI checks (no host launch)
python devops/tests/test_coding_standards.py
python devops/tests/test_examples.py
python devops/tests/test_basic.py --list
```

`test_coding_standards.py` lints every `.rs` file against the Pill comment and
layout rules, `test_examples.py` builds each example in release and reports
artifact sizes, and `test_basic.py` runs the CI fast checks (fmt, clippy,
launcher builds, WASM size budget, benchmark). All three were ported from the
shell scripts that now just invoke them. See `tests/README.md` for what each
suite pins down.

## core/

Shared code, imported by everything above. Nothing here executes a measurement
or asserts anything.

| Module | Role |
| --- | --- |
| `paths.py` | devops/repository layout, plus `ensure_devops_on_path()` |
| `environment.py` | git / OS / CPU / toolchain metadata |
| `storage.py` | measurement files and the `index.json` manifest |
| `compare.py` | baseline comparison behind `pill_lab.py compare` |
| `criterion.py` | Criterion output parsing |
| `cargo_timings.py` | cargo `--timings` report parsing |
| `cli.py` | the argument-parser and store/report contract benchmarks share |
| `test_report.py` | pass/fail/skip tally, summary block and size reports |
| `suite_common.py` | host process plumbing: log tokens, output monitor, backup/restore |
| `common.sh` | the shell equivalent, sourced by the ci_cd scripts |

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

The CI container and the shell entry points CI calls. Each is a thin wrapper
around a Python script elsewhere in `devops/`.

Every script here is a thin wrapper: it locates the repository root and
forwards its arguments to a Python script elsewhere in `devops/`. None of them
holds check logic any more.

| Wrapper | Delegates to |
| --- | --- |
| `run_hot_reload_tests.sh` | the four suites in `tests/` |
| `run_basic_tests.sh` | `tests/test_basic.py` |
| `run_coding_standards_test.sh` | `tests/test_coding_standards.py` |
| `run_examples_tests.sh` | `tests/test_examples.py` |
| `generate_documentation.sh` | `docs/generate_documentation_markdown.py` |

```powershell
bash devops/ci_cd/run_hot_reload_tests.sh       # 4 suites
bash devops/ci_cd/run_basic_tests.sh code_linting
bash devops/ci_cd/run_coding_standards_test.sh --root modules
bash devops/ci_cd/run_examples_tests.sh --list
bash devops/ci_cd/generate_documentation.sh
```

Arguments pass straight through, so `run_coding_standards_test.sh --root
modules` and `python devops/tests/test_coding_standards.py --root modules` do
the same thing. `core/common.sh` is no longer sourced by any of them; it
remains for any future shell script that needs its helpers.
