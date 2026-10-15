"""The `pill_ui` probe suite: one shared dylib, shaping, and live restyle.

PROTECTS: Exactly one pill_ui dylib loads in a dev host and it shapes and draws text; editing a stylesheet restyles the running UI without a compile.
TIER: host

Merged from the former `test_pill_ui.py` and `test_pill_ui_style.py`. With no
binary argument the suite builds the dev `pill_standalone` posture itself (or
reuses it under `--skip-build`) through the shared host helper, takes the host
lock, and runs the executable twice, each time through the shared output
monitor:

  1. `PILL_UI_PROBE=1` (held open for `PILL_UI_PROBE_HOLD_SECONDS`) shapes
     and draws a sample through the library without a project. While the
     process is alive, the suite verifies that exactly one `pill_ui.dll` is
     mapped - beside one `pill_core.dll` and one `pill_engine_core.dll` -
     and that the probe reported glyph quads, atlas regions and vertices,
     then that it exited cleanly.
  2. `PILL_UI_PROBE=1 PILL_UI_PROBE_STYLE=<file>` renders the UI Gallery
     through a stylesheet and re-reads, re-parses and re-renders it whenever
     the file's bytes change; the suite edits the stylesheet while the
     process is alive and waits for the `restyled revision=1` render with the
     new rule count, then for a clean exit.

REQUIREMENTS: Windows (phase 1 reads the process's module list through
PowerShell); the dev `pill_standalone` built.

USAGE:
  python devops/tests/test_pill_ui.py [path/to/pill_standalone.exe] [--skip-build]

EXAMPLE USAGE:
  python devops/tests/test_pill_ui.py modules/target/debug/pill_standalone.exe
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

SUITE_ROOT = common.WORKSPACE_ROOT

# The token the shaping probe logs last; the module census happens after it.
DONE_TOKEN = "[ui probe] done"
# The style probe's tokens, in order.
STYLED_TOKEN = "[ui probe] styled"
RESTYLED_TOKEN = "[ui probe] restyled"

# The shared dylibs the probe process must have mapped exactly once each.
EXPECTED_DLLS = ("pill_core.dll", "pill_engine_core.dll", "pill_ui.dll")

# The probe draws through `pill_ui` alone, so it needs no renderer and no
# window: the default dev host, hot patch included, is the posture.
HOST_POSTURE = HOST_DEV_HOT_PATCH

# The first stylesheet: two rules, none reported. The edit adds one rule, so
# the rerender is observable.
FIRST_SHEET = """
label { color: rgb(255, 0, 0); }
button { background: rgb(0, 255, 0); border: 1px solid rgb(1, 2, 3); }
"""

SECOND_SHEET = """
label { color: rgb(0, 0, 255); }
button { background: rgb(0, 255, 0); border: 1px solid rgb(1, 2, 3); }
heading { color: rgb(9, 9, 9); }
"""


def loaded_pill_dlls(pid: int):
    """The pill DLL names mapped into `pid`, as a list; `None` on failure."""
    command = (
        f"$names = (Get-Process -Id {pid} -ErrorAction SilentlyContinue).Modules | "
        "Where-Object { $_.ModuleName -like 'pill_*.dll' } | "
        "ForEach-Object { $_.ModuleName }; $names -join ','"
    )
    completed = subprocess.run(
        ["powershell", "-NoProfile", "-NonInteractive", "-Command", command],
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        return None
    return [name for name in completed.stdout.strip().split(",") if name]


def probe_numbers(line: str) -> dict:
    """The counts a `[ui probe]` line reports, by key."""
    keys = "glyphs|atlas-regions|vertices|rules|reports"
    return {key: int(value) for key, value in re.findall(rf"({keys})=(\d+)", line)}


def run_shaping_probe(binary: Path, timeout: float) -> str:
    """Phase 1: the dylib census and the shaped sample; returns its line."""
    environment = dict(os.environ)
    environment["PILL_UI_PROBE"] = "1"
    # Long enough to read the process's modules while it is alive, short
    # enough to keep the phase quick.
    environment["PILL_UI_PROBE_HOLD_SECONDS"] = "8"
    process, monitor = launch_process([str(binary)], MODULES_ROOT, environment)
    try:
        if not monitor.wait_for(DONE_TOKEN, timeout, 0):
            raise TimeoutError(
                "the probe never reported its done token\n"
                + monitor.output_since(0)[-3000:]
            )
        # The census runs while the process holds itself open.
        names = None
        census_deadline = time.monotonic() + 10.0
        while time.monotonic() < census_deadline:
            names = loaded_pill_dlls(process.pid)
            if names:
                break
            # Poll until the probe process has its module list readable.
            time.sleep(0.2)
        if not names:
            raise RuntimeError("could not read the probe's module list")
        counts = {name: names.count(name) for name in EXPECTED_DLLS}
        if counts != {name: 1 for name in EXPECTED_DLLS}:
            raise RuntimeError(f"expected one of each shared dylib, saw {counts}")

        print("loaded modules: " + ", ".join(sorted(set(names))), flush=True)
        shaped = next(
            (
                line
                for line in monitor.output_since(0).splitlines()
                if "shaped" in line and "[ui probe]" in line
            ),
            None,
        )
        if shaped is None:
            raise RuntimeError(
                "no shaped token in the probe output\n" + monitor.output_since(0)[-3000:]
            )
        numbers = probe_numbers(shaped)
        for key in ("glyphs", "atlas-regions", "vertices"):
            if numbers.get(key, 0) <= 0:
                raise RuntimeError(f"probe reported {key}={numbers.get(key)}")
        result = process.wait(timeout=60)
        if result != 0:
            raise RuntimeError(f"the probe exited {result}")
        return shaped
    finally:
        common.terminate_process(process, monitor)


def run_restyle_probe(binary: Path, timeout: float) -> "tuple[str, str]":
    """Phase 2: a live stylesheet edit must restyle the running UI."""
    with tempfile.TemporaryDirectory() as scratch:
        sheet_path = Path(scratch) / "probe_theme.css"
        sheet_path.write_text(FIRST_SHEET, encoding="utf-8")
        environment = dict(os.environ)
        environment["PILL_UI_PROBE"] = "1"
        environment["PILL_UI_PROBE_HOLD_SECONDS"] = "0"
        environment["PILL_UI_PROBE_STYLE"] = str(sheet_path)
        # Long enough to edit and observe one reload, short enough to stay
        # quick.
        environment["PILL_UI_PROBE_STYLE_SECONDS"] = "10"
        process, monitor = launch_process([str(binary)], MODULES_ROOT, environment)
        try:
            if not monitor.wait_for(STYLED_TOKEN, timeout, 0):
                raise TimeoutError(
                    "the first render never arrived\n" + monitor.output_since(0)[-3000:]
                )
            styled = next(
                (
                    line
                    for line in monitor.output_since(0).splitlines()
                    if STYLED_TOKEN in line
                ),
                "",
            )
            first = probe_numbers(styled)
            if first.get("rules", 0) != 2 or first.get("reports", 0) != 0:
                raise RuntimeError(f"first render unexpected: {styled.strip()}")
            if first.get("vertices", 0) <= 0:
                raise RuntimeError(f"first render drew nothing: {styled.strip()}")

            # Edit the stylesheet while the probe is watching.
            sheet_path.write_text(SECOND_SHEET, encoding="utf-8")
            if not monitor.wait_for(RESTYLED_TOKEN, timeout, 0):
                raise TimeoutError(
                    "the restyle never arrived\n" + monitor.output_since(0)[-3000:]
                )
            restyled = next(
                (
                    line
                    for line in monitor.output_since(0).splitlines()
                    if RESTYLED_TOKEN in line
                ),
                "",
            )
            second = probe_numbers(restyled)
            if second.get("rules", 0) != 3 or second.get("reports", 0) != 0:
                raise RuntimeError(f"restyle unexpected: {restyled.strip()}")
            result = process.wait(timeout=60)
            if result != 0:
                raise RuntimeError(f"the probe exited {result}")
            return styled, restyled
        finally:
            common.terminate_process(process, monitor)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary",
        type=Path,
        nargs="?",
        default=None,
        help="A prebuilt pill_standalone.exe; built for the suite's posture when omitted",
    )
    parser.add_argument("--timeout", type=int, default=120)
    add_common_suite_arguments(parser)
    args = parser.parse_args()

    ensure_host_lock()
    binary = args.binary
    if binary is None:
        if not prepare_host(HOST_POSTURE, args, BUILD_TIMEOUT):
            raise SystemExit("no usable host; see the failure above")
        binary = HOST_EXE
    else:
        binary = binary if binary.is_absolute() else SUITE_ROOT / binary
        binary = binary.resolve()
        if not binary.is_file():
            raise SystemExit(f"missing {binary}; build it first: cargo build -p pill_standalone")

    try:
        shaped = run_shaping_probe(binary, args.timeout)
        print(f"PASS: one pill_ui.dll mapped; {shaped.strip()}")
        styled, restyled = run_restyle_probe(binary, args.timeout)
        print(
            "PASS: a stylesheet edit restyled the running UI; "
            f"{styled.strip()}; {restyled.strip()}"
        )
    finally:
        kill_stale_hosts()


if __name__ == "__main__":
    main()
