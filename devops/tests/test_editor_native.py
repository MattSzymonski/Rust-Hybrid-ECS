"""End-to-end acceptance for the native editor (W-0004 Stage 13, D26 level 3).

Builds against an already-built `pill_editor_native` binary, starts it on
`circus_demo` with the scripted input channel (`PILL_EDITOR_TEST_COMMANDS`),
and asserts on its log tokens: startup, the shell module loading, panel
dispatch, input routing between the scene view and the editor UI, the Game
view on its own surface with editor-camera navigation in the scene view, a
live patch of the shell's system keeping its state, and a clean exit. Then
it checks the clean exit code on five further runs.

The only files this suite edits are the shell module's source (restored in a
`finally`) and its own sandboxed layout files under `LOCALAPPDATA`;
everything else it starts is its own process tree.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import time

# A classic (Dioxus) editor layout: a Scene tabset above a Statistics one,
# plus a module panel only the classic editor knew about. The native editor
# must migrate it once - dropping the module panel by name - into its own
# layout file and never touch the classic file again.
CLASSIC_LAYOUT_FIXTURE = """{
  "schema_version": 2,
  "root": 1,
  "nodes": {
    "1": { "type": "row", "axis": "vertical", "children": [
        { "node": 2, "weight": 70.0 },
        { "node": 5, "weight": 30.0 }
    ] },
    "2": { "type": "tab_set", "tabs": [3, 4], "selected": 3 },
    "3": { "type": "tab", "title": "Scene", "panel": "scene", "closeable": false },
    "4": { "type": "tab", "title": "ghost_panel", "panel": { "module": "ghost_panel" }, "closeable": true },
    "5": { "type": "tab_set", "tabs": [6], "selected": 6 },
    "6": { "type": "tab", "title": "Statistics", "panel": "statistics", "closeable": true }
  },
  "active_tabset": 2,
  "next_id": 7
}
"""


def read_log(path: Path) -> str:
    """The log so far, tolerant of a file the writer holds open."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_for_token(
    path: Path, pattern: str, process: subprocess.Popen, timeout: float, label: str
) -> str:
    """Waits until `pattern` (a regex) appears in the log, and returns it.

    Raises when the deadline passes or the process exits first, with the
    log tail in the message so a failure is diagnosable without the file.
    """
    deadline = time.monotonic() + timeout
    expression = re.compile(pattern)
    while time.monotonic() < deadline:
        match = expression.search(read_log(path))
        if match:
            return match.group(0)
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode} before {pattern}\n"
                + read_log(path)[-4000:]
            )
        time.sleep(0.5)
    raise TimeoutError(
        f"{label}: timeout waiting for {pattern}\n" + read_log(path)[-4000:]
    )


def start_editor(
    binary: Path,
    project: str,
    working_directory: Path,
    log_path: Path,
    extra_environment: dict,
) -> subprocess.Popen:
    """Starts the built editor binary and logs to `log_path`.

    The binary is spawned directly rather than through `cargo run`: it is
    what a user runs, and - since the Stage 13 staging fix - a directly
    spawned editor resolves the shell's engine dylibs (staged beside the
    module it loads) without cargo's PATH help, so this launch keeps that
    fix proven. `cargo run` was the historical workaround and is no longer
    needed.
    """
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = project
    environment.update(extra_environment)
    log = log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(binary)],
        cwd=str(working_directory),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
    )
    # The suite keeps reading the file; the handle is the child's now.
    log.close()
    return process


def append_command(path: Path, line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with path.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        help="the built pill_editor_native binary (default: the debug build)",
    )
    parser.add_argument(
        "--project",
        default="../examples/circus_demo",
        help="the project, resolved the way the editor resolves PROJECT_PATH (from modules/)",
    )
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument(
        "--exit-runs",
        type=int,
        default=5,
        help="clean-exit runs to check after the full flow",
    )
    arguments = parser.parse_args()
    # The native editor is a Windows executable until the platform layer
    # grows its X11 and Wayland implementations (Stage 24); elsewhere this
    # suite has nothing to check.
    if os.name != "nt":
        print("SKIP: the native editor is Windows-only until Stage 24")
        return 0
    # A stale editor from an interrupted run would fight this one over the
    # module build directory; every other host-driving suite does the same.
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    root = Path(__file__).resolve().parents[2]
    binary = (
        arguments.binary.resolve()
        if arguments.binary
        else (root / "modules" / "target" / "debug" / "pill_editor_native.exe")
    )
    if not binary.is_file():
        raise SystemExit(
            f"editor binary not found at {binary}; build it with "
            "`cargo build -p pill_editor_native` in modules/ first"
        )
    working_directory = root / "modules"
    logs = root / "local" / "editor-validation"
    logs.mkdir(parents=True, exist_ok=True)
    appdata = logs / "appdata"
    appdata.mkdir(parents=True, exist_ok=True)
    # Give the editor a classic layout to migrate and no native file yet.
    state_directory = appdata / "RustHybridEcs"
    state_directory.mkdir(parents=True, exist_ok=True)
    (state_directory / "editor_layout.json").write_text(
        CLASSIC_LAYOUT_FIXTURE, encoding="utf-8"
    )
    native_layout = state_directory / "editor_layout_native.json"
    if native_layout.exists():
        native_layout.unlink()
    commands_path = logs / "commands.txt"
    commands_path.write_text("", encoding="utf-8")
    shell_source = (
        root
        / "modules"
        / "extensions"
        / "pill_editor_shell"
        / "pill_editor_shell_editor"
        / "src"
        / "pill_editor_shell_editor.rs"
    )
    original_source = shell_source.read_text(encoding="utf-8")
    log_path = logs / "editor-native.log"
    process = start_editor(
        binary,
        arguments.project,
        working_directory,
        log_path,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(commands_path),
        },
    )
    try:
        # Step 1: startup, all the way to panels being painted.
        wait_for_token(
            log_path, r"\[editor\] window created", process, 120, "startup"
        )
        wait_for_token(
            log_path, r"\[editor\] scene surface attached\s+detached=false",
            process, 120, "scene surface",
        )
        wait_for_token(
            log_path, r"\[editor\] pill_editor_shell_editor loaded", process,
            180, "shell load",
        )
        heartbeat = wait_for_token(
            log_path, r"\[editor\] shell frame\s+frames=(\d+)\s+panels=(\d+)",
            process, 180, "shell heartbeat",
        )
        match = re.search(r"frames=(\d+)\s+panels=(\d+)", heartbeat)
        assert match and int(match.group(2)) > 0, f"no panels painted: {heartbeat}"
        print(f"  [OK] shell loaded, {heartbeat}")

        # Step 1b: the classic layout was migrated once - the module panel
        # dropped by name - into the native layout file.
        wait_for_token(
            log_path, r"layout migration dropped panel", process, 60,
            "migration drop",
        )
        wait_for_token(
            log_path, r"classic layout migrated to the native layout file",
            process, 60, "migration",
        )
        assert native_layout.is_file(), "the native layout file was not written"
        assert "ghost_panel" not in native_layout.read_text(encoding="utf-8"), (
            "the migrated layout kept a dropped module panel"
        )
        print("  [OK] classic layout migrated once, module panel dropped")

        # Step 2: input routing - a click inside the scene view goes to the
        # editor camera's view (never the game), one on a panel goes to the
        # UI. The classic fixture has no Game tab, so the scene rectangle
        # belongs to the scene view here.
        append_command(commands_path, "click 600 300")
        wait_for_token(
            log_path,
            r'\[editor\] test click\s+x=600(?:\.0)?\s+y=300(?:\.0)?\s+target="?scene"?',
            process,
            30,
            "scene click",
        )
        append_command(commands_path, "click 40 780")
        wait_for_token(
            log_path,
            r'\[editor\] test click\s+x=40(?:\.0)?\s+y=780(?:\.0)?\s+target="?ui"?',
            process,
            30,
            "ui click",
        )
        print("  [OK] input routing: scene and UI targets")

        # Step 3: a live patch of the shell's system, state kept.
        before = [
            int(entry)
            for entry in re.findall(
                r"\[editor\] shell frame\s+frames=(\d+)", read_log(log_path)
            )
        ]
        highest = max(before)
        cadence = re.search(r"% (\d+) == 0", original_source)
        assert cadence, "the shell system's heartbeat cadence was not found"
        patched = original_source.replace(
            cadence.group(0), f"% {int(cadence.group(1)) + 1} == 0", 1
        )
        shell_source.write_text(patched, encoding="utf-8")
        wait_for_token(
            log_path,
            r"LIVE \d+ ms|extension reload processed",
            process,
            180,
            "shell patch",
        )
        seen_after = {}
        wait_for_token(
            log_path,
            rf"\[editor\] shell frame\s+frames=(\d+)",
            process,
            120,
            "post-patch heartbeat",
        )
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            for entry in re.findall(
                r"\[editor\] shell frame\s+frames=(\d+)", read_log(log_path)
            ):
                seen_after[int(entry)] = True
            if any(frame > highest for frame in seen_after):
                break
            time.sleep(0.5)
        else:
            raise TimeoutError(
                "the shell's frame counter did not continue past "
                f"{highest} after the patch; its state was reset\n"
                + read_log(log_path)[-4000:]
            )
        print(f"  [OK] shell live-patched, frame counter continued past {highest}")

        # Step 4: a graceful quit, with the clean exit code.
        append_command(commands_path, "quit")
        wait_for_token(
            log_path, r"\[editor\] test channel: quit requested", process, 30,
            "quit command",
        )
        status = process.wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in read_log(log_path), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        shell_source.write_text(original_source, encoding="utf-8")
        if process.poll() is None:
            process.kill()
            process.wait(timeout=30)

    # Step 4b: a fresh default workspace - both layout files gone - which is
    # the layout that carries the Game tab beside the Scene tab (D29). The
    # Game view draws the project into its own surface; the scene view keeps
    # the editor camera; input reaches whichever view is selected, and the
    # UI's keys reach neither.
    for stale in (state_directory / "editor_layout.json", native_layout):
        if stale.exists():
            stale.unlink()
    game_commands = logs / "commands-game.txt"
    game_commands.write_text("", encoding="utf-8")
    game_log = logs / "editor-native-game.log"
    game_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        game_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(game_commands),
        },
    )
    try:
        wait_for_token(
            game_log, r"\[editor\] window created", game_process, 120, "game run"
        )
        wait_for_token(
            game_log, r"\[editor\] pill_editor_shell_editor loaded", game_process,
            180, "game run shell",
        )
        # The default workspace starts on the Scene tab, so the game view
        # exists but is hidden.
        wait_for_token(
            game_log, r'\[editor\] view hidden\s+view="game"', game_process, 120,
            "game view hidden",
        )
        print("  [OK] default workspace: Game tab beside Scene, game view hidden")

        # Switching to the Game tab selects it and shows the game surface in
        # the content area; the click below is placed inside that rectangle.
        append_command(game_commands, "tab game")
        wait_for_token(
            game_log, r"\[editor\] tab selected\s+title=game", game_process, 60,
            "game tab",
        )
        shown = wait_for_token(
            game_log,
            r'\[editor\] view shown\s+view="game"\s+x=([\d.]+)\s+y=([\d.]+)'
            r"\s+width=([\d.]+)\s+height=([\d.]+)",
            game_process,
            60,
            "game view shown",
        )
        numbers = [float(entry) for entry in re.findall(r"=([\d.]+)", shown)]
        center_x = numbers[0] + numbers[2] / 2.0
        center_y = numbers[1] + numbers[3] / 2.0
        print(f"  [OK] Game tab selected, game surface shown ({shown[-60:]})")

        # A click inside the game view reaches the game; a key with the game
        # target reaches it too.
        append_command(game_commands, f"click {center_x:.0f} {center_y:.0f}")
        wait_for_token(
            game_log,
            rf'\[editor\] test click\s+x={center_x:.0f}(?:\.0)?\s+'
            rf'y={center_y:.0f}(?:\.0)?\s+target="?game"?',
            game_process,
            30,
            "game click",
        )
        append_command(game_commands, "key w game")
        wait_for_token(
            game_log, r'\[editor\] test key\s+key=w\s+target="?game"?', game_process,
            30, "game key",
        )

        # Switching back shows the scene surface; a click there reaches the
        # scene view (never the game) and a key moves the editor camera.
        append_command(game_commands, "tab scene")
        wait_for_token(
            game_log, r"\[editor\] tab selected\s+title=scene", game_process, 60,
            "scene tab",
        )
        wait_for_token(
            game_log, r'\[editor\] view shown\s+view="scene"', game_process, 60,
            "scene view shown",
        )
        append_command(game_commands, f"click {center_x:.0f} {center_y:.0f}")
        wait_for_token(
            game_log,
            rf'\[editor\] test click\s+x={center_x:.0f}(?:\.0)?\s+'
            rf'y={center_y:.0f}(?:\.0)?\s+target="?scene"?',
            game_process,
            30,
            "scene click",
        )
        append_command(game_commands, "key w scene")
        wait_for_token(
            game_log, r'\[editor\] test key\s+key=w\s+target="?scene"?', game_process,
            30, "scene key",
        )
        wait_for_token(
            game_log, r"\[editor\] editor camera moved\s+reason=", game_process, 30,
            "editor camera",
        )
        print("  [OK] input routed by the selected view; scene keys move the editor camera")

        # A key without a view target is the UI's, and reaches neither view.
        append_command(game_commands, "key escape")
        wait_for_token(
            game_log, r'\[editor\] test key\s+key=escape\s+target="?ui"?',
            game_process, 30, "ui key",
        )
        print("  [OK] untargeted keys go to the UI only")

        append_command(game_commands, "quit")
        wait_for_token(
            game_log, r"\[editor\] test channel: quit requested", game_process, 30,
            "game run quit",
        )
        status = game_process.wait(timeout=60)
        assert status == 0, f"game view run exited {status}"
        print("  [OK] game view run exited 0")
    finally:
        if game_process.poll() is None:
            game_process.kill()
            game_process.wait(timeout=30)

    # Step 5: the clean exit code again, on further runs, quitting by the
    # scripted seconds deadline instead of the command file.
    for run in range(1, arguments.exit_runs + 1):
        exit_log = logs / f"editor-native-exit-{run}.log"
        exit_process = start_editor(
            binary,
            arguments.project,
            working_directory,
            exit_log,
            {"LOCALAPPDATA": str(appdata), "PILL_EDITOR_TEST_SECONDS": "3"},
        )
        wait_for_token(
            exit_log, r"\[editor\] window created", exit_process, 120, "exit run"
        )
        status = exit_process.wait(timeout=120)
        assert status == 0, (
            f"exit run {run}: editor exited {status}\n" + read_log(exit_log)[-4000:]
        )
    print(f"  [OK] {arguments.exit_runs} further runs exited 0")

    # Step 6: with a child scene window forced to fail, the scene must open
    # detached, still render and exit cleanly.
    detached_log = logs / "editor-native-detached.log"
    detached_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        detached_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_SECONDS": "10",
            "PILL_EDITOR_DETACHED_SCENE": "1",
        },
    )
    wait_for_token(
        detached_log,
        r"\[editor\] child scene window forced off|child scene window refused",
        detached_process,
        120,
        "detached fallback",
    )
    wait_for_token(
        detached_log,
        r"\[editor\] scene surface attached\s+detached=true",
        detached_process,
        120,
        "detached attach",
    )
    wait_for_token(
        detached_log, r"\[editor\] scene frame report", detached_process, 120,
        "detached frames",
    )
    status = detached_process.wait(timeout=120)
    assert status == 0, f"detached fallback exited {status}"
    print("  [OK] forced detached fallback: attached, rendered, exited 0")
    print("PASS: editor native end-to-end")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
