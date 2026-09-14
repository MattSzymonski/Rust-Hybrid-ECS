"""
Shared Component Identity Integration Test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    Pins the one property that makes a component type usable from two binaries
    at once: `pill_spline::Spline` is linked BOTH by the project (which depends
    on the crate directly, so it can write `Query<&Spline>`) and by the module
    DLL the host loads alongside it. Those are separate compilation units, so
    each gets its own `TypeId` for what the programmer wrote as one type, and
    identifying the component by `TypeId` therefore splits it into two
    components with two columns that cannot see each other's entities.

    `#[pill(shared)]` replaces that identity with one derived from a declared
    name, which both binaries compute identically and without coordination.

    Two scenarios, and the second is what gives the first its meaning:

    1. `shared_identity_binds_both_binaries` - with the attribute in place, the
       engine's own ECS report shows exactly ONE `pill_spline::Spline` column
       and lists the component under `shared identity`. One column is the whole
       claim.

    2. `distinct_type_ids_are_proven_by_removing_it` - the control. Removing
       `#[pill(shared)]` must make startup FAIL with the peer-collision error,
       because the two registrations now arrive under different `TypeId`s with
       the same type name while the first still holds live rows.

       This is what proves the `TypeId`s genuinely differ rather than happening
       to coincide. If they were equal, the second registration would be an
       ordinary idempotent re-registration and nothing would be reported; the
       collision can only happen because they are not.

       It is also the regression this guards: before the collision guard
       existed, that second registration silently evicted the first's persist
       entries, and every row the evicted side owned was dropped at the next
       hot reload with no error at all.

USAGE
  python tests/test_shared_component_identity.py [--timeout-scale S]
                                                 [--scenario NAME]

EXAMPLE USAGE
  python tests/test_shared_component_identity.py
  python tests/test_shared_component_identity.py --timeout-scale 2
  python tests/test_shared_component_identity.py --scenario shared_identity_binds_both_binaries

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import List, Optional, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

SPLINE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"

# The component whose identity this suite is about, spelled as the engine
# registers it. With `#[pill(shared)]` and no explicit name, the derive uses
# `module_path!()::TypeName`, which for a crate-root type is the same string
# `std::any::type_name` produces - so nothing keyed on the name moves.
SHARED_COMPONENT_NAME = "pill_spline::Spline"

# The attribute under test, and the form it takes with sharing removed.
SHARED_ATTRIBUTE = "#[pill(persistable, shared)]"
UNSHARED_ATTRIBUTE = "#[pill(persistable)]"

# The project must load `pill_spline` as a module *and* link it directly, which
# is exactly the arrangement that produces two `TypeId`s. `examples/project_rs`
# does both, so this suite drives it rather than the fixture project.
TEST_PROJECT_SETTINGS = """\
name: "Shared Identity Test"
build_binary_name: "SharedIdentityTest"
modules:
  - "pill_spline"
"""

# Emitted by the engine's built-in diagnostics system once per second.
ECS_REPORT_TOKEN = "ECS state"
SHARED_IDENTITY_HEADING = "shared identity"

# Emitted by `World::register_persistable_component` when a second live
# registration claims one type name; see `WorldError::ComponentNameCollision`.
COLLISION_TOKEN = "two live registrations claim one component type name"
# The host turns a non-zero project init into a setup failure.
SETUP_FAILED_TOKEN = "Host setup failed"

# The module's sample-offset constant, edited to trigger a body-only reload.
# A plain `f32` literal, so exactly one line matches.
SAMPLE_OFFSET_PATTERN = re.compile(
    r"^(\s*)const SAMPLE_VERTICAL_OFFSET:\s*f32\s*=\s*([0-9.]+)\s*;?\s*$", re.MULTILINE
)

# `pill_spline::register` counts the splines already in the world before topping
# up to its demo count, and logs the number. After a reload it must be 1: the
# rows the retiring generation created survived into the new one.
SURVIVED_TOKEN = "existing=1"

STARTUP_TIMEOUT = 120
PREBUILD_TIMEOUT = 420
REPORT_TIMEOUT = 90
# How long to wait for the optional ECS-report confirmation in scenario 1.
REPORT_SAMPLE_TIMEOUT = 20
COLLISION_TIMEOUT = 150
RELOAD_TIMEOUT_SECONDS = 180

ORIGINAL_PROJECT_SETTINGS: Optional[str] = None

# A report line naming one stored column, e.g.
#   "│    bit   0  pill_spline::Spline    1 x 200 B │"
COLUMN_LINE_PATTERN = re.compile(r"^\W*bit\s+(\d+)\s+(\S+)")


# =============================================================================
# Host configuration
# =============================================================================


def install_test_project_settings() -> None:
    """Backs up the example project's settings and installs this suite's own.

    The optional-module list is read only from this file, so the suite writes a
    minimal one loading just `pill_spline`, keeping the scenario independent of
    whatever modules the project happens to list.
    """
    global ORIGINAL_PROJECT_SETTINGS
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    ORIGINAL_PROJECT_SETTINGS = (
        settings_path.read_text(encoding="utf-8") if settings_path.exists() else None
    )
    settings_path.write_text(TEST_PROJECT_SETTINGS, encoding="utf-8")


def restore_project_settings() -> None:
    """Restores the project's settings file, or removes this suite's own."""
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    if ORIGINAL_PROJECT_SETTINGS is None:
        settings_path.unlink(missing_ok=True)
    else:
        settings_path.write_text(ORIGINAL_PROJECT_SETTINGS, encoding="utf-8")


def set_shared_identity(enabled: bool) -> None:
    """Puts `#[pill(shared)]` on the spline component, or takes it off.

    Always writes, even when the text is already what is wanted, because the
    write is what makes the file newer than the artifact built from it. The
    host decides whether to rebuild a module by comparing source and artifact
    timestamps, and `BackupRegistry.restore_all` deliberately restores the
    ORIGINAL mtime - so after a previous run the source can look older than a
    DLL compiled from the opposite setting, and the host would skip the rebuild
    and load a module whose attribute does not match the source on disk.

    Written atomically, as every source edit in these suites is, so the host's
    watcher never sees a half-written file.
    """
    content = read_source(SPLINE_LIB_RS)
    wanted = SHARED_ATTRIBUTE if enabled else UNSHARED_ATTRIBUTE
    other = UNSHARED_ATTRIBUTE if enabled else SHARED_ATTRIBUTE
    if wanted not in content:
        if content.count(other) != 1:
            raise RuntimeError(
                f"Expected exactly one {other!r} in {SPLINE_LIB_RS.name}, "
                f"found {content.count(other)}"
            )
        content = content.replace(other, wanted, 1)
    atomic_write(SPLINE_LIB_RS, content)


def edit_sample_offset() -> None:
    """Changes the module's sample-offset constant, triggering a reload.

    A body-only value change: it alters what the module computes without
    touching any component's shape, so the reload takes the ordinary path
    rather than a schema migration - which is what this scenario wants to
    observe the shared column surviving.
    """
    content = read_source(SPLINE_LIB_RS)
    matches = list(SAMPLE_OFFSET_PATTERN.finditer(content))
    if len(matches) != 1:
        raise RuntimeError(
            f"Expected exactly one SAMPLE_VERTICAL_OFFSET line, found {len(matches)}"
        )
    match = matches[0]
    new_offset = (float(match.group(2)) + 1.0) % 10.0
    new_line = f"{match.group(1)}const SAMPLE_VERTICAL_OFFSET: f32 = {new_offset:.1f};"
    atomic_write(SPLINE_LIB_RS, content.replace(match.group(0), new_line, 1))


# =============================================================================
# Process helpers
# =============================================================================


def launch_host() -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts the headless host against `examples/project_rs`."""
    process_environment = os.environ.copy()
    process_environment["PROJECT_PATH"] = "../examples/project_rs"
    # `rendering` is required, not a preference. `examples/project_rs` links
    # `pill_master_renderer` directly, so building the project drags wgpu into
    # its dependency graph and turns on features in crates `pill_core` also
    # depends on. The host selects itself as a cargo anchor to unify features
    # for that build, so an anchor without `rendering` resolves `pill_core`
    # differently from the project and the project DLL fails to load with "The
    # specified procedure could not be found" (os error 127). See
    # `apply_cargo_host_overrides` in `pill_host/src/build_runner.rs`.
    return launch_process(
        ["cargo", "run", "--package", "pill_standalone", "--features", "rendering"],
        MODULES_ROOT,
        process_environment,
    )


def build_host() -> bool:
    """Builds the standalone host once so a launch does not pay for it."""
    print("\n  [PREP] Building pill_standalone...")
    try:
        result = subprocess.run(
            ["cargo", "build", "-p", "pill_standalone", "--features", "rendering"],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=PREBUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] Pre-build timed out after {PREBUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if result.returncode != 0:
        print("  [FAIL] Pre-build failed:")
        print(result.stderr[-2000:])
        return False

    print("  [OK] Host built.")
    return True


# =============================================================================
# Report parsing
# =============================================================================


def extract_latest_report(output: str) -> Optional[List[str]]:
    """Returns the lines of the last complete ECS state report in `output`.

    A report is bounded by the block's own borders, so a partially flushed one
    at the tail is skipped rather than parsed into a wrong answer - which is
    the failure mode that makes an output-matching suite report the wrong
    reason.
    """
    lines = output.splitlines()
    starts = [index for index, line in enumerate(lines) if ECS_REPORT_TOKEN in line]
    for start in reversed(starts):
        for end in range(start + 1, len(lines)):
            if "└" in lines[end]:
                return lines[start : end + 1]
    return None


def columns_named(report: List[str], component_name: str) -> List[int]:
    """Returns the mask bits of every stored column with this component name.

    One entry means one column, which is the property under test. Two would
    mean the two binaries each got their own.
    """
    bits: List[int] = []
    for line in report:
        match = COLUMN_LINE_PATTERN.search(line)
        if match and match.group(2) == component_name:
            bits.append(int(match.group(1)))
    return bits


def lists_under_shared_identity(report: List[str], component_name: str) -> bool:
    """Whether the report's `shared identity` section names this component."""
    within_section = False
    for line in report:
        if SHARED_IDENTITY_HEADING in line:
            within_section = True
            continue
        if within_section:
            # Sections are separated by the block's horizontal rules.
            if "├" in line or "└" in line:
                within_section = False
            elif component_name in line:
                return True
    return False


# =============================================================================
# Scenarios
# =============================================================================


def scenario_shared_identity_binds_both_binaries() -> bool:
    """With `#[pill(shared)]`, the two registrations become one component.

    The observable is the *absence* of the peer collision, which is decisive
    only because the control scenario shows the same arrangement producing one:
    the guard fires whenever two different `ComponentId`s carry one type name
    while the first still holds rows. Sharing removes the second id, so silence
    here means the project's registration bound to the module's column rather
    than allocating its own.

    Deliberately not asserted through the per-second ECS report: that needs
    sustained frames, and a windowed host presents its first frame and then
    waits for input under this harness, so the report may never arrive. It is
    still parsed when one does appear, as a direct confirmation of the column
    count, and its absence is reported rather than passed over in silence.
    """
    print("\n  [TEST] Shared identity: project and module bind one column.")
    set_shared_identity(True)

    process, monitor = launch_host()
    try:
        start_index = monitor.line_count
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            # Name the likely cause rather than only the symptom: a failure
            # to bind shows up here as a startup that never happens, because
            # the collision aborts the project's init before the loop.
            if COLLISION_TOKEN in monitor.output_since(0):
                print(
                    "  [FAIL] Startup aborted on a peer collision for "
                    f"{SHARED_COMPONENT_NAME}. The two registrations did NOT "
                    "bind to one component, so shared identity is not in force."
                )
            else:
                print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started with the project and the module loaded.")

        output = monitor.output_since(0)

        # The claim. Reaching the loop already means the project's init
        # returned success, so a collision could not have been reported - but
        # assert it explicitly, because that is the fact under test.
        if COLLISION_TOKEN in output:
            print(
                "  [FAIL] A peer collision was reported despite shared identity: "
                "the two registrations did not bind to one component."
            )
            return False
        print("  [OK] No peer collision: both registrations resolved to one component.")

        if SETUP_FAILED_TOKEN in output:
            print("  [FAIL] Host setup failed even though startup was reached.")
            return False
        print("  [OK] The project's init succeeded with the module already loaded.")

        # Direct confirmation when frames happen to run: exactly one column for
        # the component, listed under the report's `shared identity` section.
        if monitor.wait_for(ECS_REPORT_TOKEN, REPORT_SAMPLE_TIMEOUT, start_index):
            report = extract_latest_report(monitor.output_since(start_index))
            if report is None:
                print("  [FAIL] An ECS report was printed but could not be parsed.")
                return False
            bits = columns_named(report, SHARED_COMPONENT_NAME)
            if len(bits) != 1:
                print(
                    f"  [FAIL] Expected exactly 1 {SHARED_COMPONENT_NAME} column, "
                    f"found {len(bits)} (mask bits {bits}). Two means the project "
                    f"and the module each got their own."
                )
                print("  Report:\n" + "\n".join(report))
                return False
            if not lists_under_shared_identity(report, SHARED_COMPONENT_NAME):
                print(
                    f"  [FAIL] {SHARED_COMPONENT_NAME} is not listed under "
                    f"'{SHARED_IDENTITY_HEADING}', so it resolved to a per-binary "
                    f"identity rather than a declared one."
                )
                print("  Report:\n" + "\n".join(report))
                return False
            print(
                f"  [OK] ECS report confirms one column at mask bit {bits[0]}, "
                f"under '{SHARED_IDENTITY_HEADING}'."
            )
        else:
            # Said out loud rather than skipped quietly: the pass rests on the
            # collision evidence above, and this line records that the extra
            # confirmation was unavailable this run.
            print(
                f"  [NOTE] No ECS report within {REPORT_SAMPLE_TIMEOUT}s "
                f"(a windowed host runs frames only while it has input); the "
                f"column-count confirmation was skipped."
            )

        print("  [PASS] One type, two binaries, one component.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_distinct_type_ids_are_proven_by_removing_it() -> bool:
    """Without the attribute, the two registrations collide.

    The collision is only possible because the two `TypeId`s differ: equal ones
    would make the second registration idempotent and silent.
    """
    print("\n  [TEST] Control: removing shared identity must collide.")
    set_shared_identity(False)

    process, monitor = launch_host()
    try:
        # The host is expected to fail setup and exit, so this waits for the
        # exit rather than for a token: `wait_for*` returns `None` as soon as
        # the process is gone, whether or not the token was printed, and the
        # collision is printed milliseconds before that exit.
        deadline = time.monotonic() + COLLISION_TIMEOUT
        reached_loop = False
        while time.monotonic() < deadline:
            if STARTUP_TOKEN in monitor.output_since(0):
                reached_loop = True
                break
            if not monitor.process_alive():
                break
            time.sleep(0.2)

        output = monitor.output_since(0)

        if reached_loop:
            print(
                "  [FAIL] The host started cleanly without `#[pill(shared)]`. "
                "Two registrations of one type name should have collided, so "
                "either the two `TypeId`s no longer differ (and this suite is "
                "not testing what it claims), or the peer-collision guard in "
                "`register_persistable_component` has stopped reporting."
            )
            return False

        if COLLISION_TOKEN not in output:
            print("  [FAIL] No peer collision reported, and no project loop either.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Peer collision reported, so the two `TypeId`s genuinely differ.")

        if SHARED_COMPONENT_NAME not in output:
            print(f"  [FAIL] The collision did not name {SHARED_COMPONENT_NAME}.")
            return False
        print(f"  [OK] The collision names {SHARED_COMPONENT_NAME}.")

        # The registration must fail the init rather than evicting the peer,
        # which is the silent data-loss path this guard replaced.
        if SETUP_FAILED_TOKEN not in output:
            print(
                "  [FAIL] The collision was reported but setup continued; it must "
                "fail the init rather than evict the peer's persist entries."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Setup failed loudly instead of silently dropping the peer's rows.")

        print("  [PASS] Distinct `TypeId`s confirmed; the guard catches the collision.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_shared_component_survives_a_module_reload() -> bool:
    """A shared component's rows survive a real DLL swap.

    The combination nothing else covers. `test_hot_reload_suite.py` reloads
    `pill_spline` against the fixture project, which does not link the crate,
    so only one binary registers `Spline`. Here the project links it as well,
    so the reload makes BOTH binaries re-register the same shared component
    against a column that already holds rows.

    That is also the only path exercising `rehome_native_columns` with two
    registrants: a column stores function pointers into the artifact that
    created it, and the reload re-points them at the most recently registered
    generation. With one registrant there is nothing to compete; with two there
    is.
    """
    print("\n  [TEST] Hot reload: a shared component's rows survive a DLL swap.")
    set_shared_identity(True)

    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started; the module has created its demo spline.")

        start_index = monitor.line_count
        print("  [TEST] Editing the module to trigger a reload...")
        edit_sample_offset()

        if not monitor.wait_for(
            MODULE_RELOAD_COMPLETE_TOKEN, RELOAD_TIMEOUT_SECONDS, start_index
        ):
            print("  [FAIL] The module reload never completed.")
            print(f"  Output tail:\n{monitor.output_since(start_index)[-2000:]}")
            return False
        print("  [OK] Module reloaded from a freshly built DLL.")

        output = monitor.output_since(start_index)

        # The point of the scenario: the rows outlived the swap.
        if SURVIVED_TOKEN not in output:
            print(
                "  [FAIL] The reloaded module did not report "
                f"'{SURVIVED_TOKEN}', so the spline rows did not survive the "
                "swap - the column was lost or re-seeded from scratch."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print(f"  [OK] The reloaded module reports '{SURVIVED_TOKEN}': rows survived.")

        # Re-registering from both binaries must not look like a collision.
        if COLLISION_TOKEN in output:
            print(
                "  [FAIL] Re-registration after the reload was reported as a peer "
                "collision; the reloaded generation did not bind to the column."
            )
            return False
        print("  [OK] Both binaries re-registered without a collision.")

        # A column whose function table was left pointing into the unmapped
        # image would fault here rather than report anything.
        if has_crash_signals(monitor.output_since(0)):
            print("  [FAIL] The host crashed during or after the reload.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        if not monitor.process_alive():
            print("  [FAIL] The host exited during the reload.")
            return False
        print("  [OK] The host is alive and un-crashed after the swap.")

        print("  [PASS] A shared column survives a reload with two registrants.")
        return True
    finally:
        common.terminate_process(process, monitor)


SCENARIOS = {
    "shared_identity_binds_both_binaries": scenario_shared_identity_binds_both_binaries,
    "distinct_type_ids_are_proven_by_removing_it": (
        scenario_distinct_type_ids_are_proven_by_removing_it
    ),
    "a_shared_component_survives_a_module_reload": (
        scenario_a_shared_component_survives_a_module_reload
    ),
}


# =============================================================================
# CLI
# =============================================================================


def main() -> None:
    """Parses arguments and runs the shared-identity scenarios."""
    parser = argparse.ArgumentParser(
        description="Shared component identity integration test for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    parser.add_argument(
        "--scenario",
        choices=sorted(SCENARIOS),
        help="Run one scenario instead of all of them",
    )
    args = parser.parse_args()

    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    if not SPLINE_LIB_RS.exists():
        print(f"ERROR: Missing module source: {SPLINE_LIB_RS}")
        sys.exit(1)

    global STARTUP_TIMEOUT, PREBUILD_TIMEOUT, REPORT_TIMEOUT, COLLISION_TIMEOUT
    global REPORT_SAMPLE_TIMEOUT, RELOAD_TIMEOUT_SECONDS
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * args.timeout_scale)
    PREBUILD_TIMEOUT = int(PREBUILD_TIMEOUT * args.timeout_scale)
    REPORT_TIMEOUT = int(REPORT_TIMEOUT * args.timeout_scale)
    COLLISION_TIMEOUT = int(COLLISION_TIMEOUT * args.timeout_scale)
    REPORT_SAMPLE_TIMEOUT = int(REPORT_SAMPLE_TIMEOUT * args.timeout_scale)
    RELOAD_TIMEOUT_SECONDS = int(RELOAD_TIMEOUT_SECONDS * args.timeout_scale)

    selected = (
        [(args.scenario, SCENARIOS[args.scenario])]
        if args.scenario
        else list(SCENARIOS.items())
    )

    print("=" * 66)
    print("  Shared Component Identity Integration Test")
    print(f"  Workspace: {WORKSPACE_ROOT}")
    print(f"  Component: {SHARED_COMPONENT_NAME}")
    print(f"  Scenarios: {len(selected)}")
    print(f"  Time scale: {args.timeout_scale}x")
    print("=" * 66)

    # The suite edits the module source and the project's settings; both are
    # captured now and restored whatever happens below.
    BACKUP.capture(SPLINE_LIB_RS)
    kill_stale_hosts()
    install_test_project_settings()

    results = []
    try:
        if not build_host():
            sys.exit(1)
        for name, scenario in selected:
            try:
                results.append((name, scenario()))
            finally:
                kill_stale_hosts()
    finally:
        print("\n  [CLEANUP] Restoring module source and project settings...")
        # `reset_mtime=False` deliberately: this suite toggles an attribute that
        # changes what the compiled module MEANS, so the restored source must look
        # newer than whatever was built from the opposite setting. Putting the
        # original timestamp back would leave the tree poisoned - the host would
        # judge the stale DLL fresh and load a module whose identity does not
        # match the source on disk.
        BACKUP.restore_all(reset_mtime=False)
        restore_project_settings()
        kill_stale_hosts()
        print("  [OK] Restored.")

    print("\n" + "=" * 66)
    for name, passed in results:
        print(f"  {'PASS' if passed else 'FAIL'}  {name}")
    all_passed = bool(results) and all(passed for _, passed in results)
    print("  TEST PASSED" if all_passed else "  TEST FAILED")
    print("=" * 66)
    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    run_suite_with_timing(main)
