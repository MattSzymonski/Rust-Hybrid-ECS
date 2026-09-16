"""
Gameplay scripting analyzer suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - .NET SDK 8 on PATH
  - Run from the repository root or anywhere (paths are resolved from __file__)

DESCRIPTION
    Regression suite for the PILLxxxx compile-time rules in
    `modules/pill_csharp_runtime/analyzers`. Those rules are the only guard
    that catches several scripting hazards before they reach a running host -
    an await that escapes its frame, a static a parallel batch races on, a
    static event that roots the retiring load context - so a rule that quietly
    stops firing removes a protection nobody would notice was gone.

    Two checks, both driven by `dotnet build` on `examples/project_cs`:

      1. clean_project_is_diagnostic_free - the real gameplay project builds
         with no PILL diagnostic at all. This is what makes the second check
         meaningful: a rule that fires on correct code would be worse than no
         rule, and it also proves the analyzer is actually wired into the
         project rather than silently absent.

      2. every_rule_fires - a probe file declaring exactly one violation of
         each rule produces every expected diagnostic. The probe is written
         into the project, built, and removed afterwards.

    The probe is deliberately not committed: it does not compile by
    construction, so it can only exist for the duration of this suite.

USAGE
  python tests/test_csharp_analyzer.py [--keep-probe]

EXAMPLE USAGE
  python tests/test_csharp_analyzer.py
  python tests/test_csharp_analyzer.py --keep-probe

--- SCRIPT ---
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.suite_common import CSHARP_PROJECT_ROOT, WORKSPACE_ROOT  # noqa: E402

# =============================================================================
# Constants
# =============================================================================

PROJECT_CS_CSPROJ = CSHARP_PROJECT_ROOT / "project_cs.csproj"
ANALYZER_PROBE_CS = CSHARP_PROJECT_ROOT / "src" / "_AnalyzerProbe.cs"

BUILD_TIMEOUT_SECONDS = 300

# Every rule the analyzer declares, each of which the probe below violates
# exactly once. Keeping the list here rather than deriving it from the
# analyzer is deliberate: the suite should fail when a rule is removed, not
# quietly stop checking it.
EXPECTED_RULES = (
    "PILL0101",  # an ECS system must be static
    "PILL0102",  # unsupported parameter list
    "PILL0103",  # must return void
    "PILL0201",  # may not be async
    "PILL0301",  # mutable static state in a system-declaring type
    "PILL0302",  # static event blocks unload
    "PILL0303",  # background work blocks unload and runs outside any frame
    "PILL0304",  # GCHandle.Alloc blocks unload
    "PILL0305",  # finalizer delays unload
    "PILL0401",  # unsupported component layout
    "PILL0402",  # StructLayout.Pack is not modelled
    "PILL0403",  # unsupported component field type
)

# One violation per rule. The class is non-static so the instance method in it
# is declarable at all - an instance member inside a static class is a C# error
# that would mask PILL0101 rather than test it.
ANALYZER_PROBE_SOURCE = '''\
// Written by devops/tests/test_csharp_analyzer.py while the suite runs; removed
// afterwards. Not part of the project, and not compilable by construction:
// every declaration here violates exactly one PILLxxxx rule.
using System;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace TracyLive.AnalyzerProbe;

[StructLayout(LayoutKind.Auto)]
internal struct AutoLaidOutComponent                     // PILL0401
{
    public float X;
}

[StructLayout(LayoutKind.Sequential, Pack = 1)]
internal struct PackedComponent                          // PILL0402
{
    public byte Flag;
    public float Value;
}

internal struct UnsupportedFieldComponent                // PILL0403
{
    public bool Flag;
}

internal class ProbeSystems
{
    internal static int MutableCounter;                  // PILL0301
    internal static event Action? Ticked;                // PILL0302

    [EcsSystem]
    internal async void InstanceAsyncSystem(             // PILL0101 + PILL0201
        Query<Read<Position>> query)
    {
        _ = query;
        await Task.Yield();
    }

    [EcsSystem]
    internal static int WrongReturnSystem(Query<Read<Position>> query)   // PILL0103
    {
        _ = query;
        return 0;
    }

    [EcsSystem]
    internal static void NoParameterSystem() { }         // PILL0102

    [EcsSystem]
    internal static void StartsBackgroundWork(Query<Read<Position>> query)
    {
        _ = query;
        new Thread(() => { }).Start();                   // PILL0303
        GCHandle.Alloc(new object());                    // PILL0304
        Ticked?.Invoke();
    }
}

internal sealed class OwnsAFinalizer                     // PILL0305
{
    ~OwnsAFinalizer() { }
}
'''


# =============================================================================
# Build helpers
# =============================================================================


def build_project() -> str:
    """Build the gameplay project and return the combined build output.

    Normal verbosity is required: the quiet verbosity dotnet defaults to in
    some hosts summarises diagnostics away, and this suite reads them.
    """
    completed = subprocess.run(
        [
            "dotnet",
            "build",
            str(PROJECT_CS_CSPROJ),
            "-v",
            "n",
            "--nologo",
        ],
        cwd=str(WORKSPACE_ROOT),
        capture_output=True,
        text=True,
        timeout=BUILD_TIMEOUT_SECONDS,
    )
    return completed.stdout + completed.stderr


def rules_in(output: str) -> set:
    """Every distinct PILL rule id the build reported."""
    return set(re.findall(r"PILL\d{4}", output))


# =============================================================================
# Scenarios
# =============================================================================


def scenario_clean_project_is_diagnostic_free() -> bool:
    """The real gameplay project must build with no PILL diagnostic.

    This is the half that makes the other check meaningful. A rule that fires
    on correct code is worse than no rule, and a clean build here also proves
    the analyzer is wired into the project: if the reference were dropped, the
    second scenario would fail rather than pass vacuously.
    """
    print("\n  [TEST] The gameplay project builds with no analyzer diagnostic.")
    if ANALYZER_PROBE_CS.exists():
        ANALYZER_PROBE_CS.unlink()
    output = build_project()
    if "Build succeeded" not in output:
        print("  [FAIL] The gameplay project does not build.")
        print(output[-3000:])
        return False
    reported = rules_in(output)
    if reported:
        print(f"  [FAIL] The clean project reported {sorted(reported)}.")
        return False
    print("  [OK] Clean project: no PILL diagnostics.")
    return True


def scenario_every_rule_fires() -> bool:
    """A probe violating every rule must produce every diagnostic."""
    print("\n  [TEST] Every analyzer rule fires on a probe that violates it.")
    ANALYZER_PROBE_CS.write_text(ANALYZER_PROBE_SOURCE, encoding="utf-8")
    try:
        output = build_project()
    finally:
        if ANALYZER_PROBE_CS.exists():
            ANALYZER_PROBE_CS.unlink()

    reported = rules_in(output)
    missing = [rule for rule in EXPECTED_RULES if rule not in reported]
    if missing:
        print(f"  [FAIL] These rules did not fire: {missing}")
        print(f"  Reported: {sorted(reported)}")
        print(output[-3000:])
        return False

    # A build carrying error-severity diagnostics must fail: a rule reported
    # as a warning would let a hazardous project ship.
    if "Build succeeded" in output:
        print("  [FAIL] The probe built successfully; the error rules are not errors.")
        return False

    unexpected = sorted(reported - set(EXPECTED_RULES))
    if unexpected:
        print(f"  [FAIL] Unexpected rules fired: {unexpected}")
        return False

    print(f"  [OK] All {len(EXPECTED_RULES)} rules fired, and the build failed as it must.")
    return True


SCENARIOS = (
    scenario_clean_project_is_diagnostic_free,
    scenario_every_rule_fires,
)


# =============================================================================
# Entry point
# =============================================================================


def main() -> None:
    """Runs both analyzer scenarios and reports one summary."""
    parser = argparse.ArgumentParser(
        description="Gameplay scripting analyzer suite for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--keep-probe",
        action="store_true",
        help="Leave the probe file in place after the run, to inspect the diagnostics",
    )
    arguments = parser.parse_args()

    passed = True
    try:
        for scenario in SCENARIOS:
            if not scenario():
                passed = False
                break
    finally:
        # The probe belongs to the suite; it never survives a run unless the
        # developer explicitly asked to inspect it.
        if ANALYZER_PROBE_CS.exists() and not arguments.keep_probe:
            ANALYZER_PROBE_CS.unlink()
            print(f"  [CLEANUP] removed {ANALYZER_PROBE_CS.relative_to(WORKSPACE_ROOT)}")
        # Leave the tree in a buildable state whatever happened above.
        if not arguments.keep_probe:
            build_project()

    print(f"\n{'=' * 64}")
    print("  SUMMARY")
    print(f"{'=' * 64}")
    if passed:
        print("  [PASS] C# scripting analyzer suite passed.")
        sys.exit(0)
    print("  [FAIL] C# scripting analyzer suite failed.")
    sys.exit(1)


if __name__ == "__main__":
    main()
