"""One driver for the editor end-to-end suites.

RESPONSIBILITIES
  - EditorSession: one editor run for a suite - the machine-global host lock,
    the stray-editor cleanup under it, a per-run folder (log, command file,
    LOCALAPPDATA sandbox), the launch environment and the byte-anchored log
    waits the editor suite is built on.
  - The shared editor log tokens (*_TOKEN) the suite waits on, as plain
    substrings, so `test_log_contract.py` checks each against the Rust
    sources that print it.

DESIGN
  The suite anchors waits on byte offsets into a growing log file: an
  assertion captures `start = len(read_log())` before it sends a command and
  re-scans only what arrives after. `OutputMonitor` in `suite_common` is a
  pipe-based line monitor with token searches; it cannot serve an offset
  anchor over the whole stream without changing every assertion, so
  EditorSession owns a log FILE (the child's stdout lands in it) and
  re-reads it tolerantly, exactly as the suites did. The lock, the
  stray-process cleanup and the sandbox conventions come from
  `suite_common`; per-run folders keep every run's log instead of
  overwriting a fixed one.

USAGE
  from core.editor_session import EditorSession

  session = EditorSession("commands")
  session.start()
  try:
      session.wait(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
      session.send("command editor.pause_toggle")
      session.wait(EDITOR_SCRIPTED_REJECTED_TOKEN, 60, "no rejection")
  finally:
      session.close()
"""

import json
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from core.environment import filesystem_timestamp
from core.paths import MODULES_ROOT, REPOSITORY_ROOT, executable_name
from core.suite_common import ensure_host_lock, kill_stale_hosts, release_host_lock

# The editor binary every native-editor suite drives.
EDITOR_BINARY = executable_name("pill_editor_egui")

# Every suite's run artifacts live here, one folder per suite and run.
EDITOR_VALIDATION_ROOT = REPOSITORY_ROOT / "local" / "editor-validation"

# =============================================================================
# Shared editor log tokens
# =============================================================================

# Plain substrings of the lines the editor prints; each one must stay
# printable by a Rust string, which `test_log_contract.py` checks. Suite
# specific patterns (entity handles, labels, counts) stay in the suites.

EDITOR_WINDOW_CREATED_TOKEN = "[editor] window created"
EDITOR_ENGINE_COMMANDS_TOKEN = "[editor] engine commands applied"
EDITOR_HISTORY_PUSH_TOKEN = "[editor] history push"
EDITOR_HISTORY_UNDO_TOKEN = "[editor] history undo"
EDITOR_HISTORY_REDO_TOKEN = "[editor] history redo"
# The egui editor's one refusal line for a scripted line it cannot run: an
# unknown verb, an unknown target, or a missing or malformed argument. A suite
# typo raises `ScriptedCommandRejected` from the next wait instead of running
# out its timeout, and `test_log_contract.py` pins the literal against Rust.
EDITOR_SCRIPTED_REJECTED_TOKEN = "[editor] scripted command rejected"
REJECTION_PATTERN = re.compile(
    re.escape(EDITOR_SCRIPTED_REJECTED_TOKEN)
    + r'\s+command="((?:[^"\\]|\\.)*)"\s+reason="((?:[^"\\]|\\.)*)"'
)

class ScriptedCommandRejected(RuntimeError):
    """A scripted line the editor refused, raised before any timeout can hide it."""

    def __init__(self, command: str, reason: str) -> None:
        super().__init__(
            "the editor rejected the scripted line {!r}: {}".format(command, reason)
        )
        self.command = command
        self.reason = reason

# =============================================================================
# Run directories
# =============================================================================

# One run folder per suite per process: a suite that launches several editor
# sessions (the undo suite's asset phases) shares its folder, one file set
# per session. Cached so every session of one process agrees on the path.
_RUN_DIRECTORIES: Dict[str, Path] = {}

def run_directory(suite: str) -> Path:
    """The run folder of `suite` for this process; computed once, then cached."""
    if suite not in _RUN_DIRECTORIES:
        _RUN_DIRECTORIES[suite] = EDITOR_VALIDATION_ROOT / "{}-{}".format(
            suite, filesystem_timestamp()
        )
    return _RUN_DIRECTORIES[suite]

# =============================================================================
# EditorSession
# =============================================================================

class EditorSession:
    """One editor run for a suite, with the paths and waits suites share.

    `start` takes the machine-global host lock before it kills stray
    processes, so two suites started together queue instead of killing each
    other's editor. `close` restores every source the suite edited, kills the
    editor if it is still alive, and releases the lock.
    """

    def __init__(
        self,
        suite: str,
        session: str = "main",
        project: str = "../examples/project_rs",
        test_seconds: int = 300,
        sandbox: Optional[Path] = None,
        binary: Optional[Path] = None,
    ) -> None:
        """Names the run: `suite` owns the folder, `session` the file set.

        `sandbox` overrides the session-local LOCALAPPDATA: a suite whose runs
        share one editor state (the native suite's layout migration) passes
        one path and every session of the suite reads and writes it. `binary`
        overrides the editor executable, for the native suite's `--binary`
        flag.
        """
        self.suite = suite
        self.session = session
        self.project = project
        self.test_seconds = test_seconds
        self._sandbox_override = Path(sandbox) if sandbox is not None else None
        self._binary = Path(binary) if binary is not None else EDITOR_BINARY
        self._process: Optional[subprocess.Popen] = None
        self._sources: Dict[Path, str] = {}
        # Refusals already reported to a suite, so a wait that reads the whole
        # log (the default anchor) only raises on a line new since the last
        # one: a probe that expects a refusal must not poison every later wait.
        self._rejections_seen = 0

    @property
    def binary(self) -> Path:
        """The editor executable this session launches."""
        return self._binary

    @classmethod
    def supported(cls) -> bool:
        """Whether this platform runs the native editor (Windows only)."""
        return os.name == "nt"

    @property
    def run_directory(self) -> Path:
        """This suite's run folder for the process."""
        return run_directory(self.suite)

    @property
    def log_path(self) -> Path:
        """The session's log file (the editor's merged stdout/stderr)."""
        return self.run_directory / "{}.log".format(self.session)

    @property
    def commands_path(self) -> Path:
        """The scripted command file the editor reads lines from."""
        return self.run_directory / "{}-commands.txt".format(self.session)

    @property
    def sandbox(self) -> Path:
        """The session's LOCALAPPDATA sandbox (layout, preferences, keymap)."""
        if self._sandbox_override is not None:
            return self._sandbox_override
        return self.run_directory / "{}-appdata".format(self.session)

    @property
    def process(self) -> Optional[subprocess.Popen]:
        """The editor process, for the few waits a suite runs itself."""
        return self._process

    # -------------------------------------------------------------------------
    # Lifecycle
    # -------------------------------------------------------------------------

    def start(
        self,
        project: Optional[str] = None,
        test_seconds: Optional[int] = None,
        extra_env: Optional[Dict[str, str]] = None,
        scripted: bool = True,
    ) -> None:
        """Locks the host, clears strays, and launches the editor.

        `scripted=False` launches without the test channel: no command file is
        written and its environment variables are left unset, for the runs that
        only prove startup and exit.

        Raises `SystemExit` with the build hint when the binary is missing -
        the suites surface that as their usage error.
        """
        if self._process is not None:
            raise RuntimeError("this session already started an editor")
        if not self._binary.is_file():
            raise SystemExit(
                "editor binary not found at {}; build it with "
                "`cargo build -p pill_editor_egui` in modules/ first".format(self._binary)
            )
        # The lock first, then the kills: a concurrent suite's editor is only
        # ever killed by the holder, never by a queued waiter.
        ensure_host_lock()
        kill_stale_hosts()
        self.run_directory.mkdir(parents=True, exist_ok=True)
        self.sandbox.mkdir(parents=True, exist_ok=True)
        environment = dict(os.environ)
        environment["PROJECT_PATH"] = project or self.project
        environment["LOCALAPPDATA"] = str(self.sandbox)
        if scripted:
            self.commands_path.write_text(
                "# {} suite: {}\n".format(self.suite, self.session), encoding="utf-8"
            )
            environment["PILL_EDITOR_TEST_COMMANDS"] = str(self.commands_path)
            environment["PILL_EDITOR_TEST_SECONDS"] = str(test_seconds or self.test_seconds)
        environment.update(extra_env or {})
        log = self.log_path.open("w", encoding="utf-8")
        self._process = subprocess.Popen(
            [str(self._binary)],
            cwd=str(MODULES_ROOT),
            env=environment,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        log.close()

    def close(self) -> None:
        """Restores edited sources, kills the editor, releases the lock."""
        self.restore_sources()
        process = self._process
        if process is not None and process.poll() is None:
            process.kill()
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                pass
        release_host_lock()

    def __enter__(self) -> "EditorSession":
        self.start()
        return self

    def __exit__(self, *exc_info) -> None:
        self.close()

    def quit_and_wait(self, timeout: float = 120.0) -> int:
        """Sends `quit` and waits for the editor's exit; returns its code."""
        self.send("quit")
        assert self._process is not None, "the session has not started"
        return self._process.wait(timeout=timeout)

    def wait_exit(self, timeout: float = 120.0) -> int:
        """Waits for the editor to leave on its own (a UI quit); returns its code."""
        assert self._process is not None, "the session has not started"
        return self._process.wait(timeout=timeout)

    def exit_code(self) -> Optional[int]:
        """The editor's exit code once it has left, `None` while it runs."""
        assert self._process is not None, "the session has not started"
        return self._process.poll()

    # -------------------------------------------------------------------------
    # The log: file-backed, byte-anchored waits
    # -------------------------------------------------------------------------

    def read_log(self) -> str:
        """The editor log so far, tolerant of a file the writer holds open."""
        try:
            return self.log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    def wait(self, pattern: str, timeout: float, label: str, start: int = 0) -> int:
        """Waits for `pattern` after byte offset `start`; returns the new length.

        Same contract the suites grew up with: prints `[OK] <label>` on a
        match, raises `RuntimeError` when a running editor exits first, and
        `TimeoutError` on timeout - both with the log tail for diagnosis. A
        refusal of a scripted line raises `ScriptedCommandRejected` the moment
        it lands, so a typo never waits out the timeout; a refusal an earlier
        wait already raised on is not raised again.
        """
        deadline = time.monotonic() + timeout
        expression = re.compile(pattern)
        process = self._process
        while time.monotonic() < deadline:
            text = self.read_log()
            window = text[start:]
            if expression.search(window):
                print("  [OK] {}".format(label))
                return len(text)
            if EDITOR_SCRIPTED_REJECTED_TOKEN in text:
                rejected = REJECTION_PATTERN.findall(text)
                if len(rejected) > self._rejections_seen:
                    self._rejections_seen = len(rejected)
                    command, reason = rejected[-1]
                    raise ScriptedCommandRejected(command, reason)
            if process is not None and process.poll() is not None:
                raise RuntimeError(
                    "{}: the editor exited {}\n".format(label, process.returncode)
                    + text[-4000:]
                )
            time.sleep(0.5)
        raise TimeoutError(
            "{}: timeout waiting for {}\n".format(label, pattern) + self.read_log()[-4000:]
        )

    def wait_token(self, token: str, timeout: float, label: str, start: int = 0) -> int:
        """Waits for a plain token (escaped before the regex search)."""
        return self.wait(re.escape(token), timeout, label, start)

    # -------------------------------------------------------------------------
    # Engine run state: the pause gate the suites share
    # -------------------------------------------------------------------------

    # A running engine reports tens of frames a second in its scene frame
    # report; a paused one about one. Anything below the limit reads as
    # paused.
    RUNNING_FPS_LIMIT = 8.0

    def await_engine_frame_rate(self, timeout: float = 20.0) -> float:
        """The engine's tick rate, from the next two scene frame reports.

        The scene frame report prints about once a second in both states and
        carries the engine's own tick rate. Suites gate their pauses on it:
        an input or a leftover queued command can toggle the engine before a
        step expects it, and a blind toggle would flip it the wrong way. The
        wait is for fresh reports, not the last one in the log (a report
        trails a state change by up to a second), and the SECOND report is
        the one returned: the first can straddle the boot's own slow frames
        and read like a paused engine. Returns the last rate seen, 0.0 when
        no report lands within `timeout`.
        """
        deadline = time.monotonic() + timeout
        seen = 0
        rate = 0.0
        while True:
            start = len(self.read_log())
            # Half a report interval: a fresh report is picked up as early
            # as it lands.
            time.sleep(0.5)
            for line in self.read_log()[start:].splitlines():
                match = re.search(
                    re.escape(EDITOR_FRAME_REPORT_TOKEN) + r"\s+fps=([0-9.]+)", line
                )
                if match:
                    seen += 1
                    rate = float(match.group(1))
            if seen >= 2 or time.monotonic() >= deadline:
                return rate

    def engine_is_running(self, timeout: float = 8.0) -> bool:
        """Whether the engine is ticking, by `await_engine_frame_rate`.

        No report yet counts as running: that is what the suites assumed
        before they had a state to read.
        """
        rate = self.await_engine_frame_rate(timeout)
        return rate == 0.0 or rate >= self.RUNNING_FPS_LIMIT

    def wait_occurrence(
        self, pattern: str, occurrence: int, timeout: float, label: str
    ) -> str:
        """Waits until `pattern` has appeared at least `occurrence` times.

        A token a run repeats - a window move, a popup dismissal - must be
        waited for by count: the earlier occurrence is already in the log and
        would satisfy a plain search before the new one has happened. Returns
        the matched text, which suites compare (a frame count, a placement).
        """
        deadline = time.monotonic() + timeout
        expression = re.compile(pattern)
        process = self._process
        while time.monotonic() < deadline:
            matches = [match.group(0) for match in expression.finditer(self.read_log())]
            if len(matches) >= occurrence:
                print("  [OK] {}".format(label))
                return matches[occurrence - 1]
            if process is not None and process.poll() is not None:
                raise RuntimeError(
                    "{}: the editor exited {} before occurrence {} of {}\n".format(
                        label, process.returncode, occurrence, pattern
                    )
                    + self.read_log()[-4000:]
                )
            time.sleep(0.5)
        raise TimeoutError(
            "{}: timeout waiting for occurrence {} of {}\n".format(label, occurrence, pattern)
            + self.read_log()[-4000:]
        )

    def wait_either(self, patterns: List[str], timeout: float, start: int = 0) -> Tuple[str, int]:
        """Waits for the first of `patterns`; returns the one seen and the length.

        Same contract as `wait`: `RuntimeError` when a running editor exits
        first, `TimeoutError` when none of the patterns appear in time.
        """
        deadline = time.monotonic() + timeout
        expressions = [(pattern, re.compile(pattern)) for pattern in patterns]
        process = self._process
        while time.monotonic() < deadline:
            text = self.read_log()
            for pattern, expression in expressions:
                if expression.search(text[start:]):
                    return pattern, len(text)
            if process is not None and process.poll() is not None:
                raise RuntimeError(
                    "the editor exited {} before any of {}\n".format(
                        process.returncode, patterns
                    )
                    + text[-4000:]
                )
            time.sleep(0.5)
        raise TimeoutError(
            "timeout waiting for any of {}\n".format(patterns)
            + self.read_log()[-4000:]
        )

    def find(self, pattern: str, start: int = 0) -> re.Match:
        """The first match of `pattern` in the log from `start`, now."""
        match = re.compile(pattern).search(self.read_log()[start:])
        if not match:
            raise RuntimeError(
                "no match for {}\n".format(pattern) + self.read_log()[-3000:]
            )
        return match

    def send(self, line: str) -> None:
        """Appends one scripted command, flushed, so the editor sees it whole."""
        with self.commands_path.open("a", encoding="utf-8") as file:
            file.write(line + "\n")

    def request(
        self,
        text: str,
        pattern: str,
        timeout: float = 30.0,
        label: Optional[str] = None,
    ) -> str:
        """Appends one scripted line and waits for its answer, anchored.

        The wait starts at the log's length before the line is written, so an
        answer the log already carried cannot satisfy it; a refusal of the
        line raises `ScriptedCommandRejected` at once. `pattern` is a regex
        (suites pass `re.escape(<token>)` for a literal). Returns the log text
        from the anchor through the answer, for the callers that parse it.
        """
        start = len(self.read_log())
        self.send(text)
        self.wait(pattern, timeout, label or "scripted {}".format(text), start)
        return self.read_log()[start:]

    def send_many(self, lines: List[str]) -> None:
        """Appends several scripted commands in one write.

        The channel polls once per frame, so a single write hands them over in
        the same frame - needed when their meaning depends on arriving together
        (a multi-file drop).
        """
        with self.commands_path.open("a", encoding="utf-8") as file:
            file.write("".join(line + "\n" for line in lines))

    # -------------------------------------------------------------------------
    # Sandbox files and edited sources
    # -------------------------------------------------------------------------

    def write_appdata_json(self, name: str, body: dict) -> Path:
        """Writes one of the editor's live-read files under the sandbox."""
        path = self.sandbox / "RustHybridEcs" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(body), encoding="utf-8")
        return path

    def edit_source(self, path: Path, text: str) -> None:
        """Writes `text` over a source file, LF preserved, restored on close.

        The original content is captured once and restored by `close`, so a
        suite never leaves an edited crate behind, whatever path it exits by.
        """
        path = Path(path)
        if path not in self._sources:
            with path.open("r", encoding="utf-8", newline="") as file:
                self._sources[path] = file.read()
        with path.open("w", encoding="utf-8", newline="") as file:
            file.write(text)

    def restore_sources(self) -> None:
        """Restores every source `edit_source` wrote, byte for byte."""
        for path, original in self._sources.items():
            with path.open("w", encoding="utf-8", newline="") as file:
                file.write(original)
        self._sources.clear()
