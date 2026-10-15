"""Shared-dylib check for `pill_ui`: one copy loads in a dev host process.

PROTECTS: Exactly one pill_ui dylib loads in a dev host, and it shapes and draws text.
TIER: host

Build the dev standalone first (`cargo build -p pill_standalone`) and pass its
path. This suite runs it with `PILL_UI_PROBE=1`, which shapes and draws a
sample through the library without a project (`PILL_UI_PROBE_HOLD_SECONDS`
keeps the process alive afterwards), and then, while the process is alive,
verifies that exactly one `pill_ui.dll` is mapped - beside one `pill_core.dll`
and one `pill_engine_core.dll` - that the probe reported glyph quads, atlas
regions and vertices, and that the process exited cleanly.

The probe runs from `modules/`, so the host binary is the one `cargo build -p
pill_standalone` writes there; with no path argument the suite builds that
posture itself (or reuses it under `--skip-build`) through the shared host
helper, and takes the host lock before it starts anything.

REQUIREMENTS: Windows (reads the process's module list through PowerShell);
the dev `pill_standalone` built.

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
import time

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

SUITE_ROOT = common.WORKSPACE_ROOT

# The token the probe logs last; the module census happens after it.
DONE_TOKEN = "[ui probe] done"

# The shared dylibs the probe process must have mapped exactly once each.
EXPECTED_DLLS = ("pill_core.dll", "pill_engine_core.dll", "pill_ui.dll")

# The probe draws through `pill_ui` alone, so it needs no renderer and no
# window: the default dev host, hot patch included, is the posture.
HOST_POSTURE = HOST_DEV_HOT_PATCH


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


def probe_numbers(shaped_line: str) -> dict:
    """The `glyphs=`, `atlas-regions=` and `vertices=` counts of a shaped line."""
    return {
        key: int(value)
        for key, value in re.findall(r"(glyphs|atlas-regions|vertices)=(\d+)", shaped_line)
    }


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

    logs = SUITE_ROOT / "local" / "renderer-validation"
    logs.mkdir(parents=True, exist_ok=True)
    log_path = logs / "pill_ui-probe.log"

    environment = dict(os.environ)
    environment["PILL_UI_PROBE"] = "1"
    # Long enough to read the process's modules while it is alive, short
    # enough to keep the suite quick.
    environment["PILL_UI_PROBE_HOLD_SECONDS"] = "8"
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen(
            [str(binary)],
            cwd=str(SUITE_ROOT / "modules"),
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                text = log_path.read_text(encoding="utf-8", errors="replace")
                if DONE_TOKEN in text:
                    break
                if process.poll() is not None:
                    raise RuntimeError(
                        f"probe exited {process.returncode} before its done token; see {log_path}"
                    )
                time.sleep(0.2)
            else:
                raise TimeoutError(f"probe timed out; see {log_path}")

            # The census runs while the process holds itself open.
            names = None
            census_deadline = time.monotonic() + 10.0
            while time.monotonic() < census_deadline:
                names = loaded_pill_dlls(process.pid)
                if names:
                    break
                time.sleep(0.2)
            if not names:
                raise RuntimeError(f"could not read the probe's module list; see {log_path}")
            counts = {name: names.count(name) for name in EXPECTED_DLLS}
            if counts != {name: 1 for name in EXPECTED_DLLS}:
                raise RuntimeError(f"expected one of each shared dylib, saw {counts}")

            print("loaded modules: " + ", ".join(sorted(set(names))), flush=True)
            shaped = next(
                (line for line in text.splitlines() if "shaped" in line and "[ui probe]" in line),
                None,
            )
            if shaped is None:
                raise RuntimeError(f"no shaped token in the probe output; see {log_path}")
            numbers = probe_numbers(shaped)
            for key in ("glyphs", "atlas-regions", "vertices"):
                if numbers.get(key, 0) <= 0:
                    raise RuntimeError(f"probe reported {key}={numbers.get(key)}; see {log_path}")

            result = process.wait(timeout=60)
            if result != 0:
                raise RuntimeError(f"probe exited {result}; see {log_path}")
            print(f"PASS: one pill_ui.dll mapped; {shaped.strip()}; {log_path}")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
            kill_stale_hosts()


if __name__ == "__main__":
    main()
