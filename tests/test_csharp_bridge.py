"""
C# <-> Rust Bridge Integration Suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo) on PATH
  - .NET SDK 8 on PATH (the host runs `dotnet build` for the managed project)
  - Run from the repository root or anywhere (paths are resolved from __file__)

DESCRIPTION
    End-to-end suite for the managed (C#) project backend and the optional
    Rust module -> C# component bridge. Launches the standalone host with
    `examples/project_cs` and the `pill_spline` module plus a component-less
    dummy module, then verifies from console output and on-disk artifacts:

      1. csharp_bridge_startup  - the host starts the C# backend, the host
          auto-generates the module's C# mirror file (exact namespace, size,
          alignment pad), a component-less dummy module writes NO mirror, the
          managed build is warning-free (no CS8019 unused-using), and the
          bridge probe proves both directions of the Rust <-> C# connection:
            * Rust -> C#: C# reads the spline the Rust module seeded
              (sees 1 spline, first P0.X=0, count=4);
            * C# -> Rust: C# creates its own spline through Commands and
              sees it in the SAME native column (sees 2 splines).
      2. csharp_hot_reload      - editing Systems.cs (a behavior-only change)
          is picked up by the watcher: dotnet rebuilds, the collectible
          assembly is reloaded (`[csharp_runtime] reloaded project_cs.dll`,
          `C# hot reload complete`), and the new assembly's probe runs with
          the expected spline count (module + pre-reload seed + post-reload
          seed = 3), proving the connection survives a managed hot reload.
      3. csharp_codegen_rebuild - after a clean restart with the mirror file
          deleted, the host regenerates it from the module's real layout
          before the project build (missing-file regeneration), and the
          bridge still works.

    Every file the suite touches (pill_config.yaml, Systems.cs, the generated
    mirror files) is backed up at startup and restored afterwards, so a
    developer workspace is left exactly as it was.

USAGE
  python tests/test_csharp_bridge.py [--timeout-scale S] [--skip-build]

EXAMPLE USAGE
  python tests/test_csharp_bridge.py
  python tests/test_csharp_bridge.py --timeout-scale 1.5
  python tests/test_csharp_bridge.py --skip-build

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
from suite_common import *  # noqa: F401,F403

# =============================================================================
# Session configuration
# =============================================================================

PROJECT_CS_SYSTEMS_CS = WORKSPACE_ROOT / "examples" / "project_cs" / "src" / "Systems.cs"
SPLINE_GENERATED_FILE = (
    MODULES_ROOT / "optional" / "pill_spline" / "generated" / "pill_spline_Components.g.cs"
)
DUMMY_GENERATED_FILE = (
    MODULES_ROOT / "optional" / "pill_dummy_color" / "generated"
    / "pill_dummy_color_Components.g.cs"
)

# The C# project plus a component-less dummy module: the dummy exercises the
# empty-exposure codegen path (no mirror file must be written) inside the same
# session that asserts the real module's mirror.
CSHARP_YAML = """\
project: "../examples/project_cs"
modules:
  - "pill_spline"
  - "pill_dummy_color"
"""

# --- C# / bridge specific output tokens ---------------------------------------

# The host's collectible loader prints this after a successful assembly swap.
CSHARP_RELOADED_TOKEN = "[csharp_runtime] reloaded project_cs.dll"
# The host's reload poll accepts the swap only when system/signature/manifest
# identity is unchanged (behavior-only reload).
CSHARP_RELOAD_COMPLETE_TOKEN = "C# hot reload complete"
CSHARP_RELOAD_REJECTED_TOKEN = "C# reload rejected"
# The bridge probe emitted by ModuleSplineBridgeDemo in project_cs.
BRIDGE_PROBE_PREFIX = "cs spline bridge: sees"
BRIDGE_PROBE_V2_PREFIX = "cs spline bridge v2: sees"
# dotnet build's summary line; a clean managed build reports exactly these.
DOTNET_CLEAN_WARNING_SUMMARY = "0 Warning(s)"
WARNING_CS_TOKEN = "warning CS"

# =============================================================================
# Scenario model (same shape as the native hot-reload suite)
# =============================================================================


@dataclass
class ScenarioPhase:
    """One edit+wait step inside a scenario."""

    edits: Sequence[Tuple[Path, Sequence[Tuple[str, str]]]]
    wait_token: str
    required_tokens: Sequence[str] = ()
    forbidden_tokens: Sequence[str] = ()
    wait_after: Sequence[Tuple[str, float]] = ()


@dataclass
class Scenario:
    """One C# bridge scenario: sequential phases, expected output, cleanup."""

    name: str
    phases: Sequence[ScenarioPhase]
    restore_after: Sequence[Path] = field(default_factory=list)


def run_scenario(scenario: Scenario, monitor: OutputMonitor) -> bool:
    """Runs one scenario and asserts the host's C# behaviour."""
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
    # The restore rewrites the file, which the watcher turns into another C#
    # reload; wait for that reload to finish so its output cannot pollute the
    # next scenario's wait window.
    settle_start = monitor.line_count
    for path in scenario.restore_after:
        BACKUP.restore_one(path)
        if not monitor.wait_for(CSHARP_RELOADED_TOKEN, SETTLE_TIMEOUT, settle_start):
            print(f"  [WARN] Restore of {path.name} did not settle with a C# reload")
        time.sleep(STABILITY_SLEEP)

    return True

# =============================================================================
# Bridge-specific startup assertions
# =============================================================================


def verify_generated_mirror(
    generated_file: Path,
    expected_fragments: Sequence[str],
    should_exist: bool,
    description: str,
) -> bool:
    """Asserts a module's generated mirror file exists (or not) and matches."""
    if not should_exist:
        if generated_file.exists():
            print(
                f"  [FAIL] {description}: expected no {generated_file.name}, "
                "but a stale mirror file exists"
            )
            return False
        print(f"  [OK] {description}: no {generated_file.name} (empty exposure).")
        return True

    if not generated_file.exists():
        print(f"  [FAIL] {description}: {generated_file.name} was not generated")
        return False
    content = generated_file.read_text(encoding="utf-8")
    for fragment in expected_fragments:
        if fragment not in content:
            print(
                f"  [FAIL] {description}: generated file misses {fragment!r}:\n"
                f"{content[:600]}"
            )
            return False
    print(f"  [OK] {description}: {generated_file.name} has the expected content.")
    return True


def verify_startup(
    monitor: OutputMonitor,
    startup_output: str,
) -> bool:
    """Validates host startup plus the on-disk codegen and clean C# build."""
    for token in (ANALYTICS_REPORT_TOKEN,):
        if token not in startup_output:
            print(f"  [FAIL] Missing startup token: {token!r}")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
    if has_crash_signals(startup_output):
        print("  [FAIL] Crash signals during startup.")
        print(f"  Output tail:\n{startup_output[-1600:]}")
        return False

    # The module mirror must be regenerated from the module's real layout:
    # exact namespace, struct name, size, alignment pad, and the safe accessor.
    if not verify_generated_mirror(
        SPLINE_GENERATED_FILE,
        [
            "namespace pill_spline {",
            "public struct Spline",
            "Size = 196",
            "private readonly uint _alignmentPad;",
            "MemoryMarshal.AsBytes(MemoryMarshal.CreateSpan(ref Unsafe.AsRef(in this), 1))",
        ],
        should_exist=True,
        description="pill_spline mirror",
    ):
        return False

    # A component-less dummy module must NOT produce a mirror file (no
    # unused-using warnings, no binding for a component that never existed).
    if not verify_generated_mirror(
        DUMMY_GENERATED_FILE,
        (),
        should_exist=False,
        description="pill_dummy_color mirror",
    ):
        return False

    # The managed build must be clean: dotnet reports "0 Warning(s)" and the
    # compiler emits no `warning CS` lines (e.g. CS8019 unused usings from a
    # header-only generated file).
    if DOTNET_CLEAN_WARNING_SUMMARY not in startup_output:
        print(
            "  [FAIL] Managed build did not report "
            f"{DOTNET_CLEAN_WARNING_SUMMARY!r} at startup."
        )
        print(f"  Output tail:\n{startup_output[-1600:]}")
        return False
    if WARNING_CS_TOKEN in startup_output:
        print(f"  [FAIL] Managed build produced {WARNING_CS_TOKEN!r} warnings.")
        print(f"  Output tail:\n{startup_output[-1600:]}")
        return False
    print(f"  [OK] Managed build is warning-free ({DOTNET_CLEAN_WARNING_SUMMARY}).")

    # Bridge probes (both directions of the Rust <-> C# connection). The
    # probe is throttled to one line every 2s, so both waits are bounded by
    # PROBE_TIMEOUT. start_index 0 covers the whole run because the first
    # probe can fire before verify_startup finishes its file checks.
    #   Rust -> C#: C# reads the spline the module seeded with the correct
    #   layout (first P0.X=0, count=4).
    if not monitor.wait_for(
        f"{BRIDGE_PROBE_PREFIX} 1 spline(s), first P0.X=0, count=4",
        PROBE_TIMEOUT,
        0,
    ):
        print("  [FAIL] Rust -> C# bridge probe missing (C# did not read the module spline).")
        print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
        return False
    print("  [OK] Rust -> C#: C# reads the module-seeded spline with the right layout.")
    #   C# -> Rust: C# created its own spline via Commands and sees it in the
    #   same native column (2 total).
    if not monitor.wait_for(
        f"{BRIDGE_PROBE_PREFIX} 2 spline(s), first P0.X=0, count=4",
        PROBE_TIMEOUT,
        0,
    ):
        print("  [FAIL] C# -> Rust bridge probe missing (C# spline not in the native column).")
        print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
        return False
    print("  [OK] C# -> Rust: C#-created spline is visible in the module's native column.")
    return True

# =============================================================================
# Scenarios
# =============================================================================

# Behavior-only edit to the bridge probe: changes the log prefix but not the
# system name, its signature, the component manifest, or the startups, so the
# collectible reload is accepted (C# reload is behavior-only by design).
# Matches inside the interpolated string `$"[project_cs] cs spline bridge:
# sees {visibleSplines} spline(s), "`.
BRIDGE_PROBE_PREFIX_EDIT = (
    "cs spline bridge: sees {visibleSplines} spline(s), ",
    "cs spline bridge v2: sees {visibleSplines} spline(s), ",
)

SESSION_SCENARIOS = [
    Scenario(
        name="csharp_hot_reload",
        phases=[
            ScenarioPhase(
                edits=[(PROJECT_CS_SYSTEMS_CS, [BRIDGE_PROBE_PREFIX_EDIT])],
                wait_token=CSHARP_RELOADED_TOKEN,
                required_tokens=[
                    CSHARP_RELOADED_TOKEN,
                    CSHARP_RELOAD_COMPLETE_TOKEN,
                ],
                forbidden_tokens=[
                    CSHARP_RELOAD_REJECTED_TOKEN,
                    PANIC_TOKEN,
                    ACCESS_VIOLATION_TOKEN,
                ],
                # After the swap the new assembly seeds one more spline (its
                # static re-initializes) on top of the module's and the
                # pre-reload C# spline, which both survive: 1 + 1 + 1 = 3.
                wait_after=[
                    (f"{BRIDGE_PROBE_V2_PREFIX} 3 spline(s)", PROBE_TIMEOUT),
                ],
            )
        ],
        restore_after=[PROJECT_CS_SYSTEMS_CS],
    ),
]

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


def run_csharp_session() -> bool:
    """Writes the C# config, launches the host, and runs the scenarios."""
    print("\n  [TEST] Launching standalone host with project_cs...")
    write_host_config(CSHARP_YAML)
    process, monitor = launch_host()
    session_passed = True

    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop in time.")
            print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
            return False
        print("  [OK] Host started and entered the project loop.")

        startup_output = monitor.output_since(0)
        if not verify_startup(monitor, startup_output):
            return False

        for scenario in SESSION_SCENARIOS:
            if not run_scenario(scenario, monitor):
                session_passed = False
                break

        if session_passed:
            print("\n  [PASS] C# session completed.")
    finally:
        terminate_process(process, monitor)

    return session_passed


def verify_codegen_rebuild() -> bool:
    """Restart the host with the mirror deleted and verify regeneration.

    After the session every artifact is current. Deleting the generated mirror
    and restarting must make the host regenerate it from the module's real
    layout (missing-file path) before the managed project builds, and the
    bridge must still work afterwards.
    """
    print("\n  [TEST] Codegen rebuild (mirror deleted, host restarts)...")
    if SPLINE_GENERATED_FILE.exists():
        SPLINE_GENERATED_FILE.unlink()
        print(f"  [PREP] Deleted {SPLINE_GENERATED_FILE.name}.")

    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Codegen-rebuild restart did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
            return False
        startup_output = monitor.output_since(0)
        if has_crash_signals(startup_output):
            print("  [FAIL] Crash signals during codegen-rebuild restart.")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
        # The mirror must exist again with the exact expected content.
        if not verify_generated_mirror(
            SPLINE_GENERATED_FILE,
            [
                "namespace pill_spline {",
                "public struct Spline",
                "Size = 196",
                "private readonly uint _alignmentPad;",
            ],
            should_exist=True,
            description="regenerated pill_spline mirror",
        ):
            return False
        # And the bridge must be live again (Rust -> C# direction).
        if not monitor.wait_for(
            f"{BRIDGE_PROBE_PREFIX} 1 spline(s), first P0.X=0, count=4",
            PROBE_TIMEOUT,
        ):
            print("  [FAIL] Bridge probe missing after codegen rebuild.")
            print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
            return False
        print("  [OK] Codegen rebuild: mirror regenerated and bridge live.")
        return True
    finally:
        terminate_process(process, monitor)


# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Parses arguments, runs the C# session and the codegen-rebuild check."""
    parser = argparse.ArgumentParser(
        description="C# <-> Rust bridge integration suite for Rust-Hybrid-ECS"
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

    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, PROBE_TIMEOUT, SETTLE_TIMEOUT
    global STABILITY_SLEEP, SETTLE_SLEEP, BUILD_TIMEOUT

    scale = args.timeout_scale
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * scale)
    PROBE_TIMEOUT = int(PROBE_TIMEOUT * scale)
    SETTLE_TIMEOUT = int(SETTLE_TIMEOUT * scale)
    STABILITY_SLEEP = max(1, int(STABILITY_SLEEP * scale))
    SETTLE_SLEEP = max(1, int(SETTLE_SLEEP * scale))
    BUILD_TIMEOUT = int(BUILD_TIMEOUT * scale)

    # Capture originals for every file the suite may touch, including the
    # committed bootstrap mirror that the codegen-rebuild check deletes.
    for path in (
        HOST_CONFIG_YAML,
        PROJECT_CS_SYSTEMS_CS,
        SPLINE_GENERATED_FILE,
        DUMMY_GENERATED_FILE,
    ):
        BACKUP.capture(path)

    kill_stale_hosts()

    passed = True
    try:
        if not args.skip_build:
            if not build_host():
                sys.exit(1)
        if not run_csharp_session():
            passed = False
        if passed:
            if not verify_codegen_rebuild():
                passed = False
    finally:
        # Always restore the developer's files, even on failure.
        BACKUP.restore_all()
        # Restore the host config to its original content last.
        if HOST_CONFIG_YAML in BACKUP._originals:
            BACKUP.restore_one(HOST_CONFIG_YAML)

    print(f"\n{'=' * 64}")
    print("  SUMMARY")
    print(f"{'=' * 64}")
    if passed:
        print("  [PASS] C# <-> Rust bridge suite passed.")
        sys.exit(0)
    print("  [FAIL] C# <-> Rust bridge suite failed.")
    sys.exit(1)


if __name__ == "__main__":
    main()
