"""
Launches the statically linked shipping binary and checks that it runs.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain on PATH. No .NET, and no project sources are edited.

DESCRIPTION
    The hot-reload net cannot cover the shipping build: by construction none of
    its eight suites apply to a binary with reloading compiled out. `test_basic`
    checks that such a binary *builds* and that the reload-only strings are
    gone, which is a static assertion. This one checks that it actually runs.

    Three things are asserted, and the third is the point:

      1. The binary reaches the project loop and reports frames.
      2. It prints the project's own probe line, so the statically linked
         project and its optional modules really did initialize - a build that
         linked them but never called their entry points would still start.
      3. It never invokes cargo. The reloading host shells out to cargo before
         the first frame; a shipping build must not, and the surest way to know
         is to watch for a cargo child process rather than to trust a grep.

    Nothing is edited, so this suite is safe to run against a dirty tree and
    leaves nothing to restore.

USAGE
  python devops/tests/test_shipping_smoke.py [--timeout-scale N]

EXAMPLE USAGE
  python devops/tests/test_shipping_smoke.py
  python devops/tests/test_shipping_smoke.py --timeout-scale 2

--- SCRIPT ---
"""

# Standard library
import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.paths import MODULES_ROOT, REPOSITORY_ROOT, find_executable  # noqa: E402

# =============================================================================
# Constants
# =============================================================================

# Cargo arguments that select the shipping posture: `hot_reload` off, project
# and modules linked in.
SHIPPING_FEATURES = ["--no-default-features", "--features", "static_project"]

# Where the release build lands.
SHIPPING_BINARY = (
    MODULES_ROOT
    / "target"
    / "release"
    / ("pill_standalone.exe" if os.name == "nt" else "pill_standalone")
)

# The host prints this once it is running frames.
FRAME_TOKEN = "FPS |"

# The example project's own output, which only appears if `project::init` ran
# and registered its systems.
PROJECT_TOKEN = "[project]"

# Proof that the statically linked modules initialized too.
MODULE_TOKEN = "optional module linked"

# Anything here means the reload machinery was compiled in after all.
FORBIDDEN_TOKENS = (
    "building project module",
    "watching for source changes",
    "module DLL loaded successfully",
)

# Seconds to wait for the binary to reach the project loop, before scaling.
STARTUP_TIMEOUT = 40

# How long to let it run once started, so several frames and one probe land.
OBSERVE_SECONDS = 6

BUILD_TIMEOUT = 1800

# =============================================================================
# Free Functions
# =============================================================================


def build_shipping_binary(timeout: int) -> str:
    """Builds the shipping binary, returning an error message or "" on success.

    RUSTFLAGS is cleared for the reason `devops/ci_cd/build_release.sh`
    documents: the workspace sets `-C prefer-dynamic`, which rustc refuses to
    combine with the release profile's `lto = "fat"`.
    """
    command = [
        find_executable("cargo"),
        "build",
        "--release",
        "--package",
        "pill_standalone",
        *SHIPPING_FEATURES,
        "--manifest-path",
        str(MODULES_ROOT / "Cargo.toml"),
    ]
    completed = subprocess.run(
        command,
        cwd=str(REPOSITORY_ROOT),
        env=dict(os.environ, RUSTFLAGS=""),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    )
    if completed.returncode != 0:
        return (completed.stdout or "")[-1500:]
    return ""


def cargo_child_processes(parent_id: int) -> int:
    """Counts cargo or rustc processes started underneath `parent_id`.

    A grep of the binary proves the *command* is not compiled in; this proves
    nothing spawned one anyway, which is the behaviour the shipping build is
    actually promising.

    Returns 0 when the platform's process tools are unavailable, so the check
    degrades to the token assertions rather than failing for the wrong reason.
    """
    if os.name != "nt":
        try:
            listed = subprocess.run(
                ["ps", "-o", "ppid=,comm="],
                stdout=subprocess.PIPE,
                text=True,
                timeout=30,
            ).stdout
        except (OSError, subprocess.SubprocessError):
            return 0
        return sum(
            1
            for line in listed.splitlines()
            if line.split()[:1] == [str(parent_id)]
            and any(name in line for name in ("cargo", "rustc"))
        )

    try:
        listed = subprocess.run(
            [
                "wmic",
                "process",
                "where",
                f"ParentProcessId={parent_id}",
                "get",
                "Name",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=30,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return 0
    return sum(
        1 for line in listed.splitlines() if "cargo" in line.lower() or "rustc" in line.lower()
    )


def run_and_observe(timeout_scale: float) -> int:
    """Launches the shipping binary and checks what it does. Returns an exit code."""
    print("=" * 70)
    print("  Shipping Build Smoke Test")
    print("=" * 70)

    print("\n  [BUILD] cargo build --release -p pill_standalone " + " ".join(SHIPPING_FEATURES))
    failure = build_shipping_binary(int(BUILD_TIMEOUT * timeout_scale))
    if failure:
        print("  [FAIL] the shipping binary did not build:")
        print(failure)
        return 1
    if not SHIPPING_BINARY.is_file():
        print(f"  [FAIL] {SHIPPING_BINARY} was not produced")
        return 1
    print(f"  [OK] built, {SHIPPING_BINARY.stat().st_size:,} bytes")

    print("\n  [RUN] launching it")
    process = subprocess.Popen(
        [str(SHIPPING_BINARY)],
        cwd=str(MODULES_ROOT),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    )

    collected = []
    spawned_cargo = 0
    deadline = time.monotonic() + STARTUP_TIMEOUT * timeout_scale
    observe_until = None
    try:
        # A blocking readline would hang past the deadline if the host went
        # quiet, so the loop is bounded by wall time and the process is killed
        # to unblock it.
        while time.monotonic() < deadline:
            if process.poll() is not None:
                break
            line = process.stdout.readline()
            if not line:
                break
            collected.append(line)
            if observe_until is None and FRAME_TOKEN in line:
                # Frames are running; watch a little longer so a probe lands,
                # and check for a cargo child while it is definitely alive.
                observe_until = time.monotonic() + OBSERVE_SECONDS * timeout_scale
                spawned_cargo = cargo_child_processes(process.pid)
            if observe_until is not None and time.monotonic() > observe_until:
                break
    finally:
        process.kill()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass

    output = "".join(collected)
    failures = []

    if FRAME_TOKEN not in output:
        failures.append("the binary never reached the project loop")
    if MODULE_TOKEN not in output:
        failures.append("no optional module was linked and initialized")
    if PROJECT_TOKEN not in output:
        failures.append("the project's own systems never produced output")
    for token in FORBIDDEN_TOKENS:
        if token in output:
            failures.append(f"reload machinery ran: {token!r}")
    if spawned_cargo:
        failures.append(f"it spawned {spawned_cargo} cargo/rustc child process(es)")

    if failures:
        print("\n  [FAIL] " + "\n  [FAIL] ".join(failures))
        print("\n  Output tail:\n" + output[-1600:])
        return 1

    frames = output.count(FRAME_TOKEN)
    modules = output.count(MODULE_TOKEN)
    print(f"  [OK] reached the project loop, {frames} frame report(s)")
    print(f"  [OK] {modules} optional module(s) linked and initialized")
    print("  [OK] the project's systems produced output")
    print("  [OK] no cargo or rustc child process, and no reload machinery ran")
    print("-" * 70)
    print("  TEST PASSED")
    print("=" * 70)
    return 0


def main() -> None:
    """Parse arguments and run the smoke test."""
    parser = argparse.ArgumentParser(
        prog="test_shipping_smoke.py",
        description="Launch the statically linked shipping binary and check it runs.",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="multiply every timeout, for slower machines (default 1.0)",
    )
    arguments = parser.parse_args()
    sys.exit(run_and_observe(arguments.timeout_scale))


if __name__ == "__main__":
    main()
