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

Each run keeps its log, sandbox and command file under
`local/editor-validation/native-<run id>/`, one file set per session; the
runs share that one sandbox on purpose - the classic-layout migration in
the first run is exactly what the later runs must see - and the suite's own
edits of the shell module are restored when the session that made them
closes.

The only files this suite edits are the shell module's source (restored
when its session closes) and its own sandboxed layout files under
`LOCALAPPDATA`; everything else it starts is its own process tree.
"""
import argparse
import re
import subprocess
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_HISTORY_UNDO_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_TEST_QUIT_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
    run_directory,
)
from core.paths import MODULES_ROOT, REPOSITORY_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources. Patterns that
# embed runtime values (frame counts, placements) compose them with their
# regex suffix at the wait.
TEST_CLICK_TOKEN = "[editor] test click"
TEST_KEY_TOKEN = "[editor] test key"
TEST_MOVE_TOKEN = "[editor] test move"
TEST_COMMAND_REQUEST_TOKEN = "[editor] test channel: command request"
SHELL_FRAME_TOKEN = "[editor] shell frame"
SCENE_SURFACE_TOKEN = "[editor] scene surface attached"
VIEW_SHOWN_TOKEN = "[editor] view shown"
VIEW_HIDDEN_TOKEN = "[editor] view hidden"
LAYOUT_MIGRATION_TOKEN = "[editor] layout migration dropped panel"
SCENE_FRAME_REPORT_TOKEN = "scene frame report"
CLASSIC_MIGRATED_TOKEN = "classic layout migrated to the native layout file"
OVERLAY_CREATED_TOKEN = "[editor] overlay created"
WORK_AREA_TOKEN = "[editor] monitor work area"
OVERLAY_CONTENT_TOKEN = "[editor] overlay content"
OVERLAY_DISABLED_TOKEN = "[editor] overlay disabled by PILL_EDITOR_NO_OVERLAY"
MENU_OPENED_TOKEN = "[editor] menu opened"
PALETTE_OPENED_TOKEN = "[editor] command palette opened"
CONTEXT_MENU_TOKEN = "[editor] context menu opened"
TEST_POPUP_DISMISS_TOKEN = "[editor] test popup dismiss"
TEST_POPUP_POINTER_TOKEN = "[editor] test popup pointer"
TEST_WINDOW_MOVE_TOKEN = "[editor] test window move"
POPUP_DISMISSED_TOKEN = "[editor] popup dismissed"
SCENE_VIEW_CLICK_TOKEN = "[editor] scene view click"
EDITOR_CAMERA_TOKEN = "[editor] editor camera moved"
SAFE_ENTERED_TOKEN = "[editor] safe mode entered: no shell generation is running"
SAFE_RETRY_TOKEN = "[editor] safe mode retry button"
SAFE_RETRY_REQUESTED_TOKEN = "[editor] safe mode retry requested"
SAFE_LEFT_TOKEN = "[editor] safe mode left: the shell runs again"
PROBLEM_ROW_TOKEN = "[editor] problem"
MODULE_LOAD_FAILED_TOKEN = "optional module failed to load; the host continues without it"
UI_FRAMES_TOKEN = "[editor] ui frames"
CHILD_SCENE_REFUSED_PATTERN = (
    re.escape("[editor] child scene window forced off")
    + "|"
    + re.escape("[editor] child scene window refused")
)
LIVE_OR_RELOAD_PATTERN = r"LIVE \d+ ms|extension reload processed"

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


def wait_for_token(session: EditorSession, pattern: str, timeout: float, label: str) -> str:
    """Waits until `pattern` matches the session's log; returns the match.

    Same contract the suite grew up with - the callers compare the matched
    text - now over one `EditorSession` rather than a log path and a process.
    """
    session.wait(pattern, timeout, label)
    return session.find(pattern).group(0)


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
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
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
    binary = arguments.binary.resolve() if arguments.binary else None
    # One run folder for the whole suite, and one sandbox shared by every
    # session: the classic-layout migration in the first run is what the
    # later runs must see.
    run_folder = run_directory("native")
    run_folder.mkdir(parents=True, exist_ok=True)
    appdata = run_folder / "appdata"
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
    shell_source = (
        MODULES_ROOT
        / "extensions"
        / "pill_editor_shell"
        / "pill_editor_shell_editor"
        / "src"
        / "pill_editor_shell_editor.rs"
    )
    original_source = shell_source.read_text(encoding="utf-8")
    # The phases wait for two shell heartbeats and a patch compile; the
    # executable's seconds deadline is a hang guard, kept comfortably above
    # the phases even on a slow machine.
    main_session = EditorSession(
        "native",
        "main",
        project=arguments.project,
        test_seconds=120,
        sandbox=appdata,
        binary=binary,
    )
    main_session.start()
    try:
        # Step 1: startup, all the way to panels being painted.
        wait_for_token(
            main_session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "startup"
        )
        wait_for_token(
            main_session,
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+detached=false",
            120,
            "scene surface",
        )
        wait_for_token(
            main_session, re.escape(EDITOR_SHELL_LOADED_TOKEN), 180, "shell load"
        )
        heartbeat = wait_for_token(
            main_session,
            re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)\s+panels=(\d+)",
            180,
            "shell heartbeat",
        )
        match = re.search(r"frames=(\d+)\s+panels=(\d+)", heartbeat)
        assert match and int(match.group(2)) > 0, f"no panels painted: {heartbeat}"
        print(f"  [OK] shell loaded, {heartbeat}")

        # Step 1b: the classic layout was migrated once - the module panel
        # dropped by name - into the native layout file.
        wait_for_token(
            main_session, re.escape(LAYOUT_MIGRATION_TOKEN), 60, "migration drop"
        )
        wait_for_token(
            main_session, re.escape(CLASSIC_MIGRATED_TOKEN), 60, "migration"
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
        main_session.send("click 600 300")
        wait_for_token(
            main_session,
            re.escape(TEST_CLICK_TOKEN)
            + r'\s+x=600(?:\.0)?\s+y=300(?:\.0)?\s+target="?scene"?',
            30,
            "scene click",
        )
        main_session.send("click 40 780")
        wait_for_token(
            main_session,
            re.escape(TEST_CLICK_TOKEN)
            + r'\s+x=40(?:\.0)?\s+y=780(?:\.0)?\s+target="?ui"?',
            30,
            "ui click",
        )
        print("  [OK] input routing: scene and UI targets")

        # Step 3: a live patch of the shell's system, state kept.
        before = [
            int(entry)
            for entry in re.findall(
                re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)", main_session.read_log()
            )
        ]
        highest = max(before)
        cadence = re.search(r"% (\d+) == 0", original_source)
        assert cadence, "the shell system's heartbeat cadence was not found"
        patched = original_source.replace(
            cadence.group(0), f"% {int(cadence.group(1)) + 1} == 0", 1
        )
        main_session.edit_source(shell_source, patched)
        wait_for_token(
            main_session, LIVE_OR_RELOAD_PATTERN, 180, "shell patch"
        )
        seen_after = {}
        wait_for_token(
            main_session,
            re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)",
            120,
            "post-patch heartbeat",
        )
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            for entry in re.findall(
                re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)", main_session.read_log()
            ):
                seen_after[int(entry)] = True
            if any(frame > highest for frame in seen_after):
                break
            # Poll until a frame past the reload shows up.
            time.sleep(0.5)
        else:
            raise TimeoutError(
                "the shell's frame counter did not continue past "
                f"{highest} after the patch; its state was reset\n"
                + main_session.read_log()[-4000:]
            )
        print(f"  [OK] shell live-patched, frame counter continued past {highest}")

        # Step 4: a graceful quit, with the clean exit code.
        status = main_session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in main_session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        main_session.close()

    # Step 4b: a fresh default workspace - both layout files gone - which is
    # the layout that carries the Game tab beside the Scene tab (D29). The
    # Game view draws the project into its own surface; the scene view keeps
    # the editor camera; input reaches whichever view is selected, and the
    # UI's keys reach neither.
    for stale in (state_directory / "editor_layout.json", native_layout):
        if stale.exists():
            stale.unlink()
    game_session = EditorSession(
        "native",
        "game",
        project=arguments.project,
        test_seconds=300,
        sandbox=appdata,
        binary=binary,
    )
    game_session.start()
    try:
        wait_for_token(
            game_session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "game run"
        )
        wait_for_token(
            game_session, re.escape(EDITOR_SHELL_LOADED_TOKEN), 180, "game run shell"
        )
        # The default workspace starts on the Scene tab, so the game view
        # exists but is hidden.
        wait_for_token(
            game_session,
            re.escape(VIEW_HIDDEN_TOKEN) + r'\s+view="game"',
            120,
            "game view hidden",
        )
        print("  [OK] default workspace: Game tab beside Scene, game view hidden")

        # Switching to the Game tab selects it and shows the game surface in
        # the content area; the click below is placed inside that rectangle.
        game_session.send("tab game")
        wait_for_token(
            game_session,
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title=?game"?',
            60,
            "game tab",
        )
        shown = wait_for_token(
            game_session,
            re.escape(VIEW_SHOWN_TOKEN)
            + r'\s+view="game"\s+x=([\d.]+)\s+y=([\d.]+)'
            r"\s+width=([\d.]+)\s+height=([\d.]+)",
            60,
            "game view shown",
        )
        numbers = [float(entry) for entry in re.findall(r"=([\d.]+)", shown)]
        center_x = numbers[0] + numbers[2] / 2.0
        center_y = numbers[1] + numbers[3] / 2.0
        print(f"  [OK] Game tab selected, game surface shown ({shown[-60:]})")

        # A click inside the game view reaches the game; a key with the game
        # target reaches it too.
        game_session.send(f"click {center_x:.0f} {center_y:.0f}")
        wait_for_token(
            game_session,
            re.escape(TEST_CLICK_TOKEN)
            + rf'\s+x={center_x:.0f}(?:\.0)?\s+'
            rf'y={center_y:.0f}(?:\.0)?\s+target="?game"?',
            30,
            "game click",
        )
        game_session.send("key w game")
        wait_for_token(
            game_session,
            re.escape(TEST_KEY_TOKEN) + r'\s+key=w\s+target="?game"?',
            30,
            "game key",
        )

        # Switching back shows the scene surface; a click there reaches the
        # scene view (never the game) and a key moves the editor camera.
        game_session.send("tab scene")
        wait_for_token(
            game_session,
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title=?scene"?',
            60,
            "scene tab",
        )
        wait_for_token(
            game_session,
            re.escape(VIEW_SHOWN_TOKEN) + r'\s+view="scene"',
            60,
            "scene view shown",
        )
        game_session.send(f"click {center_x:.0f} {center_y:.0f}")
        wait_for_token(
            game_session,
            re.escape(TEST_CLICK_TOKEN)
            + rf'\s+x={center_x:.0f}(?:\.0)?\s+'
            rf'y={center_y:.0f}(?:\.0)?\s+target="?scene"?',
            30,
            "scene click",
        )
        game_session.send("key w scene")
        wait_for_token(
            game_session,
            re.escape(TEST_KEY_TOKEN) + r'\s+key=w\s+target="?scene"?',
            30,
            "scene key",
        )
        wait_for_token(
            game_session,
            re.escape(EDITOR_CAMERA_TOKEN) + r'\s+reason=',
            30,
            "editor camera",
        )
        print("  [OK] input routed by the selected view; scene keys move the editor camera")

        # A key without a view target is the UI's, and reaches neither view.
        game_session.send("key escape")
        wait_for_token(
            game_session,
            re.escape(TEST_KEY_TOKEN) + r'\s+key=escape\s+target="?ui"?',
            30,
            "ui key",
        )
        print("  [OK] untargeted keys go to the UI only")

        status = game_session.quit_and_wait(timeout=60)
        assert status == 0, f"game view run exited {status}"
        print("  [OK] game view run exited 0")
    finally:
        game_session.close()

    # Step 4c: the overlay layer and the platform popups (D23/D24).
    # The FPS badge and the docking drop preview float over the scene's own
    # child window on a DirectComposition surface, and the menus, the
    # command palette and the context menu open as windows of their own,
    # placed and flipped against the monitor's work area. The overlay
    # publishes the rectangles it covers, so a click on the badge belongs to
    # the UI and one beside it to the scene underneath.
    overlay_session = EditorSession(
        "native",
        "overlay",
        project=arguments.project,
        test_seconds=180,
        sandbox=appdata,
        binary=binary,
    )
    overlay_session.start()
    try:
        wait_for_token(
            overlay_session,
            re.escape(OVERLAY_CREATED_TOKEN) + r"\s+width=(\d+)\s+height=(\d+)",
            180,
            "overlay created",
        )
        # The monitor's work area, which the popup placement flips and slides
        # inside; the platform is the only thing that knows it.
        work = wait_for_token(
            overlay_session,
            re.escape(WORK_AREA_TOKEN)
            + r"\s+x=(-?\d+)\s+y=(-?\d+)\s+"
            r"width=(\d+)\s+height=(\d+)\s+scale=([\d.]+)",
            60,
            "work area",
        )
        work_x, work_y, work_width, work_height, scale = [
            float(entry) for entry in re.findall(r"=(-?[\d.]+)", work)
        ]
        window = wait_for_token(
            overlay_session,
            re.escape(EDITOR_WINDOW_CREATED_TOKEN) + r"\s+width=(\d+)\s+height=(\d+)",
            60,
            "window size",
        )
        window_width = float(re.search(r"width=(\d+)", window).group(1))
        # The overlay carries the UI the panels cannot: the badge and the
        # drop preview, drawn through the composition path, with the opaque
        # rectangles the input routing uses.
        content = wait_for_token(
            overlay_session,
            re.escape(OVERLAY_CONTENT_TOKEN)
            + r'\s+path="composition"\s+forced=false\s+'
            r"frames=\d+\s+vertices=([1-9]\d*)\s+opaque=([1-9]\d*)",
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
            overlay_session,
            re.escape(VIEW_SHOWN_TOKEN)
            + r'\s+view="scene"\s+x=([\d.]+)\s+y=([\d.]+)\s+'
            r"width=([\d.]+)\s+height=([\d.]+)",
            120,
            "scene view",
        )
        view_x, view_y, view_width, view_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", view)
        ]
        badge_x = view_x + view_width - 76.0 - 8.0
        badge_y = view_y + 8.0
        overlay_session.send(f"click {badge_x + 38:.0f} {badge_y + 10:.0f}")
        wait_for_token(
            overlay_session,
            re.escape(TEST_CLICK_TOKEN)
            + rf'\s+x={badge_x + 38:.0f}(?:\.0)?\s+'
            rf'y={badge_y + 10:.0f}(?:\.0)?\s+target="?ui"?\s+overlay=true',
            30,
            "badge click",
        )
        overlay_session.send(
            f"click {view_x + 40:.0f} {view_y + view_height - 40:.0f}"
        )
        wait_for_token(
            overlay_session,
            re.escape(TEST_CLICK_TOKEN)
            + rf'\s+x={view_x + 40:.0f}(?:\.0)?\s+'
            rf'y={view_y + view_height - 40:.0f}(?:\.0)?\s+'
            r'target="?scene"?\s+overlay=false',
            30,
            "scene click beside the badge",
        )
        wait_for_token(
            overlay_session, re.escape(SCENE_VIEW_CLICK_TOKEN), 30, "scene click routed"
        )
        print("  [OK] badge click reached the UI, the scene beside it the view")

        # A menu title opens its dropdown as a popup window of its own; a
        # click outside dismisses it the way the platform does.
        overlay_session.send("click 40 13")
        wait_for_token(
            overlay_session,
            re.escape(MENU_OPENED_TOKEN) + r"\s+path=",
            30,
            "menu opened",
        )
        wait_for_token(
            overlay_session,
            r"\[editor\] popup opened id=menu size=\((\d+), (\d+)\)\s+"
            r"requested=\((-?\d+), (-?\d+)\)\s+placed=\((-?\d+), (-?\d+)\)\s+"
            r"flipped=false slid=false",
            30,
            "menu popup",
        )
        overlay_session.send(f"click {view_x + 40:.0f} {view_y + 40:.0f}")
        wait_for_token(
            overlay_session,
            re.escape(POPUP_DISMISSED_TOKEN) + r'\s+popup="menu"',
            30,
            "menu dismissed",
        )
        print("  [OK] menu dropdown is a popup window, dismissed by an outside click")

        # The command palette is the second popup, with its own text field.
        overlay_session.send("command editor.command_palette")
        wait_for_token(
            overlay_session, re.escape(PALETTE_OPENED_TOKEN), 30, "palette opened"
        )
        wait_for_token(
            overlay_session,
            r"\[editor\] popup opened id=palette\s+size=",
            30,
            "palette popup",
        )
        overlay_session.send("popup-dismiss palette")
        wait_for_token(
            overlay_session,
            re.escape(TEST_POPUP_DISMISS_TOKEN) + r"\s+id=palette",
            30,
            "palette dismiss",
        )
        wait_for_token(
            overlay_session,
            re.escape(POPUP_DISMISSED_TOKEN) + r'\s+popup="palette"',
            30,
            "palette dismissed",
        )
        print("  [OK] palette popup opened and dismissed through the platform channel")

        # A right-click asks for a context menu: a window of its own, placed
        # at the click, and its rows activated through the popup's pointer.
        overlay_session.send("rightclick 400 300")
        wait_for_token(
            overlay_session,
            re.escape(CONTEXT_MENU_TOKEN) + r"\s+x=400(?:\.0)?",
            30,
            "context menu asked",
        )
        wait_for_token(
            overlay_session,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            r"requested=\(400(?:\.0)?, 300(?:\.0)?\)\s+placed=\((-?\d+), (-?\d+)\)\s+"
            r"flipped=false slid=false",
            30,
            "context popup",
        )
        overlay_session.send("popup-pointer context 110 16 release")
        wait_for_token(
            overlay_session,
            re.escape(TEST_POPUP_POINTER_TOKEN) + r"\s+id=context",
            30,
            "popup pointer",
        )
        # The first row is Undo: with an empty history that is its own log.
        wait_for_token(
            overlay_session, re.escape(EDITOR_HISTORY_UNDO_TOKEN), 30, "popup row command"
        )
        overlay_session.wait_occurrence(
            re.escape(POPUP_DISMISSED_TOKEN) + r'\s+popup="context"',
            1,
            30,
            "context dismissed",
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
        overlay_session.send(
            f"window-move {round(window_right - window_width)} {round(work_y) + 40}"
        )
        overlay_session.wait_occurrence(
            re.escape(TEST_WINDOW_MOVE_TOKEN), 1, 30, "window moved"
        )
        overlay_session.send(f"rightclick {click_x} 300")
        opened = wait_for_token(
            overlay_session,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            rf"requested=\({click_x}(?:\.0)?, 300(?:\.0)?\)\s+"
            r"placed=\((-?\d+), (-?\d+)\)\s+flipped=false slid=false",
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
        overlay_session.send(f"click {view_x + 40:.0f} {view_y + 40:.0f}")
        overlay_session.wait_occurrence(
            re.escape(POPUP_DISMISSED_TOKEN) + r'\s+popup="context"',
            2,
            30,
            "context dismissed before the flip",
        )
        overlay_session.send(
            f"window-move {round(work_x + work_width - window_width)} {round(work_y) + 40}"
        )
        overlay_session.wait_occurrence(
            re.escape(TEST_WINDOW_MOVE_TOKEN), 2, 30, "window moved to the edge"
        )
        overlay_session.send(f"rightclick {click_x} 300")
        flipped = wait_for_token(
            overlay_session,
            r"\[editor\] popup opened id=context size=\((\d+), (\d+)\)\s+"
            rf"requested=\({click_x}(?:\.0)?, 300(?:\.0)?\)\s+"
            r"placed=\((-?\d+), (-?\d+)\)\s+flipped=true slid=false",
            30,
            "context popup flipped",
        )
        flipped_x = int(re.findall(r"placed=\((-?\d+),", flipped)[0])
        assert flipped_x >= work_x, f"the flipped menu left the work area: {flipped}"
        assert flipped_x + size_x <= work_x + work_width, (
            f"the flipped menu left the work area: {flipped}"
        )
        print(f"  [OK] menu flipped at the monitor's edge, placed at x={flipped_x}")

        status = overlay_session.quit_and_wait(timeout=60)
        assert status == 0, f"the overlay run exited {status}"
        assert "panic" not in overlay_session.read_log(), "a panic was logged"
        print("  [OK] overlay and popup run exited 0")
    finally:
        overlay_session.close()

    # Step 4d: with the composition overlay forced to fail, the same content
    # must reach the scene through the renderer's UI pass (the overlay
    # fallback), with one warning and no loss.
    fallback_session = EditorSession(
        "native",
        "fallback",
        project=arguments.project,
        test_seconds=20,
        sandbox=appdata,
        binary=binary,
    )
    fallback_session.start(
        scripted=False,
        extra_env={"PILL_EDITOR_NO_OVERLAY": "1", "PILL_EDITOR_TEST_SECONDS": "20"},
    )
    try:
        wait_for_token(
            fallback_session,
            re.escape(OVERLAY_DISABLED_TOKEN)
            + r";\s+the UI falls back to the scene pass",
            180,
            "overlay forced failure",
        )
        fallback = wait_for_token(
            fallback_session,
            re.escape(OVERLAY_CONTENT_TOKEN)
            + r'\s+path="scene-pass"\s+forced=true\s+'
            r"frames=0\s+vertices=([1-9]\d*)\s+opaque=([1-9]\d*)",
            120,
            "fallback content",
        )
        vertices = int(re.search(r"vertices=(\d+)", fallback).group(1))
        opaque = int(re.search(r"opaque=(\d+)", fallback).group(1))
        assert vertices > 0, f"the fallback UI pass got nothing to draw: {fallback}"
        assert opaque > 0, f"the fallback published no opaque region: {fallback}"
        assert OVERLAY_CREATED_TOKEN not in fallback_session.read_log(), (
            "the composition overlay was created despite the forced failure"
        )
        status = fallback_session.process.wait(timeout=120)
        assert status == 0, f"the fallback run exited {status}"
        print(
            f"  [OK] forced overlay failure: the same {vertices} vertices went "
            "through the scene pass"
        )
    finally:
        fallback_session.close()

    # Step 5: the clean exit code again, on further runs, quitting by the
    # scripted seconds deadline instead of the command file.
    for run in range(1, arguments.exit_runs + 1):
        exit_session = EditorSession(
            "native",
            f"exit-{run}",
            project=arguments.project,
            test_seconds=3,
            sandbox=appdata,
            binary=binary,
        )
        exit_session.start(scripted=False, extra_env={"PILL_EDITOR_TEST_SECONDS": "3"})
        try:
            wait_for_token(
                exit_session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "exit run"
            )
            status = exit_session.process.wait(timeout=120)
            assert status == 0, (
                f"exit run {run}: editor exited {status}\n"
                + exit_session.read_log()[-4000:]
            )
        finally:
            exit_session.close()
    print(f"  [OK] {arguments.exit_runs} further runs exited 0")

    # Step 6: with a child scene window forced to fail, the scene must open
    # detached, still render and exit cleanly.
    detached_session = EditorSession(
        "native",
        "detached",
        project=arguments.project,
        test_seconds=10,
        sandbox=appdata,
        binary=binary,
    )
    detached_session.start(
        scripted=False,
        extra_env={
            "PILL_EDITOR_TEST_SECONDS": "10",
            "PILL_EDITOR_DETACHED_SCENE": "1",
        },
    )
    try:
        wait_for_token(
            detached_session,
            CHILD_SCENE_REFUSED_PATTERN,
            120,
            "detached fallback",
        )
        wait_for_token(
            detached_session,
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+detached=true",
            120,
            "detached attach",
        )
        wait_for_token(
            detached_session, re.escape(SCENE_FRAME_REPORT_TOKEN), 120, "detached frames"
        )
        status = detached_session.process.wait(timeout=120)
        assert status == 0, f"detached fallback exited {status}"
        print("  [OK] forced detached fallback: attached, rendered, exited 0")
    finally:
        detached_session.close()

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
    safe_session = EditorSession(
        "native",
        "safe",
        project=arguments.project,
        test_seconds=300,
        sandbox=appdata,
        binary=binary,
    )
    # Broken before the run starts; the session captured the original, so
    # closing it restores the source whatever path the phase exits by.
    safe_session.edit_source(shell_source, broken_source)
    safe_session.start()
    try:
        wait_for_token(
            safe_session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 180, "safe run"
        )
        wait_for_token(
            safe_session,
            re.escape(MODULE_LOAD_FAILED_TOKEN),
            240,
            "tolerated shell failure",
        )
        wait_for_token(
            safe_session,
            re.escape(PROBLEM_ROW_TOKEN)
            + r'\s+module="?pill_editor_shell_editor"?\s+'
            r'level="?error"?\s+file="[^"]*pill_editor_shell_editor\.rs"\s+line=\d+',
            120,
            "shell build problem rows",
        )
        wait_for_token(
            safe_session,
            re.escape(SAFE_ENTERED_TOKEN) + r'\s+reason="?shell-load-failed"?',
            120,
            "safe mode entered",
        )
        retry = wait_for_token(
            safe_session,
            re.escape(SAFE_RETRY_TOKEN)
            + r"\s+x=([\d.]+)\s+y=([\d.]+)\s+width=([\d.]+)\s+height=([\d.]+)",
            60,
            "retry button",
        )
        retry_x, retry_y, retry_width, retry_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", retry)
        ]
        # The safe layout keeps the Scene view: a click in the right column
        # routes to the scene, not to the safe-mode UI.
        safe_session.send("click 900 300")
        wait_for_token(
            safe_session,
            re.escape(TEST_CLICK_TOKEN)
            + r'\s+x=900(?:\.0)?\s+y=300(?:\.0)?\s+target="?scene"?',
            30,
            "safe mode scene view",
        )
        print(
            f"  [OK] safe mode after a failed load; retry at "
            f"({retry_x:.0f}, {retry_y:.0f})"
        )
        # Fix the source and click Retry; the rebuild and load run inside
        # the click's own frame, so this wait covers the build too. The plain
        # write restores the same content the session captured.
        shell_source.write_text(original_source, encoding="utf-8", newline="")
        safe_session.send(
            f"click {retry_x + retry_width / 2:.0f} {retry_y + retry_height / 2:.0f}"
        )
        wait_for_token(
            safe_session, re.escape(SAFE_RETRY_REQUESTED_TOKEN), 300, "retry request"
        )
        wait_for_token(
            safe_session,
            re.escape(EDITOR_SHELL_LOADED_TOKEN),
            120,
            "shell load after retry",
        )
        wait_for_token(
            safe_session, re.escape(SAFE_LEFT_TOKEN), 120, "safe mode left"
        )
        wait_for_token(
            safe_session,
            re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)\s+panels=([1-9]\d*)",
            120,
            "panels after retry",
        )
        print("  [OK] retry loaded the shell and left safe mode, layout painted")
        status = safe_session.quit_and_wait(timeout=60)
        assert status == 0, f"the safe-mode run exited {status}"
        print("  [OK] safe-mode run exited 0")
    finally:
        safe_session.close()

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
    stopped_session = EditorSession(
        "native",
        "stopped",
        project=arguments.project,
        test_seconds=300,
        sandbox=appdata,
        binary=binary,
    )
    # Panicking before the run starts; the session restored the captured
    # original when it closes.
    stopped_session.edit_source(shell_source, panicking_source)
    stopped_session.start()
    try:
        wait_for_token(
            stopped_session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 180, "stopped run"
        )
        wait_for_token(
            stopped_session,
            re.escape(EDITOR_SHELL_LOADED_TOKEN),
            240,
            "stopped run shell load",
        )
        wait_for_token(
            stopped_session,
            re.escape(SAFE_ENTERED_TOKEN) + r'\s+reason="?shell-stopped"?',
            180,
            "safe mode entered (stopped)",
        )
        retry = wait_for_token(
            stopped_session,
            re.escape(SAFE_RETRY_TOKEN)
            + r"\s+x=([\d.]+)\s+y=([\d.]+)\s+width=([\d.]+)\s+height=([\d.]+)",
            60,
            "retry button (stopped)",
        )
        retry_x, retry_y, retry_width, retry_height = [
            float(entry) for entry in re.findall(r"=([\d.]+)", retry)
        ]
        live_seen = len(re.findall(LIVE_OR_RELOAD_PATTERN, stopped_session.read_log()))
        stopped_session.send(
            f"click {retry_x + retry_width / 2:.0f} {retry_y + retry_height / 2:.0f}"
        )
        wait_for_token(
            stopped_session,
            re.escape(SAFE_RETRY_REQUESTED_TOKEN),
            60,
            "retry request (stopped)",
        )
        # The retry rebuilds and reloads the same panicking code; the shell
        # must stay in safe mode.
        stopped_session.wait_occurrence(
            LIVE_OR_RELOAD_PATTERN, live_seen + 1, 240, "retry reload"
        )
        # Give a stray safe-mode exit two seconds to show itself before asserting.
        time.sleep(2.0)
        assert SAFE_LEFT_TOKEN not in stopped_session.read_log(), (
            "safe mode left while the shell's code still panics"
        )
        print("  [OK] safe mode with a panicking shell; retry kept waiting")
        # Fix the source: the watcher reloads it and the frames resume. The
        # plain write restores the same content the session captured.
        shell_source.write_text(original_source, encoding="utf-8", newline="")
        stopped_session.wait_occurrence(
            LIVE_OR_RELOAD_PATTERN, live_seen + 2, 240, "recovery reload"
        )
        wait_for_token(
            stopped_session, re.escape(SAFE_LEFT_TOKEN), 120, "safe mode left (stopped)"
        )
        wait_for_token(
            stopped_session,
            re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=(\d+)\s+panels=([1-9]\d*)",
            120,
            "panels after recovery",
        )
        print("  [OK] fixing the source reloaded the shell and left safe mode")
        status = stopped_session.quit_and_wait(timeout=60)
        assert status == 0, f"the panicking-shell run exited {status}"
        print("  [OK] panicking-shell run exited 0")
    finally:
        stopped_session.close()

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
    idle_session = EditorSession(
        "native",
        "idle",
        project=arguments.project,
        test_seconds=180,
        sandbox=appdata,
        binary=binary,
    )
    idle_session.start()
    try:
        wait_for_token(
            idle_session, re.escape(EDITOR_SHELL_LOADED_TOKEN), 180, "idle run"
        )
        wait_for_token(
            idle_session,
            re.escape(SHELL_FRAME_TOKEN) + r"\s+frames=\d+\s+panels=([1-9]\d*)",
            120,
            "idle layout",
        )
        # Pause the project's systems; the shell's own system keeps running
        # (that is what safe mode's liveness check relies on).
        idle_session.send("command editor.pause_toggle")
        wait_for_token(
            idle_session, re.escape(TEST_COMMAND_REQUEST_TOKEN), 30, "pause request"
        )
        # Let the pause take effect before counting UI frames.
        time.sleep(2.0)

        def ui_frames_so_far() -> int:
            """The presented-frame count, by asking the editor to dump it."""
            pattern = re.escape(UI_FRAMES_TOKEN) + r"\s+presented=(\d+)"
            seen = len(re.findall(pattern, idle_session.read_log()))
            idle_session.send("dump ui-frames")
            idle_session.wait_occurrence(pattern, seen + 1, 60, "ui frames dump")
            return int(re.findall(pattern, idle_session.read_log())[-1])

        first = ui_frames_so_far()
        # A five-second window for the paused frames to prove they do not advance.
        time.sleep(5.0)
        second = ui_frames_so_far()
        assert second - first <= 2, (
            f"the idle editor kept presenting UI frames: {first} -> {second}"
        )
        print(
            f"  [OK] idle: {second - first} UI frames in five seconds"
        )
        idle_session.send("move 40 13")
        wait_for_token(
            idle_session,
            re.escape(TEST_MOVE_TOKEN) + r"\s+x=40(?:\.0)?\s+y=13(?:\.0)?",
            30,
            "move command",
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if ui_frames_so_far() > second:
                break
        else:
            raise TimeoutError(
                "hovering the UI did not redraw it\n" + idle_session.read_log()[-4000:]
            )
        print("  [OK] a pointer move onto the UI redrew it")
        status = idle_session.quit_and_wait(timeout=60)
        assert status == 0, f"the idle run exited {status}"
        print("  [OK] idle run exited 0")
    finally:
        idle_session.close()

    print("PASS: editor native end-to-end")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
