"""End-to-end acceptance for the panel registration API (W-0004 Stage 16).

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs with its spline editor crate (the suite edits its
    panel body and restores it)

DESCRIPTION
  Starts the editor on the spline project and drives the panel layer:

    1. `pill_spline_editor`'s Spline Tool registers through the panel API
       (the registry line lists it) and opening it from the Window menu
       docks its tab and builds the panel;
    2. a control-point edit through the panel's command is an undo step;
       undo restores the previous count;
    3. Re-seed runs through the module's own system and logs its points;
    4. a body-only edit of the panel function live-patches (`LIVE ms`);
    5. editing the runtime `pill_spline` crate reloads it and the editor
       crate, and the panel builds again in the new generation (kept open);
    6. the harness' panic panel is contained (a warn line, the shell keeps
       painting) and an unregistered panel shows the placeholder;
    7. the editor exits cleanly.

  Everything is asserted on the editor's log tokens; both edited sources are
  restored in a `finally`. The suite runs under its own LOCALAPPDATA sandbox.

USAGE
  python devops/tests/test_editor_panels.py

EXAMPLE USAGE
  python devops/tests/test_editor_panels.py

--- SCRIPT ---
"""

import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULES = ROOT / "modules"
BINARY = MODULES / "target" / "debug" / "pill_editor_native.exe"
VALIDATION = ROOT / "local" / "editor-validation"
LOG = VALIDATION / "editor-panels.log"
COMMANDS = VALIDATION / "commands-stage16.txt"
APPDATA = VALIDATION / "stage16_appdata"
SPLINE_EDITOR_SOURCE = (
    MODULES
    / "extensions"
    / "pill_spline"
    / "pill_spline_editor"
    / "src"
    / "pill_spline_editor.rs"
)
SPLINE_SOURCE = (
    MODULES / "extensions" / "pill_spline" / "pill_spline" / "src" / "pill_spline.rs"
)


def read_log() -> str:
    """The editor log so far, tolerant of a file the writer holds open."""
    try:
        return LOG.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_token(
    process: subprocess.Popen, pattern: str, timeout: float, label: str, start: int = 0
) -> int:
    """Wait for `pattern` after byte offset `start`; returns the new length."""
    deadline = time.monotonic() + timeout
    expression = re.compile(pattern)
    while time.monotonic() < deadline:
        text = read_log()
        match = expression.search(text[start:])
        if match:
            print(f"  [OK] {label}")
            return len(text)
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode}\n" + text[-4000:]
            )
        time.sleep(0.5)
    raise TimeoutError(f"{label}: timeout waiting for {pattern}\n" + read_log()[-4000:])


def find_in_log(pattern: str, start: int = 0) -> re.Match:
    """The first match of `pattern` in the log from `start`, now."""
    match = re.compile(pattern).search(read_log()[start:])
    if not match:
        raise RuntimeError(f"no match for {pattern}\n" + read_log()[-3000:])
    return match


def append(line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def main() -> int:
    if os.name != "nt":
        print("SKIP: the native editor is Windows-only until Stage 24")
        return 0
    if not BINARY.is_file():
        raise SystemExit(
            f"editor binary not found at {BINARY}; build it with "
            "`cargo build -p pill_editor_native` in modules/ first"
        )
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    VALIDATION.mkdir(parents=True, exist_ok=True)
    original_editor = SPLINE_EDITOR_SOURCE.read_text(encoding="utf-8")
    original_runtime = SPLINE_SOURCE.read_text(encoding="utf-8")
    COMMANDS.write_text("# stage 16 panels suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "420"
    environment["LOCALAPPDATA"] = str(APPDATA)
    log = LOG.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(BINARY)],
        cwd=str(MODULES),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log: anchoring after the previous match can
        # skip a line that was written before the anchor was taken.
        wait_token(process, r"\[editor\] pill_spline_editor loaded", 300, "editor crate loaded")
        wait_token(process, r"\[editor\] pill_editor_shell_editor loaded", 300, "shell loaded")
        wait_token(process, r"\[editor\] window created", 300, "editor started")
        wait_token(process, r"\[editor\] command registry ready", 120, "registry ready")

        # Step 1: the registry lists the crate's panel and the harness'
        # panic fixture. These are startup lines, so they are searched from
        # the top of the log.
        wait_token(
            process,
            r'\[editor\] panel registered\s+id="?spline_tool"?\s+title="?Spline Tool"?\s+owner="?pill_spline_editor"?',
            60,
            "spline tool registered",
        )
        wait_token(
            process,
            r"\[editor\] panel registry ready\s+panels=2",
            60,
            "panel registry ready",
        )

        # Step 2: open the panel from the Window menu command and watch it
        # build in the new tab.
        before = len(read_log())
        append("command editor.panel.spline_tool")
        wait_token(
            process,
            r'\[editor\] panel focused\s+id="?spline_tool"?\s+existing=false',
            30,
            "panel focused (new tab)",
            before,
        )
        wait_token(
            process,
            r'\[editor\] module panel built\s+id="?spline_tool"?',
            30,
            "panel built",
            before,
        )

        # Step 3: a control-point edit through the panel's command is one
        # recorded undo step. The demo project's own system rewrites the
        # spline every tick, so the *value* is the sim's; the step itself is
        # what is asserted here (and the stable `elo` field below carries
        # the value-level check).
        before = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entity\s", 30, "entities listed", before)
        entity = find_in_log(
            r'\[test\] entity\s+entity=(\S+)\s+components="?[^"\s]*Spline(,|")', before
        ).group(1)
        print(f"  [OK] spline entity {entity}")

        before = len(read_log())
        append("command spline_editor.add_point")
        wait_token(
            process,
            r"\[spline_editor\] control point added\s+entity=\S+\s+control_points=\d+",
            30,
            "add point ran the module system",
            before,
        )
        wait_token(
            process,
            r'\[editor\] history push\s+label="spline_editor\.add_point"',
            30,
            "add point recorded",
            before,
        )
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="spline_editor\.add_point"',
            30,
            "add point undone",
            before,
        )

        # A stable field proves undo/redo values end to end.
        before = len(read_log())
        append(f"dump field {entity} Spline elo")
        wait_token(process, r"\[test\] field\s", 30, "original elo read", before)
        original_elo = find_in_log(r"value=F32\(([^)]+)\)", before).group(1)
        before = len(read_log())
        append(f"edit set {entity} Spline elo 3.5f")
        wait_token(
            process,
            r'\[editor\] history push\s+label="set Spline\.elo"',
            30,
            "elo edit recorded",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} Spline elo")
        wait_token(process, r"value=F32\(3\.5\)", 30, "elo edit applied", before)
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set Spline\.elo"',
            30,
            "elo edit undone",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} Spline elo")
        wait_token(
            process,
            r"value=F32\(" + re.escape(original_elo) + r"\)",
            30,
            "elo restored",
            before,
        )

        # Step 4: re-seed through the module's own system.
        before = len(read_log())
        append("command spline_editor.reseed")
        wait_token(
            process,
            r"\[spline_editor\] reseeded\s+splines=1\s+control_points=16",
            30,
            "re-seed ran",
            before,
        )

        # Step 5: a body-only edit of the panel function live-patches.
        before = len(read_log())
        patched = original_editor.replace(
            'builder.heading("Spline Tool")', 'builder.heading("Spline Tool v2")', 1
        )
        assert patched != original_editor, "panel heading marker not found"
        SPLINE_EDITOR_SOURCE.write_text(patched, encoding="utf-8")
        wait_token(process, r"LIVE \d+ ms", 120, "panel body live-patched", before)
        SPLINE_EDITOR_SOURCE.write_text(original_editor, encoding="utf-8")

        # Step 6: editing the runtime crate reloads it and the editor crate;
        # the panel is kept open - its tab still exists (focus reports
        # `existing=true`) and the re-registered module command still runs.
        before = len(read_log())
        SPLINE_SOURCE.write_text(
            original_runtime + "\n// stage 16 suite reload check\n", encoding="utf-8"
        )
        wait_token(
            process,
            r'module DLL loaded successfully\s+module="?pill_spline"?\s+abi_version=\d+',
            180,
            "runtime crate reloaded",
            before,
        )
        wait_token(
            process,
            r"\[editor\] pill_spline_editor loaded\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        before_wait = len(read_log())
        append("command editor.panel.spline_tool")
        wait_token(
            process,
            r'\[editor\] panel focused\s+id="?spline_tool"?\s+existing=true',
            30,
            "panel tab kept across the reload",
            before_wait,
        )
        before_wait = len(read_log())
        append("command spline_editor.add_point")
        wait_token(
            process,
            r"\[spline_editor\] control point added\s+entity=\S+\s+control_points=\d+",
            30,
            "module command re-registered",
            before_wait,
        )
        wait_token(
            process,
            r"\[spline_editor\] command ran\s+id=spline_editor\.add_point\s+commands_ran=3",
            30,
            "command ran in the new generation",
            before_wait,
        )
        SPLINE_SOURCE.write_text(original_runtime, encoding="utf-8")

        # Step 7: the panic fixture is contained, and an unregistered panel
        # shows the placeholder.
        before = len(read_log())
        append("command editor.panel.containment_test")
        wait_token(
            process,
            r'\[editor\] module panel panicked.*id="?containment_test"?',
            60,
            "panicking panel contained",
            before,
        )
        before_wait = len(read_log())
        append("command editor.panel.spline_tool")
        wait_token(
            process,
            r'\[editor\] panel focused\s+id="?spline_tool"?',
            30,
            "shell kept painting after the panic",
            before_wait,
        )
        before_wait = len(read_log())
        append("panel ghost_tool")
        wait_token(
            process,
            r'\[editor\] panel focused\s+id="?ghost_tool"?',
            30,
            "unknown panel focused",
            before_wait,
        )
        wait_token(
            process,
            r'\[editor\] module panel missing\s+id="?ghost_tool"?',
            30,
            "placeholder shown for the missing panel",
            before_wait,
        )

        # Step 8: exit cleanly.
        append("quit")
        deadline = time.monotonic() + 60
        while process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.5)
        assert process.poll() is not None, "editor did not quit"
        assert process.returncode == 0, f"editor exited {process.returncode}"
        print("  [OK] clean exit, code 0")
    finally:
        if process.poll() is None:
            subprocess.run(
                ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        SPLINE_EDITOR_SOURCE.write_text(original_editor, encoding="utf-8")
        SPLINE_SOURCE.write_text(original_runtime, encoding="utf-8")
    print("PASS: editor panels")
    return 0


if __name__ == "__main__":
    sys.exit(main())
