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

    Every file the suite touches (project_settings.yaml, Systems.cs, the generated
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
# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Session configuration
# =============================================================================

PROJECT_CS_SYSTEMS_CS = WORKSPACE_ROOT / "examples" / "project_cs" / "src" / "Systems.cs"
PROJECT_CS_COMPONENTS_CS = WORKSPACE_ROOT / "examples" / "project_cs" / "src" / "Components.cs"
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
CSHARP_SETTINGS = """\
name: "C# Bridge Test"
build_binary_name: "CSharpBridgeTest"
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

# The managed loader prints the SPECIFIC reason to stderr before the Rust side
# prints its single generic line. Asserting on these is what tells a developer
# which of the three contracts they broke, and pins the wording that tells them.
CSHARP_LOAD_FAILED_TOKEN = "[csharp_runtime] reload failed:"
CSHARP_REJECT_SIGNATURE_REASON = "C# system names or query signatures changed"
CSHARP_REJECT_COMPONENT_REASON = "C# component identities or layouts changed"
CSHARP_REJECT_STARTUP_REASON = "C# startup methods changed"
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
    # Tokens that must appear AFTER `wait_token` was observed, not merely
    # somewhere in the scenario's output.
    #
    # `wait_after` searches from the start of the scenario, so it is satisfied by
    # a line printed before the edit even happened - fine for a token whose text
    # is new, useless for asking "is the previous assembly still running?", where
    # the expected line was already streaming beforehand. These are waited for
    # from a fresh index taken once `wait_token` lands, which is the only way to
    # tell "still alive" from "was alive".
    alive_tokens: Sequence[Tuple[str, float]] = ()


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
        # Captured per phase, not per scenario. Waiting from the scenario's start
        # index makes every phase after the first match the PREVIOUS phase's line
        # and return immediately, so a multi-phase scenario silently asserted
        # nothing about phases 2..n.
        phase_index = monitor.line_count

        for path, replacements in phase.edits:
            # Captured here rather than relying on the startup list. `restore_one`
            # silently does nothing for a path it never captured, so a scenario
            # touching a file the list did not name left that file modified - and
            # every scenario after it then ran against the wrong sources and
            # failed for reasons that had nothing to do with what it tested.
            # Capturing at the point of edit makes that impossible to get wrong,
            # and `capture` is idempotent so the startup list still stands.
            BACKUP.capture(path)
            if not apply_replacements(path, replacements):
                return False

        if not monitor.wait_for(phase.wait_token, RELOAD_TIMEOUT, phase_index):
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

        # Proof of life measured from after the phase's outcome, so a line
        # that was already streaming before the edit cannot satisfy it.
        alive_index = monitor.line_count
        for token, timeout in phase.alive_tokens:
            if not monitor.wait_for(token, timeout, alive_index):
                print(f"  [FAIL] {token!r} did not appear AFTER the reload outcome "
                      f"in scenario: {scenario.name}")
                print("         The previously loaded assembly should still be running.")
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

# Each of the three contracts the managed loader refuses to let a reload break.
# All three edits are valid C# that compiles cleanly - the build must succeed and
# the LOADER must refuse, which is the whole point. A compile error would prove
# nothing about the loader.

# Adds a field to a project-owned component, changing its layout and therefore
# the manifest the native component registry was built from.
COMPONENT_LAYOUT_EDIT = (
    "public struct BallTag\n{\n    public uint Kind;\n}",
    "public struct BallTag\n{\n    public uint Kind;\n    public uint Extra;\n}",
)

# Renames an [EcsSystem] method. Rust builds its execution graph once at startup
# from these names, so the set has to stay stable until a restart.
SYSTEM_NAME_EDIT = (
    "public static void Observe(Query<Read<BallTag>> query)",
    "public static void ObserveRenamed(Query<Read<BallTag>> query)",
)

# Renames an [EcsStartup] method. Startups are not re-run on reload, so a change
# to the set is refused rather than silently ignored.
STARTUP_NAME_EDIT = (
    "public static void Start(Commands commands)",
    "public static void StartRenamed(Commands commands)",
)

# Behavior-only edits chained so each phase is a distinct change to the same
# line. Used to reload repeatedly in one session.
BRIDGE_PROBE_V2_TO_V3_EDIT = (
    "cs spline bridge v2: sees {visibleSplines} spline(s), ",
    "cs spline bridge v3: sees {visibleSplines} spline(s), ",
)
BRIDGE_PROBE_V3_TO_V4_EDIT = (
    "cs spline bridge v3: sees {visibleSplines} spline(s), ",
    "cs spline bridge v4: sees {visibleSplines} spline(s), ",
)

BRIDGE_PROBE_V3_PREFIX = "cs spline bridge v3: sees"
BRIDGE_PROBE_V4_PREFIX = "cs spline bridge v4: sees"


def rejection_scenario(name, path, edit, reason, still_running_prefix):
    """One scenario asserting the loader refuses a contract-breaking change.

    Three things have to hold, and each has been wrong at some point in a system
    like this: the reload is refused, the refusal names the contract that was
    broken, and the assembly that was already running keeps running. The third
    is what `alive_tokens` exists for - the probe line was streaming before the
    edit, so only an occurrence AFTER the refusal proves anything.
    """
    return Scenario(
        name=name,
        phases=[
            ScenarioPhase(
                edits=[(path, [edit])],
                wait_token=CSHARP_RELOAD_REJECTED_TOKEN,
                required_tokens=[
                    CSHARP_RELOAD_REJECTED_TOKEN,
                    CSHARP_LOAD_FAILED_TOKEN,
                    reason,
                ],
                forbidden_tokens=[
                    # The swap must NOT have happened.
                    CSHARP_RELOAD_COMPLETE_TOKEN,
                    PANIC_TOKEN,
                    ACCESS_VIOLATION_TOKEN,
                ],
                alive_tokens=[(still_running_prefix, PROBE_TIMEOUT)],
            )
        ],
        restore_after=[path],
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
    # Reloading repeatedly in one session. Each phase is a distinct edit to the
    # same line, so every swap is real work rather than a no-op rebuild.
    #
    # What this is for: every accepted reload calls `oldContext.Unload()` on a
    # collectible AssemblyLoadContext. If a context were ever retained - by a
    # cached delegate, a static, or a live reference into the old assembly - the
    # unload would not complete and versions would accumulate silently. A swap
    # that still works on the third consecutive reload, with the world's entity
    # count intact, is the observable end of that.
    Scenario(
        name="csharp_repeated_reload_stability",
        phases=[
            ScenarioPhase(
                edits=[(PROJECT_CS_SYSTEMS_CS, [BRIDGE_PROBE_PREFIX_EDIT])],
                wait_token=CSHARP_RELOAD_COMPLETE_TOKEN,
                forbidden_tokens=[CSHARP_RELOAD_REJECTED_TOKEN, PANIC_TOKEN,
                                  ACCESS_VIOLATION_TOKEN],
                alive_tokens=[(BRIDGE_PROBE_V2_PREFIX, PROBE_TIMEOUT)],
            ),
            ScenarioPhase(
                edits=[(PROJECT_CS_SYSTEMS_CS, [BRIDGE_PROBE_V2_TO_V3_EDIT])],
                wait_token=CSHARP_RELOAD_COMPLETE_TOKEN,
                forbidden_tokens=[CSHARP_RELOAD_REJECTED_TOKEN, PANIC_TOKEN,
                                  ACCESS_VIOLATION_TOKEN],
                alive_tokens=[(BRIDGE_PROBE_V3_PREFIX, PROBE_TIMEOUT)],
            ),
            ScenarioPhase(
                edits=[(PROJECT_CS_SYSTEMS_CS, [BRIDGE_PROBE_V3_TO_V4_EDIT])],
                wait_token=CSHARP_RELOAD_COMPLETE_TOKEN,
                forbidden_tokens=[CSHARP_RELOAD_REJECTED_TOKEN, PANIC_TOKEN,
                                  ACCESS_VIOLATION_TOKEN],
                alive_tokens=[(BRIDGE_PROBE_V4_PREFIX, PROBE_TIMEOUT)],
            ),
        ],
        restore_after=[PROJECT_CS_SYSTEMS_CS],
    ),
    # The three refusals. C# hot reload is behavior-only by design, and these
    # pin what "behavior-only" actually means at the boundary - previously the
    # rejection token appeared in this suite only as something that must NEVER
    # happen, so nothing checked that it happens when it should.
    rejection_scenario(
        "csharp_rejects_component_layout_change",
        PROJECT_CS_COMPONENTS_CS,
        COMPONENT_LAYOUT_EDIT,
        CSHARP_REJECT_COMPONENT_REASON,
        BRIDGE_PROBE_PREFIX,
    ),
    rejection_scenario(
        "csharp_rejects_system_signature_change",
        PROJECT_CS_SYSTEMS_CS,
        SYSTEM_NAME_EDIT,
        CSHARP_REJECT_SIGNATURE_REASON,
        BRIDGE_PROBE_PREFIX,
    ),
    rejection_scenario(
        "csharp_rejects_startup_change",
        PROJECT_CS_SYSTEMS_CS,
        STARTUP_NAME_EDIT,
        CSHARP_REJECT_STARTUP_REASON,
        BRIDGE_PROBE_PREFIX,
    ),
]

# =============================================================================
# Host session
# =============================================================================


def build_host() -> bool:
    """Builds the standalone host once before the suite runs."""
    print("\n  [PREP] Building pill_standalone (offline)...")
    try:
        # `hot_patch` is a default feature now; this suite exercises the C#
        # assembly-swap reload, so pin the reload-only posture to keep the
        # Rust patch fast path out of the host it drives.
        result = subprocess.run(
            [
                "cargo",
                "build",
                "--package",
                "pill_standalone",
                "--no-default-features",
                "--features",
                "hot_reload",
                "--offline",
            ],
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
    """Launches the standalone host exe with PROJECT_PATH pinned to the C# project."""
    environment = os.environ.copy()
    environment["PROJECT_PATH"] = "../examples/project_cs"
    return launch_process([str(HOST_EXE)], MODULES_ROOT, environment)


def run_csharp_session() -> bool:
    """Writes the C# project's settings, launches the host, and runs the scenarios."""
    print("\n  [TEST] Launching standalone host with project_cs...")
    write_project_settings(CSHARP_PROJECT_ROOT, CSHARP_SETTINGS)
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
        project_settings_yaml(CSHARP_PROJECT_ROOT),
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
        # Restore the C# project's settings file last.
        csharp_settings_path = project_settings_yaml(CSHARP_PROJECT_ROOT)
        if csharp_settings_path in BACKUP._originals:
            BACKUP.restore_one(csharp_settings_path)

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
