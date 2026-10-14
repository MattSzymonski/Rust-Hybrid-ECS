"""End-to-end acceptance for the native editor.

PROTECTS: The native editor: startup, shell module, input routing, live patch, overlays, safe mode.
TIER: editor

Builds against an already-built `pill_editor_native` binary, starts it on
`italian_brainrot` - a three-entity project, so the frames are fast and the
suite's heartbeat waits are short - with the scripted input channel
(`PILL_EDITOR_TEST_COMMANDS`), and asserts on its log tokens: startup, the
shell module loading, panel dispatch, input routing between the scene view
and the editor UI, the Game view on its own surface with editor-camera
navigation in the scene view, a live patch of the shell's system keeping its
state, and a clean exit. Then it checks the clean exit code on five further
runs. (The 200k-entity `circus_demo` this suite used to start is the
pill-lab editor-shell measurement's job; it only made these phases race the
executable's seconds deadline.) It also covers the safe-mode paths - a shell
that never loads, and a shell generation that panics on every frame - the
Retry button that brings the shell back, and the redraw-on-change idle check
(the executable's own `dump ui-frames` counter through the scripted channel).

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


def wait_for_occurrence(
    path: Path,
    pattern: str,
    occurrence: int,
    process: subprocess.Popen,
    timeout: float,
    label: str,
) -> str:
    """Waits until `pattern` has appeared at least `occurrence` times.

    A token a run repeats - a window move, a popup dismissal - must be
    waited for by count: the earlier occurrence is already in the log and
    would satisfy a plain search before the new one has happened.
    """
    deadline = time.monotonic() + timeout
    expression = re.compile(pattern)
    while time.monotonic() < deadline:
        matches = [match.group(0) for match in expression.finditer(read_log(path))]
        if len(matches) >= occurrence:
            return matches[occurrence - 1]
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode} before "
                f"occurrence {occurrence} of {pattern}\n" + read_log(path)[-4000:]
            )
        time.sleep(0.5)
    raise TimeoutError(
        f"{label}: timeout waiting for occurrence {occurrence} of {pattern}\n"
        + read_log(path)[-4000:]
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
    what a user runs, and - since the shell's dylibs are staged beside it - a directly
    spawned editor resolves the shell's engine dylibs (staged beside the
    module it loads) without cargo's PATH help, so this launch keeps that
    fix proven. `cargo run` was the historical workaround and is no longer
    needed.
    """
    environment = dict(os.environ)
    # Never inherit the scripted channel from the calling shell: a stale
    # value would feed the editor another run's commands (each launch here
    # either names its own file in `extra_environment` or runs without one).
    environment.pop("PILL_EDITOR_TEST_COMMANDS", None)
    environment.pop("PILL_EDITOR_TEST_SECONDS", None)
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
        default="../examples/italian_brainrot",
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
    # grows its X11 and Wayland implementations; elsewhere this
    # suite has nothing to check.
    if os.name != "nt":
        print("SKIP: the native editor suites need Windows")
        return 0
    # A stale editor from an interrupted run would fight this one over the
    # module build directory; every other host-driving suite does the same.
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    # A concurrent cargo build rewrites the shared dylibs under the modules
    # this suite loads, which turns a clean start into a loader failure:
    # detect it and say so, instead of failing on a
    # load whose log cannot tell the difference.
    cargo = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq cargo.exe", "/NH"],
        capture_output=True,
        text=True,
    ).stdout
    if "cargo.exe" in cargo:
        raise SystemExit(
            "a cargo build (cargo.exe) is running; wait for it to finish "
            "before running the editor suite - it rewrites the shared dylibs "
            "the modules load against"
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
            # The phases wait for two shell heartbeats and a patch compile;
            # the executable's seconds deadline is a hang guard, kept
            # comfortably above the phases even on a slow machine.
            "PILL_EDITOR_TEST_SECONDS": "120",
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

    # Step 4c: the overlay layer and the platform popups (D23/D24).
    # The FPS badge and the docking drop preview float over the scene's own
    # child window on a DirectComposition surface, and the menus, the
    # command palette and the context menu open as windows of their own,
    # placed and flipped against the monitor's work area. The overlay
    # publishes the rectangles it covers, so a click on the badge belongs to
    # the UI and one beside it to the scene underneath.
    overlay_commands = logs / "commands-overlay.txt"
    overlay_commands.write_text("", encoding="utf-8")
    overlay_log = logs / "editor-native-overlay.log"
    overlay_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        overlay_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(overlay_commands),
            "PILL_EDITOR_TEST_SECONDS": "180",
        },
    )
    try:
        wait_for_token(
            overlay_log, r"\[editor\] overlay created\s+width=(\d+)\s+height=(\d+)",
            overlay_process, 180, "overlay created",
        )
        # The monitor's work area, which the popup placement flips and slides
        # inside; the platform is the only thing that knows it.
        work = wait_for_token(
            overlay_log,
            r"\[editor\] monitor work area\s+x=(-?\d+)\s+y=(-?\d+)\s+"
            r"width=(\d+)\s+height=(\d+)\s+scale=([\d.]+)",
            overlay_process,
            60,
            "work area",
        )
        work_x, work_y, work_width, work_height, scale = [
            float(entry) for entry in re.findall(r"=(-?[\d.]+)", work)
        ]
        window = wait_for_token(
            overlay_log, r"\[editor\] window created\s+width=(\d+)\s+height=(\d+)",
            overlay_process, 60, "window size",
        )
        window_width = float(re.search(r"width=(\d+)", window).group(1))
        # The overlay carries the UI the panels cannot: the badge and the
        # drop preview, drawn through the composition path, with the opaque
        # rectangles the input routing uses.
        content = wait_for_token(
            overlay_log,
            r"\[editor\] overlay content\s+path=\"composition\"\s+forced=false\s+"
            r"frames=\d+\s+vertices=([1-9]\d*)\s+opaque=([1-9]\d*)",
            overlay_process,
            120,
            "overlay content",
        )
        vertices = int(re.search(r"vertices=(\d+)", content).group(1))
        opaque = int(re.search(r"opaque=(\d+)", content).group(1))
        assert vertices > 0, f"the composition overlay drew nothing: {content}"
        assert opaque > 0, f"the overlay published no opaque region: {content}"
        print(f"  [OK] composition overlay carries {vertices} vertices, {opaque} opaque regions")

        # A click on the badge reaches the UI; one beside it, inside the same
        # scene view, reaches the scene. The badge is the view's top-right
        # corner (its size is the shell's, in logical pixels).
        view = wait_for_token(
            overlay_log,
            r'\[editor\] view shown\s+view="scene"\s+x=([\d.]+)\s+y=([\d.]+)\s+'
            r"width=([\d.]+)\s+height=([\d.]+)",
            overlay_process,
            120,
            "scene view",
        )
        view_x, view_y, view_width, view_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", view)
        ]
        badge_x = view_x + view_width - 76.0 - 8.0
        badge_y = view_y + 8.0
        append_command(overlay_commands, f"click {badge_x + 38:.0f} {badge_y + 10:.0f}")
        wait_for_token(
            overlay_log,
            rf"\[editor\] test click\s+x={badge_x + 38:.0f}(?:\.0)?\s+"
            rf"y={badge_y + 10:.0f}(?:\.0)?\s+target=\"?ui\"?\s+overlay=true",
            overlay_process,
            30,
            "badge click",
        )
        append_command(
            overlay_commands, f"click {view_x + 40:.0f} {view_y + view_height - 40:.0f}"
        )
        wait_for_token(
            overlay_log,
            rf"\[editor\] test click\s+x={view_x + 40:.0f}(?:\.0)?\s+"
            rf"y={view_y + view_height - 40:.0f}(?:\.0)?\s+target=\"?scene\"?\s+overlay=false",
            overlay_process,
            30,
            "scene click beside the badge",
        )
        wait_for_token(
            overlay_log, r"\[editor\] scene view click", overlay_process, 30,
            "scene click routed",
        )
        print("  [OK] badge click reached the UI, the scene beside it the view")

        # A menu title opens its dropdown as a popup window of its own; a
        # click outside dismisses it the way the platform does.
        append_command(overlay_commands, "click 40 13")
        wait_for_token(
            overlay_log, r"\[editor\] menu opened\s+path=", overlay_process, 30,
            "menu opened",
        )
        wait_for_token(
            overlay_log,
            r"\[editor\] popup opened id=menu size=\((\d+), (\d+)\)\s+"
            r"requested=\((-?\d+), (-?\d+)\)\s+placed=\((-?\d+), (-?\d+)\)\s+"
            r"flipped=false slid=false",
            overlay_process,
            30,
            "menu popup",
        )
        append_command(overlay_commands, f"click {view_x + 40:.0f} {view_y + 40:.0f}")
        wait_for_token(
            overlay_log, r"\[editor\] popup dismissed\s+popup=\"menu\"", overlay_process,
            30, "menu dismissed",
        )
        print("  [OK] menu dropdown is a popup window, dismissed by an outside click")

        # The command palette is the second popup, with its own text field.
        append_command(overlay_commands, "command editor.command_palette")
        wait_for_token(
            overlay_log, r"\[editor\] command palette opened", overlay_process, 30,
            "palette opened",
        )
        wait_for_token(
            overlay_log, r"\[editor\] popup opened id=palette\s+size=",
            overlay_process, 30, "palette popup",
        )
        append_command(overlay_commands, "popup-dismiss palette")
        wait_for_token(
            overlay_log, r"\[editor\] test popup dismiss\s+id=palette", overlay_process,
            30, "palette dismiss",
        )
        wait_for_token(
            overlay_log, r"\[editor\] popup dismissed\s+popup=\"palette\"", overlay_process,
            30, "palette dismissed",
        )
        print("  [OK] palette popup opened and dismissed through the platform channel")

        # A right-click asks for a context menu: a window of its own, placed
        # at the click, and its rows activated through the popup's pointer.
        append_command(overlay_commands, "rightclick 400 300")
        wait_for_token(
            overlay_log, r"\[editor\] context menu opened\s+x=400(?:\.0)?", overlay_process,
            30, "context menu asked",
        )
        wait_for_token(
            overlay_log,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            r"requested=\(400(?:\.0)?, 300(?:\.0)?\)\s+placed=\((-?\d+), (-?\d+)\)\s+"
            r"flipped=false slid=false",
            overlay_process,
            30,
            "context popup",
        )
        append_command(overlay_commands, "popup-pointer context 110 16 release")
        wait_for_token(
            overlay_log, r"\[editor\] test popup pointer\s+id=context", overlay_process,
            30, "popup pointer",
        )
        # The first row is Undo: with an empty history that is its own log.
        wait_for_token(
            overlay_log, r"\[editor\] history undo", overlay_process, 30,
            "popup row command",
        )
        wait_for_occurrence(
            overlay_log, r"\[editor\] popup dismissed\s+popup=\"context\"", 1,
            overlay_process, 30, "context dismissed",
        )
        print("  [OK] context menu popup: rows run their commands, then it closes")

        # A menu near the window's right edge extends past the window - it is
        # a window of its own, so the client area no longer clips it - and it
        # flips to the other side of the click when the monitor's edge is
        # reached. The window is moved so its right edge sits inside the work
        # area, then at it, and the click lands on the client's right edge.
        client_width = window_width / scale
        click_x = round(client_width - 20.0)
        window_right = work_x + work_width - 300.0 * scale
        append_command(
            overlay_commands, f"window-move {round(window_right - window_width)} {round(work_y) + 40}"
        )
        wait_for_occurrence(
            overlay_log, r"\[editor\] test window move", 1, overlay_process, 30,
            "window moved",
        )
        append_command(overlay_commands, f"rightclick {click_x} 300")
        opened = wait_for_token(
            overlay_log,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            rf"requested=\({click_x}(?:\.0)?, 300(?:\.0)?\)\s+"
            r"placed=\((-?\d+), (-?\d+)\)\s+flipped=false slid=false",
            overlay_process,
            30,
            "context popup at the edge",
        )
        numbers = re.search(r"placed=\((-?\d+), (-?\d+)\)", opened)
        placed_x = int(numbers.group(1))
        size_x = int(re.search(r"size=\((\d+),", opened).group(1))
        assert placed_x + size_x > window_right, (
            f"the menu did not extend past the window: {opened}, "
            f"window right edge {window_right}"
        )
        assert placed_x + size_x <= work_x + work_width, (
            f"the menu left the monitor's work area: {opened}"
        )
        print(
            f"  [OK] menu at the window's right edge extends past it "
            f"({placed_x + size_x:.0f} > {window_right:.0f}), inside the work area"
        )
        # Now with the window's right edge at the monitor's edge, the same
        # click must flip the menu to the other side of the anchor.
        append_command(overlay_commands, f"click {view_x + 40:.0f} {view_y + 40:.0f}")
        wait_for_occurrence(
            overlay_log, r"\[editor\] popup dismissed\s+popup=\"context\"", 2,
            overlay_process, 30, "context dismissed before the flip",
        )
        append_command(
            overlay_commands,
            f"window-move {round(work_x + work_width - window_width)} {round(work_y) + 40}",
        )
        wait_for_occurrence(
            overlay_log, r"\[editor\] test window move", 2, overlay_process, 30,
            "window moved to the edge",
        )
        append_command(overlay_commands, f"rightclick {click_x} 300")
        flipped = wait_for_token(
            overlay_log,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            rf"requested=\({click_x}(?:\.0)?, 300(?:\.0)?\)\s+"
            r"placed=\((-?\d+), (-?\d+)\)\s+flipped=true slid=false",
            overlay_process,
            30,
            "context popup flipped",
        )
        flipped_x = int(re.findall(r"placed=\((-?\d+),", flipped)[0])
        assert flipped_x >= work_x, f"the flipped menu left the work area: {flipped}"
        assert flipped_x + size_x <= work_x + work_width, (
            f"the flipped menu left the work area: {flipped}"
        )
        print(f"  [OK] menu flipped at the monitor's edge, placed at x={flipped_x}")

        append_command(overlay_commands, "quit")
        wait_for_token(
            overlay_log, r"\[editor\] test channel: quit requested", overlay_process, 30,
            "overlay run quit",
        )
        status = overlay_process.wait(timeout=60)
        assert status == 0, f"the overlay run exited {status}"
        assert "panic" not in read_log(overlay_log), "a panic was logged"
        print("  [OK] overlay and popup run exited 0")
    finally:
        if overlay_process.poll() is None:
            overlay_process.kill()
            overlay_process.wait(timeout=30)

    # Step 4d: with the composition overlay forced to fail, the same content
    # must reach the scene through the renderer's UI pass (the overlay
    # fallback), with one warning and no loss.
    fallback_log = logs / "editor-native-overlay-fallback.log"
    fallback_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        fallback_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_NO_OVERLAY": "1",
            "PILL_EDITOR_TEST_SECONDS": "20",
        },
    )
    try:
        wait_for_token(
            fallback_log,
            r"\[editor\] overlay disabled by PILL_EDITOR_NO_OVERLAY; "
            r"the UI falls back to the scene pass",
            fallback_process,
            180,
            "overlay forced failure",
        )
        fallback = wait_for_token(
            fallback_log,
            r"\[editor\] overlay content\s+path=\"scene-pass\"\s+forced=true\s+"
            r"frames=0\s+vertices=([1-9]\d*)\s+opaque=([1-9]\d*)",
            fallback_process,
            120,
            "fallback content",
        )
        vertices = int(re.search(r"vertices=(\d+)", fallback).group(1))
        opaque = int(re.search(r"opaque=(\d+)", fallback).group(1))
        assert vertices > 0, f"the fallback UI pass got nothing to draw: {fallback}"
        assert opaque > 0, f"the fallback published no opaque region: {fallback}"
        assert "overlay created" not in read_log(fallback_log), (
            "the composition overlay was created despite the forced failure"
        )
        status = fallback_process.wait(timeout=120)
        assert status == 0, f"the fallback run exited {status}"
        print(
            f"  [OK] forced overlay failure: the same {vertices} vertices went "
            "through the scene pass"
        )
    finally:
        if fallback_process.poll() is None:
            fallback_process.kill()
            fallback_process.wait(timeout=30)

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

    # Step 7: safe mode after a failed first load (D33). The
    # shell's sources do not build at startup, so no shell generation ever
    # loads; the editor must stay up - not exit - with its built-in
    # safe-mode UI: the shell's build errors as rows, the scene, and a
    # Retry button. Fixing the source and clicking Retry loads the shell
    # and leaves safe mode with the saved layout.
    broken_source = original_source.replace(
        "let editor_frames_per_second = state.readout_frames_per_second;",
        "let editor_frames_per_second = ;",
        1,
    )
    assert broken_source != original_source, "the shell breakage anchor was not found"
    shell_source.write_text(broken_source, encoding="utf-8")
    safe_commands = logs / "commands-safe-mode.txt"
    safe_commands.write_text("", encoding="utf-8")
    safe_log = logs / "editor-native-safe-mode.log"
    safe_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        safe_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(safe_commands),
            "PILL_EDITOR_TEST_SECONDS": "300",
        },
    )
    try:
        wait_for_token(safe_log, r"\[editor\] window created", safe_process, 180, "safe run")
        wait_for_token(
            safe_log,
            r"optional module failed to load; the host continues without it",
            safe_process,
            240,
            "tolerated shell failure",
        )
        wait_for_token(
            safe_log,
            r'\[editor\] problem\s+module="?pill_editor_shell_editor"?\s+'
            r'level="?error"?\s+file="[^"]*pill_editor_shell_editor\.rs"\s+line=\d+',
            safe_process,
            120,
            "shell build problem rows",
        )
        wait_for_token(
            safe_log,
            r'\[editor\] safe mode entered: no shell generation is running\s+'
            r'reason="?shell-load-failed"?',
            safe_process,
            120,
            "safe mode entered",
        )
        retry = wait_for_token(
            safe_log,
            r"\[editor\] safe mode retry button\s+x=([\d.]+)\s+y=([\d.]+)\s+"
            r"width=([\d.]+)\s+height=([\d.]+)",
            safe_process,
            60,
            "retry button",
        )
        retry_x, retry_y, retry_width, retry_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", retry)
        ]
        # The safe layout keeps the Scene view: a click in the right column
        # routes to the scene, not to the safe-mode UI.
        append_command(safe_commands, "click 900 300")
        wait_for_token(
            safe_log,
            r'\[editor\] test click\s+x=900(?:\.0)?\s+y=300(?:\.0)?\s+target="?scene"?',
            safe_process,
            30,
            "safe mode scene view",
        )
        print(
            f"  [OK] safe mode after a failed load; retry at "
            f"({retry_x:.0f}, {retry_y:.0f})"
        )
        # Fix the source and click Retry; the rebuild and load run inside
        # the click's own frame, so this wait covers the build too.
        shell_source.write_text(original_source, encoding="utf-8")
        append_command(
            safe_commands,
            f"click {retry_x + retry_width / 2:.0f} {retry_y + retry_height / 2:.0f}",
        )
        wait_for_token(
            safe_log, r"\[editor\] safe mode retry requested", safe_process, 300,
            "retry request",
        )
        wait_for_token(
            safe_log, r"\[editor\] pill_editor_shell_editor loaded", safe_process, 120,
            "shell load after retry",
        )
        wait_for_token(
            safe_log, r"\[editor\] safe mode left: the shell runs again", safe_process,
            120, "safe mode left",
        )
        wait_for_token(
            safe_log, r"\[editor\] shell frame\s+frames=(\d+)\s+panels=([1-9]\d*)",
            safe_process, 120, "panels after retry",
        )
        print("  [OK] retry loaded the shell and left safe mode, layout painted")
        append_command(safe_commands, "quit")
        wait_for_token(
            safe_log, r"\[editor\] test channel: quit requested", safe_process, 30,
            "safe mode quit",
        )
        status = safe_process.wait(timeout=60)
        assert status == 0, f"the safe-mode run exited {status}"
        print("  [OK] safe-mode run exited 0")
    finally:
        shell_source.write_text(original_source, encoding="utf-8")
        if safe_process.poll() is None:
            safe_process.kill()
            safe_process.wait(timeout=30)

    # Step 8: safe mode with a shell that never gets going (D33):
    # the first generation loads but panics on every run, so no frame is
    # ever stamped. Retrying the same panicking code stays in safe mode;
    # fixing the source reloads it through the watcher and leaves it.
    panic_anchor = (
        "    let (mut state, mut input, mut frame, editor_snapshot, logs, "
        "clipboard_answer) = view;"
    )
    panicking_source = original_source.replace(
        panic_anchor,
        '    panic!("safe mode probe: the shell generation panics on purpose");\n'
        + panic_anchor,
        1,
    )
    assert panicking_source != original_source, "the shell panic anchor was not found"
    shell_source.write_text(panicking_source, encoding="utf-8")
    stopped_commands = logs / "commands-safe-stopped.txt"
    stopped_commands.write_text("", encoding="utf-8")
    stopped_log = logs / "editor-native-safe-stopped.log"
    stopped_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        stopped_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(stopped_commands),
            "PILL_EDITOR_TEST_SECONDS": "300",
        },
    )
    live_pattern = r"LIVE \d+ ms|extension reload processed"
    try:
        wait_for_token(
            stopped_log, r"\[editor\] window created", stopped_process, 180,
            "stopped run",
        )
        wait_for_token(
            stopped_log, r"\[editor\] pill_editor_shell_editor loaded",
            stopped_process, 240, "stopped run shell load",
        )
        wait_for_token(
            stopped_log,
            r'\[editor\] safe mode entered: no shell generation is running\s+'
            r'reason="?shell-stopped"?',
            stopped_process,
            180,
            "safe mode entered (stopped)",
        )
        retry = wait_for_token(
            stopped_log,
            r"\[editor\] safe mode retry button\s+x=([\d.]+)\s+y=([\d.]+)\s+"
            r"width=([\d.]+)\s+height=([\d.]+)",
            stopped_process,
            60,
            "retry button (stopped)",
        )
        retry_x, retry_y, retry_width, retry_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", retry)
        ]
        live_seen = len(re.findall(live_pattern, read_log(stopped_log)))
        append_command(
            stopped_commands,
            f"click {retry_x + retry_width / 2:.0f} {retry_y + retry_height / 2:.0f}",
        )
        wait_for_token(
            stopped_log, r"\[editor\] safe mode retry requested", stopped_process, 60,
            "retry request (stopped)",
        )
        # The retry rebuilds and reloads the same panicking code; the shell
        # must stay in safe mode.
        wait_for_occurrence(
            stopped_log, live_pattern, live_seen + 1, stopped_process, 240,
            "retry reload",
        )
        time.sleep(2.0)
        assert "safe mode left" not in read_log(stopped_log), (
            "safe mode left while the shell's code still panics"
        )
        print("  [OK] safe mode with a panicking shell; retry kept waiting")
        # Fix the source: the watcher reloads it and the frames resume.
        shell_source.write_text(original_source, encoding="utf-8")
        wait_for_occurrence(
            stopped_log, live_pattern, live_seen + 2, stopped_process, 240,
            "recovery reload",
        )
        wait_for_token(
            stopped_log, r"\[editor\] safe mode left: the shell runs again",
            stopped_process, 120, "safe mode left (stopped)",
        )
        wait_for_token(
            stopped_log, r"\[editor\] shell frame\s+frames=(\d+)\s+panels=([1-9]\d*)",
            stopped_process, 120, "panels after recovery",
        )
        print("  [OK] fixing the source reloaded the shell and left safe mode")
        append_command(stopped_commands, "quit")
        wait_for_token(
            stopped_log, r"\[editor\] test channel: quit requested", stopped_process,
            30, "stopped run quit",
        )
        status = stopped_process.wait(timeout=60)
        assert status == 0, f"the panicking-shell run exited {status}"
        print("  [OK] panicking-shell run exited 0")
    finally:
        shell_source.write_text(original_source, encoding="utf-8")
        if stopped_process.poll() is None:
            stopped_process.kill()
            stopped_process.wait(timeout=30)

    # Step 9: redraw on change (D25). With a static layout (one
    # Scene tabset, no live Statistics text), the project paused and no
    # input, the editor must not present UI frames; a pointer move onto a
    # menu title must produce one. The counter is the executable's own,
    # read through the scripted channel; the one thing still allowed to
    # change is the badge's integer frame-rate text, so the quiet check
    # tolerates the odd present it may cause.
    STATIC_LAYOUT = """{
  "schema_version": 2,
  "root": 1,
  "nodes": {
    "1": { "type": "tab_set", "tabs": [2], "selected": 2 },
    "2": { "type": "tab", "title": "Scene", "panel": "scene", "closeable": false }
  },
  "active_tabset": 1,
  "next_id": 3
}
"""
    native_layout.write_text(STATIC_LAYOUT, encoding="utf-8")
    idle_commands = logs / "commands-idle.txt"
    idle_commands.write_text("", encoding="utf-8")
    idle_log = logs / "editor-native-idle.log"
    idle_process = start_editor(
        binary,
        arguments.project,
        working_directory,
        idle_log,
        {
            "LOCALAPPDATA": str(appdata),
            "PILL_EDITOR_TEST_COMMANDS": str(idle_commands),
            "PILL_EDITOR_TEST_SECONDS": "180",
        },
    )
    try:
        wait_for_token(
            idle_log, r"\[editor\] pill_editor_shell_editor loaded", idle_process,
            180, "idle run",
        )
        wait_for_token(
            idle_log, r"\[editor\] shell frame\s+frames=\d+\s+panels=([1-9]\d*)",
            idle_process, 120, "idle layout",
        )
        # Pause the project's systems; the shell's own system keeps running
        # (that is what safe mode's liveness check relies on).
        append_command(idle_commands, "command editor.pause_toggle")
        wait_for_token(
            idle_log, r"\[editor\] test channel: command request", idle_process, 30,
            "pause request",
        )
        time.sleep(2.0)

        def ui_frames_so_far() -> int:
            """The presented-frame count, by asking the editor to dump it."""
            pattern = r"\[editor\] ui frames\s+presented=(\d+)"
            seen = len(re.findall(pattern, read_log(idle_log)))
            append_command(idle_commands, "dump ui-frames")
            wait_for_occurrence(
                idle_log, pattern, seen + 1, idle_process, 60, "ui frames dump"
            )
            return int(re.findall(pattern, read_log(idle_log))[-1])

        first = ui_frames_so_far()
        time.sleep(5.0)
        second = ui_frames_so_far()
        assert second - first <= 2, (
            f"the idle editor kept presenting UI frames: {first} -> {second}"
        )
        print(
            f"  [OK] idle: {second - first} UI frames in five seconds"
        )
        append_command(idle_commands, "move 40 13")
        wait_for_token(
            idle_log, r"\[editor\] test move\s+x=40(?:\.0)?\s+y=13(?:\.0)?",
            idle_process, 30, "move command",
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if ui_frames_so_far() > second:
                break
        else:
            raise TimeoutError(
                "hovering the UI did not redraw it\n" + read_log(idle_log)[-4000:]
            )
        print("  [OK] a pointer move onto the UI redrew it")
        append_command(idle_commands, "quit")
        wait_for_token(
            idle_log, r"\[editor\] test channel: quit requested", idle_process, 30,
            "idle run quit",
        )
        status = idle_process.wait(timeout=60)
        assert status == 0, f"the idle run exited {status}"
        print("  [OK] idle run exited 0")
    finally:
        if idle_process.poll() is None:
            idle_process.kill()
            idle_process.wait(timeout=30)

    print("PASS: editor native end-to-end")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
