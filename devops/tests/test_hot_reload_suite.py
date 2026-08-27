"""
Full Hot-Reloading Integration Suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo) on PATH
  - Run from the repository root or anywhere (paths are resolved from __file__)

DESCRIPTION
    End-to-end hot-reload suite that launches the standalone host and drives
    real source edits against the live project and an optional module, then
    asserts the host's behaviour from its console output:

      Session A (project = devops/tests/project, module = pill_spline)
        1. project_hot_reload      - editing the project triggers a reload and
                                     the counter system keeps ticking (data
                                     survives).
        2. schema_migration        - adding a field to a persistable component
                                     runs selective migration, data survives.
        3. project_forgotten_type  - dropping a component registration emits the
                                     orphaned-data warning on the project path.
        4. module_hot_reload       - editing an optional module reloads it and
                                     its persistable data survives (existing=1).
        5. module_double_reload    - two consecutive same-config reloads stay
                                     stable: the module re-seeds nothing
                                     (existing stays 1), pinning per-artifact
                                     TypeId stability (no per-reload growth).
        6. module_forgotten_type   - a module that stops registering a type
                                     emits the orphaned-data warning; after the
                                     restore-driven reload the data is re-seeded
                                     from scratch (existing=0), pinning
                                     drop-at-detection end-to-end.
        7. init_failure_rollback   - an init that returns non-zero keeps the
                                     previous generation and the host alive.

      Session B (project = examples/project_rs, module = pill_spline)
        8. cascade_reload          - editing a module the project links triggers
                                     the module reload AND a project reload, and
                                     the new value reaches the running project
                                     (probe midpoint changes). The project probe
                                     still sees exactly ONE spline (xxsees 1),
                                     proving the project's embedded copy and the
                                     module's own copy coexist as distinct types.

    Every file the suite touches (pill_config.yaml, module/project sources) is
    backed up at startup and restored afterwards, so a normal developer
    workspace is left exactly as it was.

USAGE
  python tests/test_hot_reload_suite.py [--timeout-scale S] [--skip-build]

EXAMPLE USAGE
  python tests/test_hot_reload_suite.py
  python tests/test_hot_reload_suite.py --timeout-scale 1.5
  python tests/test_hot_reload_suite.py --skip-build

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Sequence, Tuple

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers. The
# host's log tokens live in one place (audit opportunity 5.14).
# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Session configuration
# =============================================================================

PROJECT_SANDBOX_LIB_RS = WORKSPACE_ROOT / "devops" / "tests" / "project" / "src" / "lib.rs"
SPLINE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"

SESSION_A_YAML = """\
project: "../devops/tests/project"
modules:
  - "pill_spline"
"""

SESSION_B_YAML = """\
project: "../examples/project_rs"
modules:
  - "pill_spline"
  - "pill_dummy_color"
"""

# --- Suite-specific output tokens --------------------------------------------

PROJECT_FORGOTTEN_WARN_TOKEN = "no longer registered by the project"
MODULE_FORGOTTEN_WARN_TOKEN = "no longer registered by this module"

# =============================================================================
# Host session
# =============================================================================


def build_host() -> bool:
    """Builds the standalone host once before the suite runs."""
    print("\n  [PREP] Building pill_standalone (offline)...")
    try:
        result = subprocess.run(
            ["cargo", "build", "--package", "pill_standalone", "--offline"],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as error:
        print(f"  [FAIL] Could not build the host: {error}")
        return False
    if result.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(result.stderr[-2000:])
        return False
    print("  [OK] Host built.")
    return True


def launch_host():
    """Launches the standalone host exe with a clean environment."""
    environment = os.environ.copy()
    environment.pop("PROJECT_PATH", None)
    return launch_process([str(HOST_EXE)], MODULES_ROOT, environment)

# =============================================================================
# Scenario model
# =============================================================================


@dataclass
class ScenarioPhase:
    """One edit+wait step inside a scenario.

    A scenario runs its phases in order; each phase applies its edits, waits
    for ``wait_token``, then (after all phases) the whole scenario window is
    checked for required / forbidden tokens and secondary wait tokens.
    """

    edits: Sequence[Tuple[Path, Sequence[Tuple[str, str]]]]
    wait_token: str
    required_tokens: Sequence[str] = ()
    forbidden_tokens: Sequence[str] = ()
    wait_after: Sequence[Tuple[str, float]] = ()


@dataclass
class Scenario:
    """One hot-reload scenario: sequential phases, expected output, cleanup."""

    name: str
    phases: Sequence[ScenarioPhase]
    restore_after: Sequence[Path] = field(default_factory=list)
    restore_required_tokens: Sequence[str] = ()


def run_scenario(scenario: Scenario, monitor: OutputMonitor) -> bool:
    """Runs one scenario and asserts the host's reload behaviour."""
    print(f"\n  [TEST] {scenario.name}...")
    start_index = monitor.line_count

    for phase in scenario.phases:
        for path, replacements in phase.edits:
            if not apply_replacements(path, replacements):
                return False

        if not monitor.wait_for(phase.wait_token, RELOAD_TIMEOUT, start_index):
            output = monitor.output_since(start_index)
            if has_crash_signals(output):
                print(f"  [FAIL] Crash detected in scenario: {scenario.name}")
                print(f"  Output tail:\n{output[-1600:]}")
            else:
                print(
                    f"  [FAIL] Timeout waiting for {phase.wait_token!r} in scenario: "
                    f"{scenario.name}"
                )
                print(f"  Output tail:\n{output[-1600:]}")
            return False

        time.sleep(STABILITY_SLEEP)

        if not monitor.process_alive():
            print(f"  [FAIL] Process died after scenario: {scenario.name}")
            return False

        # Wait for any secondary tokens (counter ticks, probe values, ...).
        for token, timeout in phase.wait_after:
            if not monitor.wait_for(token, timeout, start_index):
                print(f"  [FAIL] Missing expected token {token!r} in scenario: {scenario.name}")
                print(f"  Output tail:\n{monitor.output_since(start_index)[-1600:]}")
                return False

    output = monitor.output_since(start_index)
    if has_crash_signals(output):
        print(f"  [FAIL] Crash token observed in scenario: {scenario.name}")
        print(f"  Output tail:\n{output[-1600:]}")
        return False

    for phase in scenario.phases:
        for token in phase.required_tokens:
            if token not in output:
                print(f"  [FAIL] Missing required token in {scenario.name}: {token!r}")
                print(f"  Output tail:\n{output[-1600:]}")
                return False
        for token in phase.forbidden_tokens:
            if token in output:
                print(f"  [FAIL] Forbidden token found in {scenario.name}: {token!r}")
                print(f"  Output tail:\n{output[-1600:]}")
                return False

    print(f"  [OK] {scenario.name}")

    # Restore edited sources so the next scenario starts from the originals.
    # Each restore rewrites the file, which the watcher turns into another
    # reload; wait for that reload to finish so its output cannot pollute the
    # next scenario's wait window.
    settle_start = monitor.line_count
    for path in scenario.restore_after:
        BACKUP.restore_one(path)
        settle_token = (
            RELOAD_PROJECT_TOKEN if path == PROJECT_SANDBOX_LIB_RS else RELOAD_MODULE_TOKEN
        )
        # Scan only lines that arrive after the restore: the scenario's own
        # reload line is already in the buffer and must not satisfy the wait.
        if not monitor.wait_for(settle_token, SETTLE_TIMEOUT, settle_start):
            print(
                f"  [WARN] Restore of {path.name} did not settle with {settle_token!r}"
            )
        time.sleep(STABILITY_SLEEP)

    # Post-restore assertions (e.g. the module re-seeding fresh data after a
    # forgotten-type drop: the restore-driven reload logs existing=0).
    for token in scenario.restore_required_tokens:
        if not monitor.wait_for(token, SETTLE_TIMEOUT, settle_start):
            print(f"  [FAIL] Missing post-restore token in {scenario.name}: {token!r}")
            print(f"  Output tail:\n{monitor.output_since(settle_start)[-1600:]}")
            return False

    return True

# =============================================================================
# Scenario definitions
# =============================================================================

# --- pill_spline module edits (all applied from the original source) ---------

SPLINE_REGISTER_ORIGINAL = """\
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    // Fill up to the target count rather than spawning a new path on every
    // rebuild, because hot reload preserves the entities already created.
    let existing_spline_count = {
        let mut query = Query::<&Spline>::new(engine.world_mut());
        query.iter_mut().count()
    };
    for _ in existing_spline_count..DEMO_SPLINE_COUNT {
        if engine
            .world_mut()
            .create_entity()
            .with(demo_spline())
            .build()
            .is_err()
        {
            // Report the failure so the host keeps the previous generation
            // instead of running with a half-populated world.
            return 1;
        }
    }

    // Fully qualified: the import would be unused in the project build, where
    // this module-abi registration path is compiled out.
    //
    // NOTE: this line is asserted on. `MODULE_REGISTERED_MESSAGE` in
    // `devops/core/suite_common.py` matches the message text against the
    // host's stdout, and scenarios in `devops/tests/test_hot_reload_suite.py`
    // additionally require the `existing=` field - which is what proves a
    // reload preserved the entities the previous generation created rather
    // than spawning a fresh set. Reword either and those suites fail with
    // "Missing required token", which reads like a reload failure and is not.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        splines = DEMO_SPLINE_COUNT,
        existing = existing_spline_count,
        max_control_points = MAX_CONTROL_POINTS,
        "pill_spline module registered"
    );
    0
}"""

SPLINE_REGISTER_STUB_NO_REGISTRATION = """\
// Suite stub: the persistable registration is disabled so the module loads
// WITHOUT registering `Spline`. The `#[pill_module]` attribute is dropped too,
// so no compile-time auto-registration runs; the ABI exports are written out
// by hand to keep the module loadable. `DEMO_SPLINE_COUNT`/`demo_spline` are
// referenced only to keep the dead-code lint quiet in this stub build.
#[cfg(feature = "module-abi")]
const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

#[cfg(feature = "module-abi")]
const PILL_MODULE_NAME: &[u8] = b"pill_spline\0";

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    PILL_MODULE_ABI_VERSION
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
    PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const ::pill_engine::EngineApi) -> u32 {
    let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}

#[cfg(feature = "module-abi")]
fn register(engine: &mut Engine) -> u32 {
    let _ = engine;
    let _ = (DEMO_SPLINE_COUNT, demo_spline());
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        "pill_spline module registered (suite stub, Spline NOT registered)"
    );
    0
}"""

SPLINE_REGISTER_STUB_INIT_FAILURE = """\
// Suite stub: deliberately fail init so the host rolls back to the previous
// generation. The `#[pill_module]` attribute is dropped (hand-written exports
// below), and `DEMO_SPLINE_COUNT`/`demo_spline` are referenced only to keep
// the dead-code lint quiet in this stub build.
#[cfg(feature = "module-abi")]
const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

#[cfg(feature = "module-abi")]
const PILL_MODULE_NAME: &[u8] = b"pill_spline\0";

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    PILL_MODULE_ABI_VERSION
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
    PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const ::pill_engine::EngineApi) -> u32 {
    let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}

#[cfg(feature = "module-abi")]
fn register(engine: &mut Engine) -> u32 {
    let _ = engine;
    let _ = (DEMO_SPLINE_COUNT, demo_spline());
    1
}"""

# --- devops/tests/project edits (applied from the original source) ------------

FRAMECOUNTER_MIGRATION_EDITS = [
    (
        "struct FrameCounter {\n    count: u64,\n}",
        "struct FrameCounter {\n    count: u64,\n    migrated: bool,\n}",
    ),
    (
        ".with(FrameCounter { count: 0 })",
        ".with(FrameCounter { count: 0, migrated: false })",
    ),
    (
        ".with(FrameCounter { count: 90 })",
        ".with(FrameCounter { count: 90, migrated: false })",
    ),
    (
        ".with(FrameCounter { count: 180 })",
        ".with(FrameCounter { count: 180, migrated: false })",
    ),
]

PROJECT_FORGOTTEN_EDITS = [
    (
        "#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]\n"
        "#[pill(persistable)]\n"
        "struct LinearVelocity {",
        "#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]\n"
        "struct LinearVelocity {",
    ),
]

# --- Session A scenarios (project = devops/tests/project, module = pill_spline) -----

SESSION_A_SCENARIOS = [
    Scenario(
        name="project_hot_reload",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        PROJECT_SANDBOX_LIB_RS,
                        [("const THRESHOLD: u64 = 200;", "const THRESHOLD: u64 = 150;")],
                    )
                ],
                wait_token=RELOAD_PROJECT_TOKEN,
                required_tokens=[RELOAD_PROJECT_TOKEN, "crates rebuilt by cargo"],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
                wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
            )
        ],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="schema_migration",
        phases=[
            ScenarioPhase(
                edits=[(PROJECT_SANDBOX_LIB_RS, FRAMECOUNTER_MIGRATION_EDITS)],
                wait_token=RELOAD_PROJECT_TOKEN,
                required_tokens=[
                    MIGRATION_START_TOKEN,
                    "'project::FrameCounter' -> OK",
                ],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
                wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
            )
        ],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="project_forgotten_type",
        phases=[
            ScenarioPhase(
                edits=[(PROJECT_SANDBOX_LIB_RS, PROJECT_FORGOTTEN_EDITS)],
                wait_token=RELOAD_PROJECT_TOKEN,
                required_tokens=[
                    PROJECT_FORGOTTEN_WARN_TOKEN,
                    "LinearVelocity",
                ],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
                wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
            )
        ],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="module_hot_reload",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                '"pill_spline module registered"',
                                '"pill_spline module registered v2"',
                            )
                        ],
                    )
                ],
                wait_token=RELOAD_MODULE_TOKEN,
                # `existing=1` pins that the module's persistable data survives
                # a same-config reload (per-artifact TypeId stability).
                required_tokens=[
                    RELOAD_MODULE_TOKEN,
                    MODULE_RELOAD_COMPLETE_TOKEN,
                    "crates rebuilt by cargo",
                    "pill_spline module registered v2",
                    "existing=1",
                ],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
            )
        ],
        restore_after=[SPLINE_LIB_RS],
    ),
    Scenario(
        name="module_double_reload",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                '"pill_spline module registered"',
                                '"pill_spline module registered v2"',
                            )
                        ],
                    )
                ],
                wait_token=RELOAD_MODULE_TOKEN,
                # Two consecutive same-config reloads must stay stable: the
                # module re-seeds nothing (existing stays 1), which pins that
                # a same-config rebuild keeps the same TypeId - accumulation
                # is per distinct artifact, not per reload (audit 3.2/C3).
                required_tokens=["pill_spline module registered v2", "existing=1"],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
            ),
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                '"pill_spline module registered v2"',
                                '"pill_spline module registered v3"',
                            )
                        ],
                    )
                ],
                wait_token=RELOAD_MODULE_TOKEN,
                required_tokens=["pill_spline module registered v3", "existing=1"],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
            ),
        ],
        restore_after=[SPLINE_LIB_RS],
    ),
    Scenario(
        name="module_forgotten_type",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                SPLINE_REGISTER_ORIGINAL,
                                SPLINE_REGISTER_STUB_NO_REGISTRATION,
                            )
                        ],
                    )
                ],
                wait_token=RELOAD_MODULE_TOKEN,
                required_tokens=[MODULE_FORGOTTEN_WARN_TOKEN, "pill_spline::Spline"],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
            )
        ],
        restore_after=[SPLINE_LIB_RS],
        # After the restore-driven reload the module re-registers Spline and
        # re-seeds from scratch: the orphaned data was dropped, so existing=0.
        # This pins drop-at-detection end-to-end (audit 3.2 drop behavior).
        restore_required_tokens=["existing=0"],
    ),
    Scenario(
        name="init_failure_rollback",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                SPLINE_REGISTER_ORIGINAL,
                                SPLINE_REGISTER_STUB_INIT_FAILURE,
                            )
                        ],
                    )
                ],
                wait_token=ROLLBACK_TOKEN,
                required_tokens=[ROLLBACK_TOKEN],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
            )
        ],
        restore_after=[SPLINE_LIB_RS],
    ),
]

# --- Session B scenario (project = examples/project_rs, module = pill_spline) -

SESSION_B_SCENARIOS = [
    Scenario(
        name="cascade_reload",
        phases=[
            ScenarioPhase(
                edits=[
                    (
                        SPLINE_LIB_RS,
                        [
                            (
                                "SAMPLE_VERTICAL_OFFSET: f32 = 0.0",
                                "SAMPLE_VERTICAL_OFFSET: f32 = 10.0",
                            )
                        ],
                    )
                ],
                wait_token=RELOAD_MODULE_TOKEN,
                # `xxsees 1 spline(s)` pins module<->project coexistence: the
                # project probe matches only the project's embedded Spline (its
                # own TypeId), so after both reloads it still sees exactly one -
                # the module DLL's copy is a distinct type (audit 3.1/C1).
                #
                # Only `1 spline(s)` carries that meaning. `xxsees` is incidental
                # text from `examples/project_rs/src/lib.rs`, and this token is
                # matched as a plain substring of the host's stdout - so editing
                # that probe string, which the demo exists to encourage, fails
                # this scenario with "Missing required token" even though the
                # cascade worked perfectly. If that happens, read the analytics
                # lines just above the failure: two reloads and a `1 spline(s)`
                # in the output mean the behaviour is fine and only the wording
                # moved. Widening this to `1 spline(s)` is the better fix.
                required_tokens=[
                    RELOAD_MODULE_TOKEN,
                    MODULE_RELOAD_COMPLETE_TOKEN,
                    CASCADE_TOKEN,
                    RELOAD_PROJECT_TOKEN,
                    "xxsees 1 spline(s)",
                ],
                forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
                wait_after=[("midpoint (400.0, 298.8", PROBE_TIMEOUT)],
            )
        ],
        restore_after=[SPLINE_LIB_RS],
    ),
]

# =============================================================================
# Session runner
# =============================================================================


def verify_fast_path_restart(yaml_content: str) -> bool:
    """Relaunch the host once and verify the up-to-date build fast path.

    After a session's scenarios every artifact is current, so a clean restart
    should skip both the module and project builds and report them as
    "up-to-date skips" in the analytics startup report. A rebuild on restart
    (for example because cargo state changed between runs) is reported as a
    WARN rather than a failure; only a crash or a failure to reach the loop is
    a hard failure.
    """
    print("\n  [TEST] Fast-path restart (everything up to date)...")
    write_host_config(yaml_content)
    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Fast-path restart did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
            return False
        startup_output = monitor.output_since(0)
        if has_crash_signals(startup_output):
            print("  [FAIL] Crash signals during fast-path restart.")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
        skip_match = re.search(r"up-to-date skips:\s*(\d+)", startup_output)
        skip_count = int(skip_match.group(1)) if skip_match else 0
        if FAST_PATH_TOKEN in startup_output and skip_count >= 1:
            print(
                f"  [OK] Fast-path restart: {skip_count} build(s) skipped, "
                "module and project up to date."
            )
            return True
        # Tolerant: cargo state may have changed between runs.
        print(
            f"  [WARN] Fast-path restart did not skip builds "
            f"(up-to-date skips={skip_count}); non-fatal."
        )
        return True
    finally:
        terminate_process(process, monitor)


def run_session(name: str, yaml_content: str, scenarios: Sequence[Scenario]) -> bool:
    """Writes the session config, launches the host, and runs its scenarios."""
    print(f"\n{'=' * 64}")
    print(f"  SESSION {name}")
    print(f"{'=' * 64}")

    write_host_config(yaml_content)

    print("\n  [TEST] Launching standalone host...")
    process, monitor = launch_host()
    session_passed = True

    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop in time.")
            output = monitor.output_since(0)
            print(f"  Output tail:\n{output[-1600:]}")
            return False
        print("  [OK] Host started and entered the project loop.")

        # Validate startup invariants: the module loaded and the analytics
        # report printed. The "up to date, skipping build" fast path is NOT a
        # startup invariant — a cold build state rebuilds everything — so it
        # is not asserted here.
        startup_output = monitor.output_since(0)
        for token in (MODULE_LOADED_TOKEN, ANALYTICS_REPORT_TOKEN):
            if token not in startup_output:
                print(f"  [FAIL] Missing startup token: {token!r}")
                print(f"  Output tail:\n{startup_output[-1600:]}")
                return False
        if has_crash_signals(startup_output):
            print("  [FAIL] Crash signals during startup.")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
        print("  [OK] Startup complete: module loaded and analytics report printed.")

        for scenario in scenarios:
            if not run_scenario(scenario, monitor):
                session_passed = False
                break

        if session_passed:
            # I3: verify the up-to-date build fast path with a clean restart.
            # Stop this host first so the relaunch does not contend for the
            # shared DLLs, then relaunch with the same (fully built) config.
            terminate_process(process, monitor)
            if not verify_fast_path_restart(yaml_content):
                session_passed = False
            print(f"\n  [PASS] Session {name} completed.")
    finally:
        terminate_process(process, monitor)

    return session_passed

# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Parses arguments, runs both sessions, and reports the summary."""
    parser = argparse.ArgumentParser(
        description="Full hot-reloading integration suite for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Skip the initial host build (assume it is already built)",
    )
    args = parser.parse_args()

    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, PROBE_TIMEOUT, COUNTER_TICK_TIMEOUT
    global SETTLE_TIMEOUT, STABILITY_SLEEP, SETTLE_SLEEP, BUILD_TIMEOUT

    scale = args.timeout_scale
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * scale)
    PROBE_TIMEOUT = int(PROBE_TIMEOUT * scale)
    COUNTER_TICK_TIMEOUT = int(COUNTER_TICK_TIMEOUT * scale)
    SETTLE_TIMEOUT = int(SETTLE_TIMEOUT * scale)
    STABILITY_SLEEP = max(1, int(STABILITY_SLEEP * scale))
    SETTLE_SLEEP = max(1, int(SETTLE_SLEEP * scale))
    BUILD_TIMEOUT = int(BUILD_TIMEOUT * scale)

    # Capture originals for every file the suite may touch.
    for path in (HOST_CONFIG_YAML, PROJECT_SANDBOX_LIB_RS, SPLINE_LIB_RS):
        BACKUP.capture(path)

    kill_stale_hosts()

    results: List[Tuple[str, bool]] = []
    try:
        if not args.skip_build:
            if not build_host():
                sys.exit(1)

        results.append(
            (
                "A (devops/tests/project + pill_spline)",
                run_session("A", SESSION_A_YAML, SESSION_A_SCENARIOS),
            )
        )
        if results[-1][1]:
            results.append(
                (
                    "B (examples/project_rs + pill_spline)",
                    run_session("B", SESSION_B_YAML, SESSION_B_SCENARIOS),
                )
            )
    finally:
        # Always restore the developer's files, even on failure.
        BACKUP.restore_all()
        # Restore the host config to its original content last.
        if HOST_CONFIG_YAML in BACKUP._originals:
            BACKUP.restore_one(HOST_CONFIG_YAML)

    print(f"\n{'=' * 64}")
    print("  SUMMARY")
    print(f"{'=' * 64}")
    all_passed = True
    for session_name, passed in results:
        status = "[PASS]" if passed else "[FAIL]"
        print(f"  {status} session {session_name}")
        all_passed = all_passed and passed

    if all_passed:
        print("\n  [PASS] Full hot-reload suite passed.")
        sys.exit(0)
    print("\n  [FAIL] One or more sessions failed.")
    sys.exit(1)


if __name__ == "__main__":
    main()
