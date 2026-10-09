"""The live-restyle check for `pill_ui`: editing a stylesheet restyles a
running UI without a compile.

Build the dev standalone first (`cargo build -p pill_standalone`). This
suite runs it with `PILL_UI_PROBE=1` and `PILL_UI_PROBE_STYLE=<file>`, which
renders the UI Gallery through that stylesheet and then re-reads, re-parses
and re-renders it whenever the file's bytes change. The suite:

1. writes a stylesheet into a temp file and starts the probe on it;
2. waits for the probe's `styled` token (the first render);
3. edits the stylesheet while the process is alive;
4. waits for a `restyled revision=1` token with the new rule count;
5. checks the probe exits cleanly.

REQUIREMENTS: the dev `pill_standalone` built.

USAGE:
  python devops/tests/test_pill_ui_style.py [path/to/pill_standalone.exe]

EXAMPLE USAGE:
  python devops/tests/test_pill_ui_style.py modules/target/debug/pill_standalone.exe
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

SUITE_ROOT = Path(__file__).resolve().parents[2]

# The probe's tokens, in order.
STYLED_TOKEN = "[ui probe] styled"
RESTYLED_TOKEN = "[ui probe] restyled"

# The first stylesheet: two rules, none reported.
FIRST_SHEET = """
label { color: rgb(255, 0, 0); }
button { background: rgb(0, 255, 0); border: 1px solid rgb(1, 2, 3); }
"""

# The edit: one more rule, so the rerender is observable.
SECOND_SHEET = """
label { color: rgb(0, 0, 255); }
button { background: rgb(0, 255, 0); border: 1px solid rgb(1, 2, 3); }
heading { color: rgb(9, 9, 9); }
"""


def token_numbers(line: str) -> dict:
    """The `rules=`, `reports=` and `vertices=` counts of a probe line."""
    return {
        key: int(value)
        for key, value in re.findall(r"(rules|reports|vertices)=(\d+)", line)
    }


def wait_for(log_path: Path, token: str, process: subprocess.Popen, timeout: float) -> str:
    """Wait until `token` appears in the log; return the line carrying it."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = log_path.read_text(encoding="utf-8", errors="replace")
        line = next((line for line in text.splitlines() if token in line), None)
        if line is not None:
            return line
        if process.poll() is not None:
            raise RuntimeError(
                f"probe exited {process.returncode} before `{token}`; see {log_path}"
            )
        time.sleep(0.1)
    raise TimeoutError(f"`{token}` never appeared; see {log_path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary",
        type=Path,
        nargs="?",
        default=Path("modules/target/debug/pill_standalone.exe"),
    )
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()

    binary = args.binary if args.binary.is_absolute() else SUITE_ROOT / args.binary
    binary = binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"missing {binary}; build it first: cargo build -p pill_standalone")

    logs = SUITE_ROOT / "local" / "renderer-validation"
    logs.mkdir(parents=True, exist_ok=True)
    log_path = logs / "pill_ui-style-probe.log"

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

        with log_path.open("w", encoding="utf-8") as log:
            process = subprocess.Popen(
                [str(binary)],
                cwd=str(SUITE_ROOT / "modules"),
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            try:
                styled = wait_for(log_path, STYLED_TOKEN, process, args.timeout)
                first = token_numbers(styled)
                if first.get("rules", 0) != 2 or first.get("reports", 0) != 0:
                    raise RuntimeError(f"first render unexpected: {styled.strip()}")
                if first.get("vertices", 0) <= 0:
                    raise RuntimeError(f"first render drew nothing: {styled.strip()}")

                # Edit the stylesheet while the probe is watching.
                sheet_path.write_text(SECOND_SHEET, encoding="utf-8")

                restyled = wait_for(log_path, RESTYLED_TOKEN, process, args.timeout)
                second = token_numbers(restyled)
                if second.get("rules", 0) != 3 or second.get("reports", 0) != 0:
                    raise RuntimeError(f"restyle unexpected: {restyled.strip()}")

                result = process.wait(timeout=60)
                if result != 0:
                    raise RuntimeError(f"probe exited {result}; see {log_path}")
                print(
                    "PASS: a stylesheet edit restyled the running UI; "
                    f"{styled.strip()}; {restyled.strip()}; {log_path}"
                )
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)


if __name__ == "__main__":
    main()
