# Pill Lab

One workflow for measuring, storing and browsing Rust-Hybrid-ECS performance.

Python runs the measurements and writes versioned JSON. A TypeScript + Vite
frontend renders it. Nothing generates HTML from Python any more.

```
devops/
├── benchmarks/            standalone measurement scripts
│   ├── engine.py
│   ├── hot_reload.py
│   ├── hot_reload_harness.py
│   └── cold_start.py
├── tests/                 standalone pass/fail suites + fixture crate
├── core/                  shared code every script imports
├── ci_cd/                 container, shell orchestrators, doc generation
└── pill_lab/              this directory: the web app and its CLI
    ├── pill_lab.py        CLI: mounts the benchmarks, compares, serves
    ├── measurements/      measurement history (JSON) + index.json
    ├── src/               frontend (TypeScript, no framework)
    ├── index.html
    ├── vite.config.ts
    └── package.json
```

Pill Lab owns presentation and orchestration only. The measuring lives in
`devops/benchmarks/`, where every script also runs on its own from a console;
`pill_lab.py` mounts each one as a subcommand by borrowing its argument
parser, so the two invocation paths cannot drift apart:

```powershell
python devops/benchmarks/engine.py --bench minimal --quick   # standalone
python devops/pill_lab/pill_lab.py engine --bench minimal --quick   # same thing
```

## Quick start

```powershell
# Run one category
python devops/pill_lab/pill_lab.py engine
python devops/pill_lab/pill_lab.py hot-reload
python devops/pill_lab/pill_lab.py cold-start

# Run everything
python devops/pill_lab/pill_lab.py all

# View the results (installs npm dependencies on first run)
python devops/pill_lab/pill_lab.py serve --open
```

`serve` starts Vite on <http://localhost:5180/>. Measurements written while the
server is running appear on the next page reload.

Other commands: `compare` (diff two runs in the terminal), `list` (history),
`reindex` (rebuild `measurements/index.json` from disk), `build` (static
`dist/`). Every command has `--help`.

## The two tabs

Pill Lab has two independent tabs, switchable at the top of the page and
linkable via the URL hash (`#tests` for the Tests tab):

- **Benchmarks** — same two-column shell as Tests. The sidebar holds the
  category picker and, below it, only the **selected category's** benchmark run
  row (status dot + label + phase). The main column carries, top to bottom: a
  **Run button in the top-right corner** with state chips, a live **terminal**
  (pinned to the bottom), the **Measurement / Compare against** pickers, and
  the report below. Running `pill_lab.py <category>` refreshes the manifest
  when it finishes.
- **Tests** — the same two-column shell as Benchmarks. The left sidebar lists
  every suite from `devops/tests/` with a status dot; click one to see its
  full name, description and current state in the main column, with the
  per-step **spinning circles** (green check on pass, red cross on fail) and a
  live **terminal** pinned to the bottom. **Run all** queues the suites one at
  a time.

**Every run started from the UI is also a real console run.** The dev server
spawns the Python process with its stdout piped, but the output is *teed* to a
log file that a **visible console window tails live** (Windows), so what you
see is exactly what a normal `python devops/...` invocation prints — colors
included — and the window stays open after the run so the final output stays
readable. The browser stream (spinners, live log) works in parallel.

Running tests and benchmarks needs the dev server (`serve`): a Vite plugin
exposes `GET /api/tests` (suite list), `GET /api/tests/run?name=<file>`,
`GET /api/benchmarks` (category list) and `GET /api/benchmarks/run?category=`,
all as a Server-Sent-Events stream of the output; the child process is killed
if the browser disconnects. The static `build` output has no backend, so the
Tests tab there shows a "start the dev server" hint.

## Scripted and AI-agent use

Everything the UI answers, the CLI answers too - no browser required. Every
command that produces data takes `--json` and writes one machine-readable
object to stdout, so nothing has to be scraped out of the terminal log.

The measure -> change -> measure -> compare loop:

```bash
# 1. Baseline. --json prints {"measurement": "engine/engine_....json", ...}
python devops/pill_lab/pill_lab.py engine --bench query_iteration --json

# 2. Make the code change.

# 3. Measure again.
python devops/pill_lab/pill_lab.py engine --bench query_iteration --json

# 4. What changed? Defaults to newest vs the one before it.
python devops/pill_lab/pill_lab.py compare engine
python devops/pill_lab/pill_lab.py compare engine --json      # structured
python devops/pill_lab/pill_lab.py compare --format markdown  # for a PR
```

Run selectors accept `latest`, `previous`, a zero-based index (newest first),
or any unique substring of a filename or timestamp:

```bash
python devops/pill_lab/pill_lab.py compare engine --current latest --baseline 3
python devops/pill_lab/pill_lab.py compare engine --baseline 03-32-40
```

`compare` with no category compares every category that has at least two runs.

**Exit codes.** `0` success, `1` the command failed, `2` the regression gate
tripped:

```bash
python devops/pill_lab/pill_lab.py compare engine --fail-on-regression
```

**Signal versus noise.** This matters more than any other setting. A change is
only called a regression when it exceeds 2% *and* is large compared to the
run-to-run spread, and significance is refused outright below 5 samples per
side. That floor exists because it is easy to fool yourself: two back-to-back
runs of *identical* code with Criterion's `--quick` (2 samples) produced 23
"regressions" up to +229% on this workspace, purely from scheduling noise.

So for an A/B comparison:

- **Do not use `--quick`.** It is for smoke-testing the pipeline, not for
  deciding whether a change helped. Full Criterion runs collect 100 samples
  and support real significance testing.
- Narrow the scope with `--bench <target>` instead, to keep runs short.
- For `hot-reload`, pass `--iterations 5` or more; the default of 3 is below
  the significance floor.
- `--fail-on-regression` only trips on significant regressions. Add
  `--include-insignificant` to gate on any change past the threshold, and
  expect false positives if you do.

Each metric in the output carries its own verdict, so an agent never has to
infer it: `significant`, `within run-to-run spread`, or
`only N sample(s) - too few to judge significance`.

Hot-reload comparisons include the host's phase breakdown as separate metrics
(`cascade_total / build`, `module_reload / load`, ...), so the output says
*which part* of a reload moved, not just that it did. Cold-start comparisons
include each build's compiled-unit count alongside its duration, so "the build
got slower" can be separated from "the build compiled more crates".

## Categories

### Engine Performance

Runs the Criterion benchmarks declared in `modules/pill_engine/Cargo.toml` and
normalizes `modules/target/criterion/` into JSON: per-benchmark mean / median /
std-dev / slope with 95% confidence intervals, min / max, outlier flags,
throughput, raw samples for the charts, and Criterion's own change versus its
previous run.

```powershell
python devops/pill_lab/pill_lab.py engine                          # everything
python devops/pill_lab/pill_lab.py engine --bench minimal --quick  # fast check
python devops/pill_lab/pill_lab.py engine --skip-run               # capture existing output
```

**Profile overrides.** A plain `cargo bench` currently fails in this workspace
on Windows: `modules/.cargo/config.toml` sets `-C prefer-dynamic`, which rustc
refuses to combine with the release profile's `lto = "fat"`, and that profile's
`panic = "abort"` conflicts with Criterion's unwinding harness. Pill Lab clears
`RUSTFLAGS` and passes `--config profile.release.panic="unwind"` **for its own
invocation only** - no file is modified. Opt out with `--no-profile-overrides`.

### Hot Reloading

Driven by `devops/benchmarks/hot_reload_harness.py` rather than by an invented
benchmark. The harness launches the standalone host, applies real reversible
source edits and times each reload from the edit write to the host's completion
token (so watcher detection latency is included). Every file it touches is
backed up and restored.

The harness is also runnable on its own, which exposes flags Pill Lab does not
surface (`--max-wall-ms`, `--csv`):

```powershell
python devops/benchmarks/hot_reload_harness.py --iterations 5 --csv perf.csv
```

It imports `devops/core/suite_common.py` for the host process plumbing (log
tokens, the output monitor, backup/restore) - the same module the functional
suites in `devops/tests/` use.

| Case | Edit | Measured to |
| --- | --- | --- |
| `module_reload` | `pill_spline` constant | `optional module hot reload complete` |
| `cascade_total` | same edit | the cascaded `[analytics] reload project` |
| `project_reload` | `examples/project_rs` constant | project reload analytics |
| `csharp_reload` | `examples/project_cs` probe | `C# hot reload complete` |

Native cases also carry the host's own phase split (build / stage / load / init
/ migrate) and the crates cargo rebuilt. The C# path emits no analytics line,
so it is wall time only - stated as such in the UI rather than padded.

```powershell
python devops/pill_lab/pill_lab.py hot-reload --iterations 5
python devops/pill_lab/pill_lab.py hot-reload --native-only   # no .NET SDK needed
```

### Cold Start

Four separate concepts, never conflated:

| Case | What it is |
| --- | --- |
| `clean_check` / `clean_build` | after a targeted clean of the workspace's own packages |
| `incremental_check` / `incremental_build` | after an mtime bump of `pill_engine/src/lib.rs` |
| `startup_cold` / `startup_warm` | host launch to "Entering project loop", with modules to build and on the up-to-date fast path |
| `engine_init` | the `pill_engine` smoke binary end to end (spawn + `Engine::new` + print) |

Build cases attach Cargo's own `--timings` data - per-unit compile times parsed
from the report Cargo writes to `target/cargo-timings/`. Cargo's HTML is not
reused, only its numbers.

**Cleaning is explicit and targeted.** The default `--clean-scope packages`
runs `cargo clean --package <name>` for each workspace member (discovered from
`cargo metadata`, so a new module under `modules/optional/` is picked up
automatically) and leaves every third-party dependency compiled.
`--clean-scope workspace` removes the whole target directory and asks for
confirmation first. `--clean-scope none` skips the clean cases entirely.

```powershell
python devops/pill_lab/pill_lab.py cold-start
python devops/pill_lab/pill_lab.py cold-start --clean-scope none --skip-startup
```

## Measurement format

One JSON file per run, never overwritten:

```
measurements/engine/engine_2026-08-23_03-12-45.json
measurements/hot_reload/hot_reload_2026-08-23_03-15-08.json
measurements/cold_start/cold_start_2026-08-23_03-20-31.json
measurements/index.json
```

Every file shares one envelope; the category-specific result lives under
`measurement`:

```json
{
  "schema_version": 1,
  "category": "engine",
  "timestamp": "2026-08-23T03:12:45+02:00",
  "label": "cargo bench (all benchmarks)",
  "tool": { "name": "pill_lab", "version": "1.0.0" },
  "git": { "commit": "...", "branch": "...", "dirty": false },
  "environment": { "os": "...", "cpu": "...", "rustc": "...", "cargo": "..." },
  "command": { "argv": ["..."], "cwd": "...", "duration_seconds": 92.2 },
  "notes": ["..."],
  "measurement": {}
}
```

`index.json` is regenerated from the directory contents after every run, so it
can never drift: delete a measurement file and it disappears from the UI on the
next `reindex`.

## Comparison semantics

Every metric Pill Lab compares is a duration, so **lower is better**: a
negative delta reads "faster", a positive one "slower". Differences under 2%
are reported as *no meaningful change* rather than dressed up as a result.

The same rules apply in the UI and in `pill_lab.py compare`; the threshold is
defined in `src/lib/compare.ts` and `devops/core/compare.py`, which name each
other and must be changed together. The CLI additionally reports a significance
verdict per metric (see the agent section above); the UI shows Criterion's
confidence intervals directly instead.

Engine Performance shows two independent change columns, deliberately labelled
apart:

- **Δ Criterion** - Criterion's own comparison with its immediately preceding
  run, stored in `target/criterion`.
- **Δ Baseline** - this run's mean against whichever stored Pill Lab
  measurement you selected as the baseline.

## Relationship to `gen_bench_report.py`

`modules/pill_engine/benches/reports/gen_bench_report.py` still works and still
produces its self-contained HTML report. Its Criterion parsing and machine
detection were removed and it now imports them from
`devops/core/`, so the repository has one benchmark parser
rather than two. Its HTML/CSS/JS generation stays where it is; Pill Lab's
frontend is the port of that presentation.
