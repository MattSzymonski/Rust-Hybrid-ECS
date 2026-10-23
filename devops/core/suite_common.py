"""
Shared plumbing for the Rust-Hybrid-ECS hot-reload integration suites.

REQUIREMENTS
  - Python 3.8+

DESCRIPTION
    Single source of truth for the low-level machinery every host-driving
    script needs, under `devops/core/` because both the tests and the
    benchmarks import it:

      * `devops/tests/test_hot_reload_suite.py`     - full suite (sessions A/B)
      * `devops/tests/test_hot_reload_migration.py` - schema-migration suite
      * `devops/tests/test_csharp_bridge.py`        - C# bridge suite
      * `devops/benchmarks/hot_reload_harness.py`   - reload timing harness
      * `devops/benchmarks/cold_start.py`           - host startup timing

    Shared here: path resolution, the console-color `print` wrapper, the
    rolling output monitor with a dedicated counter-tick tail, atomic source
    editing, the edit that triggers a spline reload
    (`edit_spline_sample_offset`), the host postures with their one build and
    one launch (`HostPosture`, `build_host`, `host_command`), the common suite
    arguments, host process lifecycle helpers, and the log tokens the suites
    assert on.

    The scenario runner those suites run their tables on - edit, wait, assert,
    restore - is one level up in `core/scenario.py`.

    Keeping this in one module means a reworded host log line is fixed in
    exactly one place (audit opportunity 5.14), and the OutputMonitor's
    rolling-buffer / tick-tail behavior cannot drift between suites.

USAGE
  from core.suite_common import *   # paths, tokens, print, OutputMonitor, ...

  with `devops/` on sys.path; see `core.paths.ensure_devops_on_path`.

--- SCRIPT ---
"""

import argparse
import builtins
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Dict, List, Optional, Sequence, Tuple

from .paths import HOST_EXECUTABLE, MODULES_ROOT, REPOSITORY_ROOT

# The host's output is UTF-8 (its renderer prints a box-drawing frame
# dashboard), while CPython encodes redirected output with the locale code
# page. Echoing one of those lines then raises UnicodeEncodeError inside the
# monitor thread, which stops draining the pipe - the host blocks on its next
# write and every later assertion times out for a reason nothing explains.
# Replacement keeps the log readable instead of dying on a frame border.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(errors="replace")
    except (AttributeError, OSError, ValueError):
        pass

# =============================================================================
# Paths
# =============================================================================

# The roots come from `core.paths`; the suites know them by these names.
WORKSPACE_ROOT = REPOSITORY_ROOT
HOST_EXE = HOST_EXECUTABLE

# Known project roots. Each declares its own `project_settings.yaml`, the only
# source of the extension list; the project itself is selected by
# `PROJECT_PATH` alone (there is no host-level config file anymore).
NATIVE_PROJECT_ROOT = WORKSPACE_ROOT / "examples" / "project_rs"
CSHARP_PROJECT_ROOT = WORKSPACE_ROOT / "examples" / "project_cs"
FIXTURE_PROJECT_ROOT = WORKSPACE_ROOT / "devops" / "tests" / "project"

# The fixture project's entry source: the file the native and migration
# scenarios edit, and the one whose restore settles with a project reload
# rather than a module reload.
PROJECT_SANDBOX_LIB_RS = FIXTURE_PROJECT_ROOT / "src" / "lib.rs"


def project_settings_yaml(project_root: Path) -> Path:
    """Returns a project's settings file path."""
    return project_root / "project_settings.yaml"

# =============================================================================
# Output tokens shared by the suites
#
# The host's log lines are a test contract: the suites assert on literal
# substrings. Keep every token here so a reword is fixed in one place.
#
# HOW THE ASSERTION WORKS, because the failure message does not say so: the
# harness captures the host's merged stdout/stderr into one string, and each
# token is checked with `token not in output`. Nothing is compared against a
# source file. So a token fails for exactly one reason - the host did not print
# that text - and the fix is either to reword the token or to find out why the
# host went quiet.
#
# THE TRAP. Most tokens below are printed by `pill_host`, which nobody edits
# casually. Two are not: MODULE_REGISTERED_MESSAGE and PROJECT_PROBE_PREFIX
# come from the example project and the example module - the two files the
# whole hot-reload demo invites you to type into. Editing the probe string
# breaks these suites with a "Missing required token" that reads like a reload
# failure and is not: the reload worked, the text just moved.
#
# Prefer matching the part of a line that carries meaning - a count, a computed
# value - over the part that is only there to be edited. Where a token still
# pins incidental text, say why at the assertion that uses it.
# =============================================================================

STARTUP_TOKEN = "Entering project loop"
MODULE_LOADED_TOKEN = "module DLL loaded successfully"
ANALYTICS_REPORT_TOKEN = "BUILD / LINK / HOT-RELOAD ANALYTICS"
RELOAD_PROJECT_TOKEN = "[analytics] reload project"
RELOAD_MODULE_TOKEN = "[analytics] reload pill_spline"
MODULE_RELOAD_COMPLETE_TOKEN = "extension hot reload complete"
CASCADE_TOKEN = "queuing a project reload"
COUNTER_TICK_TOKEN = "counter tick"
MIGRATION_START_TOKEN = "[persistence] Selective migration starting"
MIGRATION_FINISHED_TOKEN = "[persistence] Selective migration finished"
ROLLBACK_TOKEN = "rolling back"
PANIC_TOKEN = "panicked at"
ACCESS_VIOLATION_TOKEN = "STATUS_ACCESS_VIOLATION"

# Module register-log fields (tracing renders them after the message, e.g.
# "pill_spline module registered  splines=1 existing=0 max_control_points=16").
MODULE_REGISTERED_MESSAGE = "pill_spline module registered"
# The project probe line ("[project] xxsees N spline(s), midpoint (...)").
#
# `xxsees` is not a word: it is `sees` with characters typed into it during a
# live-coding session, and it survives only because these suites pin it. That
# is the trap above in miniature - the prefix carries no meaning. If this breaks
# again, prefer widening it to "[project] " and asserting on the spline count,
# rather than restoring whatever the demo happens to print today.
PROJECT_PROBE_PREFIX = "[project] xxsees"

# =============================================================================
# Timeouts (seconds)
# =============================================================================

STARTUP_TIMEOUT = 90
RELOAD_TIMEOUT = 60
PROBE_TIMEOUT = 25
COUNTER_TICK_TIMEOUT = 15
SETTLE_TIMEOUT = 30
# A fixed settling pause after a reload token lands. It is NOT pure
# overhead: it gives the host's reload transaction time to fully unwind
# (re-seeding, watcher re-arm) before the next edit lands, so a rapid second
# edit to the same crate cannot be coalesced or missed. Trimming it made
# `module_double_reload` flaky, so it stays conservative.
STABILITY_SLEEP = 2
PROCESS_KILL_TIMEOUT = 5
BUILD_TIMEOUT = 180
MAX_BUFFERED_LINES = 8000
# Counter-tick lines are captured in a dedicated small tail so the flood can
# never evict the reload lines the scenarios assert on; 200 ticks is still far
# more than enough for any waiter to observe a fresh one.
MAX_TICK_TAIL = 200

# =============================================================================
# Console colors
# =============================================================================

ANSI_RESET = "\033[0m"
ANSI_BOLD = "\033[1m"
ANSI_DIM = "\033[2m"
ANSI_RED = "\033[31m"
ANSI_GREEN = "\033[32m"
ANSI_YELLOW = "\033[33m"
ANSI_BLUE = "\033[34m"
ANSI_MAGENTA = "\033[35m"
ANSI_CYAN = "\033[36m"


def _enable_windows_ansi() -> bool:
    """Enables ANSI color processing on Windows terminals when possible."""
    if os.name != "nt":
        return True
    try:
        import ctypes

        kernel32 = ctypes.windll.kernel32
        standard_output_handle = -11
        enable_virtual_terminal_processing = 0x0004

        handle = kernel32.GetStdHandle(standard_output_handle)
        if handle == 0:
            return False
        mode = ctypes.c_uint32()
        if kernel32.GetConsoleMode(handle, ctypes.byref(mode)) == 0:
            return False
        new_mode = mode.value | enable_virtual_terminal_processing
        if kernel32.SetConsoleMode(handle, new_mode) == 0:
            return False
        return True
    except Exception:
        return False


def _detect_color_support() -> bool:
    """Returns True when stdout supports ANSI colors and NO_COLOR is not set."""
    if os.getenv("NO_COLOR"):
        return False
    if not sys.stdout.isatty():
        return False
    return _enable_windows_ansi()


USE_COLOR = _detect_color_support()

TAG_COLOR_MAP = {
    "[FAIL]": ANSI_BOLD + ANSI_RED,
    "[OK]": ANSI_BOLD + ANSI_GREEN,
    "[WARN]": ANSI_YELLOW,
    "[TEST]": ANSI_BOLD + ANSI_BLUE,
    "[PASS]": ANSI_BOLD + ANSI_GREEN,
    "[PREP]": ANSI_BOLD + ANSI_CYAN,
    "[CLEANUP]": ANSI_BOLD + ANSI_MAGENTA,
    "[timing]": ANSI_CYAN,
    "[std]": ANSI_DIM + ANSI_CYAN,
}


def _colorize_message(message: str) -> str:
    """Applies color to known status tags in one output line."""
    if not USE_COLOR:
        return message
    colored_message = message
    for tag, color_code in TAG_COLOR_MAP.items():
        colored_message = colored_message.replace(tag, f"{color_code}{tag}{ANSI_RESET}")
    return colored_message


def print(*args, **kwargs) -> None:  # type: ignore[override]
    """Print wrapper that colors status tags while preserving normal behavior."""
    output_file = kwargs.get("file", sys.stdout)
    if output_file is not sys.stdout:
        builtins.print(*args, **kwargs)
        return
    separator = kwargs.get("sep", " ")
    end = kwargs.get("end", "\n")
    flush = kwargs.get("flush", False)
    message = separator.join(str(argument) for argument in args)
    builtins.print(_colorize_message(message), end=end, flush=flush)


# =============================================================================
# Suite timing
# =============================================================================


def run_suite_with_timing(main_fn: Callable[[], Optional[int]]) -> Optional[int]:
    """Run one test suite's `main()` wrapped in a start/end/duration log.

    Prints the script's own name with the wall-clock start time before running
    it, then the finish time and the elapsed duration afterwards, tagging the
    outcome with `[PASSED]`/`[FAILED]` (tokens the usual suite console filters
    match). Every suite in `devops/tests/` routes its `__main__` entry through
    this helper so a plain `python devops/tests/test_*.py` run reports how
    long the suite took.

    A `SystemExit` raised inside `main()` is logged with its code and then
    re-raised so the exit status propagates; any other exception is logged and
    re-raised. Returns `main()`'s value (or `None`) on a normal return so
    callers can `sys.exit(run_suite_with_timing(main))`.
    """
    # The real suite script, not sys.argv[0]: a suite may load other devops
    # modules by exec (test_harness_parsing) and mutate sys.argv for them, so
    # argv[0] does not always name the suite being timed.
    main_module = sys.modules.get("__main__")
    main_file = getattr(main_module, "__file__", None) if main_module else None
    script_name = (
        os.path.basename(main_file)
        if main_file
        else (os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "<suite>")
    )
    started_wall = time.perf_counter()
    started_local = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[timing] {script_name}: started at {started_local}")

    def report_finish(verdict: str, note: str = "") -> None:
        elapsed = time.perf_counter() - started_wall
        finished_local = time.strftime("%Y-%m-%d %H:%M:%S")
        suffix = f" ({note})" if note else ""
        print(
            f"[timing] {script_name}: finished at {finished_local} "
            f"after {elapsed:.1f}s [{verdict}]{suffix}"
        )

    try:
        result = main_fn()
    except SystemExit as exit_error:
        # A suite may signal its result through sys.exit() from inside main;
        # report the code, then propagate so the process exits with it.
        code = exit_error.code
        report_finish("PASSED" if code in (None, 0) else "FAILED")
        raise
    except BaseException:
        report_finish("FAILED", "errored")
        raise
    report_finish("PASSED" if result in (None, 0) else "FAILED")
    return result

# =============================================================================
# Backup / restore
# =============================================================================


class BackupRegistry:
    """Captures original file bytes (and timestamps) and restores them at the end.

    Why the mtime is captured too: every suite edits module/project sources and
    then restores them, and a plain byte restore leaves each file stamped
    "now" - newer than the artifacts the host built from it, so the *next*
    host start recompiles identical content from scratch (pill_spline alone
    costs ~8s). Rewinding the modification time in `restore_all` keeps cargo's
    mtime-based fingerprint of the restored source agreeing with the artifact
    it already built, and the rebuild is skipped.

    `restore_one` deliberately does NOT rewind: it runs mid-suite while the host
    is watching, and the watcher must see a freshly written file so it reloads
    the restored (original) content.
    """

    def __init__(self) -> None:
        self._originals: Dict[Path, Tuple[bytes, float]] = {}

    def capture(self, path: Path) -> None:
        """Records the original content and mtime of a file exactly once."""
        if path not in self._originals and path.exists():
            stat = path.stat()
            self._originals[path] = (path.read_bytes(), stat.st_mtime)

    def captured(self, path: Path) -> bool:
        """True when `path` was captured, i.e. cleanup will restore it.

        Suites that plan an edit before they capture assert on this: an edit
        written to a file no one captured would survive the run.
        """
        return path in self._originals

    def restore_all(self, reset_mtime: bool = True) -> None:
        """Writes every captured file back to its original bytes.

        With `reset_mtime` (the default) the original modification time is
        restored too, leaving the workspace exactly as found so the next host
        start does not rebuild identical content. Pass `reset_mtime=False` when
        the last host build was of *edited* content and nothing reloaded the
        restored bytes - rewinding the mtime there would make the next host
        trust that stale artifact as up to date.
        """
        for path, (original_bytes, original_mtime) in self._originals.items():
            try:
                path.write_bytes(original_bytes)
                if reset_mtime:
                    os.utime(path, (original_mtime, original_mtime))
                print(f"  [CLEANUP] restored {path.relative_to(WORKSPACE_ROOT)}")
            except OSError as error:
                print(f"  [CLEANUP] failed to restore {path}: {error}")

    def restore_one(self, path: Path) -> None:
        """Restores a single captured file to its original bytes (mtime stays fresh)."""
        if path in self._originals:
            original_bytes, _ = self._originals[path]
            path.write_bytes(original_bytes)


# Suite-wide backup singleton: capture every file a suite may touch at startup
# and restore them all (even on failure) at the end.
BACKUP = BackupRegistry()


# =============================================================================
# Source edit helpers
# =============================================================================


def read_source(path: Path) -> str:
    """Reads a source file as UTF-8 text, line endings exactly as stored.

    `newline=""` turns off universal-newline translation, so a CRLF file reads
    as CRLF and an LF file as LF, and `atomic_write` writes back the same bytes
    for every line an edit did not touch.
    """
    with path.open("r", encoding="utf-8", newline="") as source_file:
        return source_file.read()


def atomic_write(path: Path, content: str) -> None:
    """Writes source content atomically via temporary file + rename.

    No newline translation: without `newline=""` Windows would write every LF
    as CRLF, turning a one-line edit into a whole-file change.
    """
    if not content.endswith("\n"):
        content += "\n"
    temporary_path = path.with_suffix(path.suffix + ".tmp")
    with temporary_path.open("w", encoding="utf-8", newline="") as temporary_file:
        temporary_file.write(content)
    os.replace(str(temporary_path), str(path))


# What regenerating the shipping bundle rewrites: the tracked bundle crate and,
# when its dependencies change, the workspace lockfile.
SHIPPING_BUNDLE_PATHS = (
    WORKSPACE_ROOT / "build" / "pill_shipping_bundle",
    MODULES_ROOT / "Cargo.lock",
)


def snapshot_paths(paths: Sequence[Path]) -> Dict[Path, bytes]:
    """Every file under `paths` (directories recursively) with its bytes."""
    snapshot: Dict[Path, bytes] = {}
    for root in paths:
        files = [root] if root.is_file() else [path for path in root.rglob("*") if path.is_file()]
        for path in files:
            snapshot[path] = path.read_bytes()
    return snapshot


def restore_paths(paths: Sequence[Path], snapshot: Dict[Path, bytes]) -> None:
    """Put `paths` back exactly as `snapshot` recorded them.

    Files created since the snapshot under a snapshotted directory are removed;
    every recorded file gets its recorded bytes back.
    """
    for root in paths:
        if root.is_dir():
            for path in root.rglob("*"):
                if path.is_file() and path not in snapshot:
                    path.unlink()
    for path, contents in snapshot.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.exists() or path.read_bytes() != contents:
            path.write_bytes(contents)


def apply_replacements(path: Path, replacements: Sequence[Tuple[str, str]]) -> bool:
    """Applies ordered replacements against the current source content."""
    content = read_source(path)
    for old_text, new_text in replacements:
        if old_text not in content:
            print(
                f"  [FAIL] Edit pattern not found in {path.relative_to(WORKSPACE_ROOT)}: "
                f"{old_text[:80].strip()!r}...",
            )
            return False
        content = content.replace(old_text, new_text, 1)
    atomic_write(path, content)
    return True


# `pill_spline`'s sample-offset constant, inside `get_location_at`. A plain
# `f32` literal on one line, so exactly one line matches and the indent comes
# back with it.
SAMPLE_OFFSET_PATTERN = re.compile(
    r"^(\s*)const SAMPLE_VERTICAL_OFFSET:\s*f32\s*=\s*([0-9.]+)\s*;?\s*$", re.MULTILINE
)


@dataclass(frozen=True)
class SplineSampleOffsetEdit:
    """The edit that bumps `pill_spline`'s sample offset by one."""

    old_line: str
    new_line: str
    current_offset: float
    new_offset: float

    @property
    def delta(self) -> float:
        """How far the sampled point moves vertically, in world units."""
        return self.new_offset - self.current_offset

    @property
    def replacements(self) -> Sequence[Tuple[str, str]]:
        """The pair `apply_replacements` takes."""
        return ((self.old_line, self.new_line),)


def edit_spline_sample_offset(content: str) -> SplineSampleOffsetEdit:
    """Plans the one edit that bumps `pill_spline`'s sample offset by one.

    The constant is the demo's behaviour-only knob: it moves the project
    probe's midpoint without touching any component's shape, so the reload
    takes the ordinary path rather than a schema migration. The native
    hot-reload suite's cascade scenario and the benchmark harness trigger a
    reload with it, which is why the edit is planned from the current source
    instead of pinned to a literal pair.

    A pinned pair ("= 0.0" -> "= 10.0") only applies to a source nobody has
    edited yet, so the suites were order-dependent: a run that ended on a
    bumped offset broke the next suite's first edit with a pattern-not-found
    failure that read like a broken layout. The offset wraps at 10.0 so the
    constant stays a single digit and the derived probe midpoint stays a
    stable one-decimal value.

    Raises RuntimeError when the constant is missing or ambiguous, which means
    the source is not the one these suites edit.
    """
    matches = list(SAMPLE_OFFSET_PATTERN.finditer(content))
    if len(matches) != 1:
        raise RuntimeError(
            f"Expected exactly one SAMPLE_VERTICAL_OFFSET line, found {len(matches)}"
        )
    match = matches[0]
    current_offset = float(match.group(2))
    new_offset = (current_offset + 1.0) % 10.0
    return SplineSampleOffsetEdit(
        old_line=match.group(0),
        new_line=f"{match.group(1)}const SAMPLE_VERTICAL_OFFSET: f32 = {new_offset:.1f};",
        current_offset=current_offset,
        new_offset=new_offset,
    )

# =============================================================================
# Output monitor
# =============================================================================


class OutputMonitor:
    """Captures merged stdout/stderr lines from the host in a background thread.

    Lines are stored as ``(sequence, line)`` pairs with a monotonically
    increasing sequence number, so ``wait_for`` / ``output_since`` stay correct
    even after the rolling buffer drops old entries.

    The host's counter system floods thousands of identical "counter tick"
    lines per second. Those ticks would roll the buffer (and with it every
    reload line) within a second, so tick lines go into a small dedicated tail
    instead of the main buffer: they stay findable for waiters (they are always
    fresh under the flood) without evicting the lines the scenarios assert on.
    """

    def __init__(self, process: subprocess.Popen) -> None:
        self._process = process
        self._lines: List[Tuple[int, str]] = []
        self._tick_tail: List[Tuple[int, str]] = []
        self._sequence = 0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None

    def start(self) -> None:
        """Starts asynchronous output capture."""
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Signals the output thread to stop."""
        self._stop.set()

    def _read_loop(self) -> None:
        """Reads process output line by line and keeps the rolling buffers."""
        try:
            for line in iter(self._process.stdout.readline, ""):
                if self._stop.is_set():
                    break
                with self._lock:
                    sequence = self._sequence
                    self._sequence += 1
                    if COUNTER_TICK_TOKEN in line:
                        self._tick_tail.append((sequence, line))
                        if len(self._tick_tail) > MAX_TICK_TAIL:
                            self._tick_tail = self._tick_tail[-MAX_TICK_TAIL:]
                    else:
                        self._lines.append((sequence, line))
                        if len(self._lines) > MAX_BUFFERED_LINES:
                            self._lines = self._lines[-MAX_BUFFERED_LINES:]
                # Forward to the console for visibility, but not the counter
                # tick flood - thousands of identical lines a second drown the
                # log without adding information.
                if COUNTER_TICK_TOKEN not in line:
                    print(f"  [std] {line.rstrip()}")
        except (ValueError, OSError) as error:
            # Reached on a genuine read failure, not on the normal end of
            # stream (a closed pipe ends the iteration without raising). This
            # used to pass silently: the reader stopped draining, the host
            # blocked on its next write, and every later assertion timed out
            # for a reason nothing in the log explained.
            print(f"  [FAIL] Output monitor stopped reading: {error!r}")

    def _iter_since(self, start_index: int):
        """Yields buffered lines with sequence number >= start_index."""
        for sequence, line in self._lines:
            if sequence >= start_index:
                yield line
        for sequence, line in self._tick_tail:
            if sequence >= start_index:
                yield line

    @property
    def line_count(self) -> int:
        """Returns the sequence number of the next line to arrive."""
        with self._lock:
            return self._sequence

    def output_since(self, start_index: int) -> str:
        """Returns output buffered for sequence numbers >= start_index."""
        with self._lock:
            return "".join(self._iter_since(start_index))

    def process_alive(self) -> bool:
        """Returns True while the process is still running."""
        return self._process.poll() is None

    def wait_for(self, token: str, timeout_seconds: float, start_index: int = 0) -> bool:
        """Waits until a token appears in the output (or timeout / exit)."""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                return False
            with self._lock:
                for line in self._iter_since(start_index):
                    if token in line:
                        return True
            time.sleep(0.1)
        return False

    def wait_for_any(
        self,
        tokens: Sequence[str],
        timeout_seconds: float,
        start_index: int = 0,
    ) -> Optional[str]:
        """Waits until any of the given tokens appears; returns it or None."""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                return None
            with self._lock:
                for line in self._iter_since(start_index):
                    for token in tokens:
                        if token in line:
                            return token
            time.sleep(0.1)
        return None

# =============================================================================
# Process lifecycle helpers
# =============================================================================


def has_crash_signals(output: str) -> bool:
    """Checks output for known crash signatures."""
    return PANIC_TOKEN in output or ACCESS_VIOLATION_TOKEN in output


# =============================================================================
# Host lock: one host-driving script at a time
# =============================================================================

# Every script that drives `pill_standalone` kills leftover hosts, builds into
# the shared `modules/target` and edits shared fixture sources, so two of them
# in one window fight over all three. That was paid for once already: a
# migration scenario looked flaky until the cause turned out to be a second
# suite started in parallel, each one's `kill_stale_hosts` killing the other's
# host (storage plan, section K). The fix is an exclusive lock on a
# machine-global file, taken by every process that owns a host and held until
# the process exits - the OS releases it on death, so there is no stale lock to
# recover from.
HOST_LOCK_PATH = Path(tempfile.gettempdir()) / "pill_host_suite.lock"

# The open descriptor holding the lock, once this process has it. A dict keeps
# the module-level mutation one obvious line instead of a `global` statement in
# every function that touches it; None means "not held".
_HOST_LOCK = {"fd": None}  # type: Dict[str, Optional[int]]

# Fixed-width PID record: always 12 bytes, so a later holder can overwrite it
# without truncation and a reader can decode it with a plain strip().
_HOST_LOCK_RECORD_WIDTH = 12


def _lock_fd_nonblocking(fd: int) -> bool:
    """Tries to take the exclusive lock on `fd`; False if another process holds it."""
    try:
        if os.name == "nt":
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except OSError:
        return False


def ensure_host_lock(poll_seconds: float = 1.0) -> None:
    """Takes the machine-global host lock, waiting for any current holder.

    Idempotent within one process: the first call locks, later calls return.
    One line is printed when the call has to wait, naming the holder's PID from
    the lock file, so a queued run explains itself instead of looking hung. The
    lock is released by `release_host_lock`, or by process exit - which is why
    a killed suite never leaves the next one stuck.
    """
    if _HOST_LOCK["fd"] is not None:
        return
    fd = os.open(HOST_LOCK_PATH, os.O_CREAT | os.O_RDWR)
    waited = False
    while not _lock_fd_nonblocking(fd):
        if not waited:
            holder = ""
            try:
                os.lseek(fd, 0, os.SEEK_SET)
                raw = os.read(fd, _HOST_LOCK_RECORD_WIDTH).decode("ascii", "replace").strip()
                if raw.isdigit():
                    holder = f" (held by PID {raw})"
            except OSError:
                pass
            print(
                f"  [WAIT] Another host-driving suite holds {HOST_LOCK_PATH}{holder};"
                " waiting for it to finish."
            )
            waited = True
        time.sleep(poll_seconds)
    # Record who holds it, for the next process that has to wait.
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        os.write(fd, f"{os.getpid():>{_HOST_LOCK_RECORD_WIDTH}}".encode("ascii"))
    except OSError:
        pass
    _HOST_LOCK["fd"] = fd


def release_host_lock() -> None:
    """Releases the host lock early; process exit releases it either way."""
    fd = _HOST_LOCK["fd"]
    if fd is None:
        return
    try:
        if os.name == "nt":
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_UN)
    except OSError:
        pass
    finally:
        os.close(fd)
        _HOST_LOCK["fd"] = None


# =============================================================================
# Host postures: one definition of each host build and launch
# =============================================================================

# A suite's host is never "a pill_standalone": it is one built with a specific
# feature set, and the set is not a preference. `pill_core.dll` and
# `pill_engine_core.dll` export symbols hashed from their resolved features, so
# a host built in one posture and a project DLL resolved in another fail at load
# with "The specified procedure could not be found" (os error 127), naming
# nothing. Each suite used to spell its own feature list, twice - once for the
# build, once for the launch - which is how the two came apart; a `HostPosture`
# is that one spelling, shared by both.
@dataclass(frozen=True)
class HostPosture:
    """One feature selection for `pill_standalone`, plus the cargo flags it needs."""

    name: str
    feature_args: Tuple[str, ...] = ()
    # Flags that select no feature and change no binary - `--offline` - so two
    # spellings of one graph stay one posture, and a `--skip-build` run by a
    # sibling suite still recognizes the binary.
    cargo_args: Tuple[str, ...] = ()

    @property
    def build_command(self) -> List[str]:
        """The `cargo build` that produces this posture's host binary."""
        return [
            "cargo",
            "build",
            "-p",
            "pill_standalone",
            *self.feature_args,
            *self.cargo_args,
        ]

    @property
    def launch_command(self) -> List[str]:
        """The `cargo run` that launches this posture's host.

        Through cargo rather than by spawning the binary: `-C prefer-dynamic`
        means the host needs the toolchain's `std-*.dll` and the workspace's
        `pill_core.dll` on the loader path, which `cargo run` sets up. After
        `build_host` built the same posture this compiles nothing.
        """
        return [
            "cargo",
            "run",
            "-p",
            "pill_standalone",
            *self.feature_args,
            *self.cargo_args,
        ]

    @property
    def description(self) -> str:
        """The build command line, for the `[BUILD]` line a suite prints."""
        return " ".join(self.build_command)


# The postures in use. `--features rendering` on the defaults is the same graph
# as the `--features pill_host/hot_patch,rendering` the hot-patch suites used to
# spell out: `hot_patch` is a default feature and it is what forwards
# `pill_host/hot_patch`.
HOST_DEV_HOT_PATCH = HostPosture("dev+hot_patch")
HOST_DEV = HostPosture("dev", ("--no-default-features", "--features", "dev"))
HOST_DEV_HOT_PATCH_RENDERING = HostPosture("dev+hot_patch+rendering", ("--features", "rendering"))
HOST_DEV_RENDERING = HostPosture(
    "dev+rendering",
    ("--no-default-features", "--features", "dev,rendering"),
    ("--offline",),
)

# What a host build compiles, for `verify_host_build`'s staleness check.
# Extension and project sources are deliberately absent: the host loads those as
# DLLs, so editing `pill_spline` - which several suites do, leaving the file
# stamped "now" - does not make cargo rebuild the host, and counting it here
# would refuse `--skip-build` for a binary that is in fact up to date.
#
# `Cargo.lock` is absent for the same reason. The host regenerates its
# `host_module_*` and `host_project_*` members on every start and prunes them on
# exit, so the `cargo run` that follows every build rewrites the lockfile: a
# record stamped 08:00:03.098 was followed by a lockfile written at
# 08:00:03.830, and a lockfile newer than the record then refused every
# `--skip-build` run made after a suite. The manifests that produce the lock are
# roots themselves, so a dependency change is still caught by their mtimes.
HOST_SOURCE_ROOTS = (
    MODULES_ROOT / "Cargo.toml",
    MODULES_ROOT / ".cargo",
    MODULES_ROOT / "pill_core",
    MODULES_ROOT / "pill_core_macros",
    MODULES_ROOT / "pill_engine",
    MODULES_ROOT / "pill_engine_core",
    MODULES_ROOT / "pill_assets",
    MODULES_ROOT / "pill_input",
    MODULES_ROOT / "hosting",
    MODULES_ROOT / "frontents" / "pill_standalone",
)

# What `build_host` records about the binary it produced. A binary carries no
# feature tag, and cargo exports no `CARGO_FEATURE_*` to the process that runs
# one, so a suite told to `--skip-build` cannot ask "was this built for my
# posture?" - it reads this record instead.
HOST_BUILD_RECORD = MODULES_ROOT / "target" / "pill_suite_host_build.json"

# Directories under a host source root that are not build inputs: cargo's own
# output, and the crates the host regenerates on every start.
_NOT_HOST_SOURCE_DIRS = ("target", "generated")


def _newest_host_source_mtime_ns() -> int:
    """The newest modification time among the files a host build reads."""
    newest = 0
    for root in HOST_SOURCE_ROOTS:
        if root.is_file():
            newest = max(newest, root.stat().st_mtime_ns)
            continue
        for directory, subdirectories, files in os.walk(root):
            subdirectories[:] = [
                name for name in subdirectories if name not in _NOT_HOST_SOURCE_DIRS
            ]
            for name in files:
                newest = max(newest, os.stat(os.path.join(directory, name)).st_mtime_ns)
    return newest


def _record_host_build(posture: HostPosture) -> None:
    """Records the posture and identity of the binary `build_host` just built."""
    try:
        binary_mtime_ns = HOST_EXE.stat().st_mtime_ns
    except OSError as error:
        print(f"  [WARN] Could not record the host build: {error}")
        return
    record = {
        "posture": posture.name,
        "binary_mtime_ns": binary_mtime_ns,
        "built_at": time.strftime("%Y-%m-%d %H:%M:%S"),
    }
    try:
        HOST_BUILD_RECORD.parent.mkdir(parents=True, exist_ok=True)
        HOST_BUILD_RECORD.write_text(
            json.dumps(record, indent=2) + "\n", encoding="utf-8"
        )
    except OSError as error:
        print(f"  [WARN] Could not record the host build: {error}")


def build_host(posture: HostPosture, timeout: int = BUILD_TIMEOUT) -> bool:
    """Builds `pill_standalone` in `posture`; False when the build failed.

    Takes the host lock first: the build writes the shared `modules/target`
    every other host-driving suite reads. `cargo build` rather than letting
    `cargo run` compile, so a failure is reported as a build failure with
    cargo's own stderr, and so the launch that follows compiles nothing.
    """
    ensure_host_lock()
    print(f"  [BUILD] {posture.description}")
    try:
        completed = subprocess.run(
            posture.build_command,
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] Host build timed out after {timeout} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False
    except OSError as error:
        print(f"  [FAIL] Could not build the host: {error}")
        return False
    if completed.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(completed.stderr[-2000:])
        return False
    _record_host_build(posture)
    print("  [OK] Host built.")
    return True


def host_command(posture: HostPosture) -> List[str]:
    """The `cargo run` that launches `pill_standalone` in `posture`."""
    return posture.launch_command


def verify_host_build(posture: HostPosture) -> bool:
    """Checks a `--skip-build` host against `posture`; False means rebuild.

    `--skip-build` exists so a slow machine can build once and run several
    suites; the risk it carries is a binary from another posture, or from older
    sources, and neither shows up where it happens - the host dies at load with
    "The specified procedure could not be found" (os error 127) naming nothing,
    or cargo quietly recompiles inside `cargo run`. So the flag verifies instead
    of trusting: the recorded posture must be the one asked for, the binary must
    be the one that build produced, and no host source may have changed since it.
    """
    if not HOST_EXE.exists():
        print(f"  [FAIL] --skip-build: {HOST_EXE} does not exist; build it first.")
        return False

    try:
        record = json.loads(HOST_BUILD_RECORD.read_text(encoding="utf-8"))
        record_mtime_ns = HOST_BUILD_RECORD.stat().st_mtime_ns
    except (OSError, ValueError):
        record = {}
        record_mtime_ns = 0

    recorded_posture = record.get("posture")
    if recorded_posture != posture.name:
        print(
            f"  [FAIL] --skip-build: the host was built for posture "
            f"{recorded_posture!r}, this suite needs {posture.name!r}."
        )
        print("         Rebuild it without --skip-build.")
        return False

    binary_mtime_ns = HOST_EXE.stat().st_mtime_ns
    if record.get("binary_mtime_ns") != binary_mtime_ns:
        print("  [FAIL] --skip-build: the host binary is not the one that build produced.")
        print("         Rebuild it without --skip-build.")
        return False

    # Against the record's own mtime, not the binary's: cargo rewrites a
    # manifest or the lock and still leaves the binary alone when nothing needs
    # recompiling, and the build that wrote this record ran cargo afterwards,
    # so it is the moment the binary was last known to match these sources.
    newest_source_mtime_ns = _newest_host_source_mtime_ns()
    if newest_source_mtime_ns > record_mtime_ns:
        print("  [FAIL] --skip-build: a host source changed after the recorded build.")
        print("         Rebuild it without --skip-build.")
        return False

    print(f"  [OK] Reusing the host built for posture {posture.name}.")
    return True


def prepare_host(
    posture: HostPosture, arguments: argparse.Namespace, timeout: int = BUILD_TIMEOUT
) -> bool:
    """Builds the host a suite drives, or verifies the one `--skip-build` reuses."""
    if getattr(arguments, "skip_build", False):
        return verify_host_build(posture)
    return build_host(posture, timeout)


def add_common_suite_arguments(parser: argparse.ArgumentParser) -> None:
    """Adds the two arguments every host-driving suite accepts.

    One definition, so the flags `run_all.py` passes through, the README and
    every suite agree on the spellings. `--skip-build` is a slow-machine
    shortcut whose binary is verified by `prepare_host`, never trusted.
    """
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Reuse the built host, after checking it matches this suite's posture",
    )


def launch_process(
    command: Sequence[str],
    cwd: Path,
    environment: Optional[Dict[str, str]] = None,
) -> Tuple[subprocess.Popen, OutputMonitor]:
    """Launches a process with merged output capture and returns (process, monitor).

    The stream is decoded as UTF-8 with replacement, not with the locale
    encoding: the host's renderer prints a box-drawing frame dashboard, and
    decoding those bytes as the console code page raises in the reader thread.
    A dead reader stops draining the pipe, the host then blocks on its next
    write, and every later assertion times out for a reason that has nothing to
    do with what it tests - so this is correctness, not tolerance.

    Takes the host lock before launching: a process that owns a host owns the
    shared build directories and fixture sources too.

    Automation turns on the host's cargo `--timings` breakdown here, once for
    every suite. The host keeps `--timings` opt-in because it costs build
    time on every reload, while the suites assert on and report the per-crate
    compile times, so they are exactly the callers that should pay for it.
    """
    ensure_host_lock()
    if environment is not None:
        environment = dict(environment)
        environment.setdefault("PILL_CARGO_TIMINGS", "1")
    process = subprocess.Popen(
        list(command),
        cwd=str(cwd),
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    monitor = OutputMonitor(process)
    monitor.start()
    return process, monitor


def terminate_process(process: subprocess.Popen, monitor: OutputMonitor) -> None:
    """Stops the monitor and terminates the host safely."""
    monitor.stop()
    try:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=PROCESS_KILL_TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    except (OSError, subprocess.SubprocessError):
        pass


def kill_stale_hosts() -> None:
    """Best-effort cleanup of leftover host and editor processes that lock shared DLLs.

    Both images lock the shared DLLs: host suites drive `pill_standalone` and
    the editor suites drive `pill_editor_egui`, so both are cleared
    the same way. Takes the host lock first: killing without it would kill
    the process a concurrent suite is driving - the exact interference this
    lock exists to stop - while killing under it can only hit a leak from an
    earlier run.
    """
    ensure_host_lock()
    for image in ("pill_standalone", "pill_editor_egui"):
        if os.name == "nt":
            subprocess.run(
                ["taskkill", "/IM", "{}.exe".format(image), "/F"],
                capture_output=True,
            )
        else:
            subprocess.run(["pkill", "-f", image], capture_output=True)


def write_project_settings(project_root: Path, content: str) -> None:
    """Writes a project's `project_settings.yaml`, the file the host reads."""
    atomic_write(project_settings_yaml(project_root), content)


# Names importable via `from suite_common import *`.
__all__ = [
    "WORKSPACE_ROOT",
    "MODULES_ROOT",
    "HOST_EXE",
    "NATIVE_PROJECT_ROOT",
    "CSHARP_PROJECT_ROOT",
    "FIXTURE_PROJECT_ROOT",
    "PROJECT_SANDBOX_LIB_RS",
    "project_settings_yaml",
    "write_project_settings",
    "STARTUP_TOKEN",
    "MODULE_LOADED_TOKEN",
    "ANALYTICS_REPORT_TOKEN",
    "RELOAD_PROJECT_TOKEN",
    "RELOAD_MODULE_TOKEN",
    "MODULE_RELOAD_COMPLETE_TOKEN",
    "CASCADE_TOKEN",
    "COUNTER_TICK_TOKEN",
    "MIGRATION_START_TOKEN",
    "MIGRATION_FINISHED_TOKEN",
    "ROLLBACK_TOKEN",
    "PANIC_TOKEN",
    "ACCESS_VIOLATION_TOKEN",
    "MODULE_REGISTERED_MESSAGE",
    "PROJECT_PROBE_PREFIX",
    "STARTUP_TIMEOUT",
    "RELOAD_TIMEOUT",
    "PROBE_TIMEOUT",
    "COUNTER_TICK_TIMEOUT",
    "SETTLE_TIMEOUT",
    "STABILITY_SLEEP",
    "PROCESS_KILL_TIMEOUT",
    "BUILD_TIMEOUT",
    "MAX_BUFFERED_LINES",
    "MAX_TICK_TAIL",
    "USE_COLOR",
    "TAG_COLOR_MAP",
    "print",
    "run_suite_with_timing",
    "BackupRegistry",
    "BACKUP",
    "read_source",
    "atomic_write",
    "apply_replacements",
    "edit_spline_sample_offset",
    "SplineSampleOffsetEdit",
    "SHIPPING_BUNDLE_PATHS",
    "snapshot_paths",
    "restore_paths",
    "OutputMonitor",
    "has_crash_signals",
    "launch_process",
    "terminate_process",
    "kill_stale_hosts",
    "HOST_LOCK_PATH",
    "ensure_host_lock",
    "release_host_lock",
    "HostPosture",
    "HOST_DEV_HOT_PATCH",
    "HOST_DEV",
    "HOST_DEV_HOT_PATCH_RENDERING",
    "HOST_DEV_RENDERING",
    "HOST_SOURCE_ROOTS",
    "HOST_BUILD_RECORD",
    "build_host",
    "host_command",
    "verify_host_build",
    "prepare_host",
    "add_common_suite_arguments",
]
