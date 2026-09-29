#!/usr/bin/env python3
"""
REQUIREMENTS:
  - Windows. The host, the session model and PsExec are all Windows-specific.
  - HIST0R-ONE itself. Every action drives that box through interfaces that
    only exist on it - taskkill, PsExec, setx, D:\\Temp - so the script checks
    which machine it is on and stops with a message when it is not that one.
  - Python 3.10 or newer.
  - `C:\\WINDOWS\\PsExec.exe` for the windowed mode (already present here).
  - `pill_standalone` buildable with `cargo build --package pill_standalone`.

DESCRIPTION:
  One entry point for the local development loop of the Pill engine, so a
  caller never has to remember the session model or the PsExec invocation.

  The host is a windowed application, and Windows OpenSSH always creates a
  session 0 (service) logon, which has no interactive desktop. A renderer
  started there cannot present and usually blocks. The interactive desktop is a
  different session, normally 1. So `build` stays in whatever session you are
  already in, while `run` hands the executable to PsExec to start it in the
  interactive session.

  Two consequences of `psexec -i` shape the design, and both are load-bearing:

    * It does not redirect stdio. A session-crossing process is a black box to
      the caller, so `run` always redirects the host's output to a file and
      `log` reads that file back. `--headless` is the exception: it needs no
      window, so it runs in the foreground and streams here directly.

    * The launch command line would otherwise pass through four layers of
      quoting (ssh -> cmd -> PsExec -> cmd), and cmd's rules for nested quotes
      are not worth the fight. Instead the inner command is written to a small
      batch file and PsExec is pointed at the file.

ACTIONS:
  pick     Choose the project module the other actions work on, and set
           PROJECT_PATH for processes started afterwards.
  build    Stop any running host, then cargo build it.
  run      Launch the host: windowed into the interactive session, or headless
           in the foreground with --headless.
  stop     Kill any running host. Mapped DLLs otherwise break the next build.
  status   Session, lock and process state, plus the tail of the host log.
  log      Print, or follow, the host log.
  cycle    stop, build, run - the edit-build-run loop as one action.

USAGE:
  python devops/tools/pill_dev.py <action> [options]

FLAGS:
  Flags are written after the action, e.g. `pill_dev.py run --headless`.
  `pill_dev.py <action> -h` prints the flags that action accepts.

  --project PATH    Project module used for PROJECT_PATH (default: the project
                    `pick` recorded, else $PROJECT_PATH, else
                    <repo>/examples/project_cs).
  --session N       Interactive session a windowed host is launched into
                    (default: 1).
  --log PATH        File a windowed host's output is redirected to
                    (default: D:\\Temp\\pill.log).
  --headless        Build without the `rendering` feature, and for `run` stay
                    in the foreground instead of opening a window.
  --release         Use the release profile and the release executable.
  --no-offline      Let cargo touch the registry (the default is --offline).
  --keep-running    Do not stop a running host first (build, cycle).
  --no-build        (run) Launch the existing executable without building.
  --lines N         Trailing log lines to print, or to seed a follow
                    (default: 40).
  --follow          Stream the host log until Ctrl-C (log, run, cycle). With
                    `run` or `cycle` the stream starts once the host is
                    launched, which is the only way to see a windowed host's
                    output - PsExec does not forward stdio.

EXAMPLE USAGE:
  python devops/tools/pill_dev.py pick
  python devops/tools/pill_dev.py pick italian_brainrot_cs
  python devops/tools/pill_dev.py cycle
  python devops/tools/pill_dev.py run --headless
  python devops/tools/pill_dev.py log --follow
--- SCRIPT ---
"""

from __future__ import annotations

import argparse
import os
import platform
import subprocess
import sys
from pathlib import Path


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# The one machine this loop drives, and the reason the guard in `main` exists:
# PsExec, the D:\Temp files and taskkill are all local to it, so a copy of this
# script running elsewhere can only fail in ways that read like host faults.
HOST_NAME = "HIST0R-ONE"
# Used by that guard's hint only, which fires on a machine that is not the host -
# where REPO_ROOT would name that machine's copy of the tree, not this one.
HOST_REPO = r"D:\Programming\Rust-Hybrid-ECS"

REPO_ROOT = Path(__file__).resolve().parents[2]
MODULES_DIRECTORY = REPO_ROOT / "modules"
DEFAULT_PROJECT = REPO_ROOT / "examples" / "project_cs"
DEFAULT_LOG = Path(r"D:\Temp\pill.log")

# A project module is a directory under examples/ with one of these beside its
# build file. `pick` scans for the marker, so a new project needs no edit here.
PROJECT_MARKER = "project_settings.yaml"
PROJECTS_DIRECTORY = REPO_ROOT / "examples"
# Where `pick` records its answer. Outside the repository for the same reason as
# LAUNCH_SCRIPT, and outside the environment because `setx` only reaches
# processes started after it runs: without this file, a pick would not be
# visible to the next action run in the terminal that made it.
PROJECT_STATE = Path(r"D:\Temp\pill_dev_project.txt")

HOST_PACKAGE = "pill_standalone"
HOST_EXECUTABLE = "pill_standalone.exe"
# The feature that turns the host into a windowed application. Without it the
# host runs the same frame loop and never touches the graphics stack.
WINDOWED_FEATURE = "rendering"

# Supplied by the PsExec distribution that ships with this machine.
PSEXEC = Path(r"C:\WINDOWS\PsExec.exe")
# The desktop every windowed launch targets. Session 0 is the one SSH gives
# you and is never the right answer here.
DEFAULT_SESSION = 1

# Where the generated launch batch is written. It must be readable by the
# process PsExec starts, and its path must need no quoting, so this is a short
# name next to the log rather than anything under the repository.
LAUNCH_SCRIPT = Path(r"D:\Temp\pill_dev_launch.bat")


def executable_path(release: bool) -> Path:
    """Location of the built host for the requested profile."""
    profile = "release" if release else "debug"
    return MODULES_DIRECTORY / "target" / profile / HOST_EXECUTABLE


def recorded_project() -> str | None:
    """The project `pick` last recorded, or None when there is no readable record."""
    try:
        recorded = PROJECT_STATE.read_text(encoding="utf-8").strip()
    except OSError:
        return None
    return recorded or None


def default_project() -> Path:
    """Project used when `--project` is absent.

    The record wins over the environment, because a terminal keeps whatever
    `PROJECT_PATH` it was started with: an older export would otherwise shadow
    every pick made since. `pick` reports a disagreement between the two rather
    than leaving the old project to come up unexplained. A recorded project that
    has since been deleted is skipped, so a stale record cannot break the loop.
    """
    for candidate in (recorded_project(), os.environ.get("PROJECT_PATH")):
        if candidate and Path(candidate).is_dir():
            return Path(candidate)
    return DEFAULT_PROJECT


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

def host_mismatch() -> str | None:
    """Name of the machine this shell is on when that is not the host, else None.

    Checked per action rather than trusted, because the wrong machine produces
    failures that read like host faults: a `setx` that sets nothing useful, a
    `taskkill` with no host to kill, a build that cannot find the toolchain.
    """
    # COMPUTERNAME first: it is what Windows itself reports, and taking it from
    # the environment is what makes this guard testable by overriding it.
    machine = os.environ.get("COMPUTERNAME") or platform.node() or "an unnamed machine"
    return None if machine.upper() == HOST_NAME.upper() else machine


def host_environment(project: Path) -> dict[str, str]:
    """Environment for the host build and for the host process.

    `PROJECT_PATH` selects the project module, and exactly one name is needed
    for both callers because the host resolves the path itself at startup.
    The POSIX form is used deliberately: it needs no quoting inside the launch
    batch file, and both cargo and the host accept it.
    """
    environment = os.environ.copy()
    environment["PROJECT_PATH"] = project.as_posix()
    # A panicking host is the failure mode worth having detail for, in both the
    # windowed and the headless path.
    environment["RUST_BACKTRACE"] = "1"
    return environment


def announce(command: list[str]) -> None:
    """Echo a command before running it, so a failed action is reproducible."""
    print(f"$ {' '.join(command)}", flush=True)


def launch_script_text(project: Path, executable: Path, log: Path) -> str:
    """The inner command for a windowed launch, as a batch file body.

    A batch file rather than a `cmd /c "..."` string: the command reaches
    PsExec through ssh, and nested double quotes survive that trip only by
    luck. A title is set so the console window that appears on the desktop
    says which project it is running.
    """
    lines = [
        "@echo off",
        f"title pill_standalone {project.name}",
        f'cd /d "{MODULES_DIRECTORY}"',
        f'set "PROJECT_PATH={project.as_posix()}"',
        # A panicking host is the failure mode this loop actually hits, and a
        # backtrace is what makes the panic actionable.
        'set "RUST_BACKTRACE=1"',
        # Truncates the previous run's log, which is what `log` reads back.
        f'"{executable}" > "{log}" 2>&1',
    ]
    return "\r\n".join(lines) + "\r\n"


def write_launch_script(project: Path, executable: Path, log: Path) -> None:
    """Write the launch batch, creating the directories it depends on."""
    for directory in (LAUNCH_SCRIPT.parent, log.parent):
        directory.mkdir(parents=True, exist_ok=True)
    # Written as bytes with explicit CRLF endings: cmd is unreliable with a
    # batch file that uses bare LF.
    LAUNCH_SCRIPT.write_bytes(
        launch_script_text(project, executable, log).encode("utf-8")
    )


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------

def display_path(path: Path) -> str:
    """Repository-relative when the path is inside it, absolute otherwise."""
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def discover_projects() -> list[Path]:
    """Project modules under examples/, in name order."""
    return sorted(path.parent for path in PROJECTS_DIRECTORY.glob(f"*/{PROJECT_MARKER}"))


def project_language(project: Path) -> str:
    """Label for the picker, taken from the project's own build file."""
    if next(project.glob("*.csproj"), None) is not None:
        return "C#"
    return "Rust" if (project / "Cargo.toml").is_file() else "?"


def resolve_project(requested: str) -> Path | None:
    """The project the caller named, or None with the candidates listed.

    A bare directory name (`italian_brainrot_cs`), a path relative to the
    repository, and an absolute path are all accepted, in that order.
    """
    for candidate in (
        PROJECTS_DIRECTORY / requested,
        REPO_ROOT / requested,
        Path(requested),
    ):
        if (candidate / PROJECT_MARKER).is_file():
            return candidate.resolve()

    print(f"no such project: {requested}", file=sys.stderr)
    for project in discover_projects():
        print(f"  {display_path(project)}", file=sys.stderr)
    return None


def prompt_for_project(projects: list[Path]) -> Path | None:
    """Numbered menu over the discovered projects; None when nothing is chosen.

    Deliberately not gated on `isatty`. Over ssh this reaches the terminal as a
    plain channel rather than a tty, and a typed answer still arrives through
    it; a closed stdin is the one case that means nobody can answer, and that is
    what the message below reports.
    """
    for index, project in enumerate(projects, start=1):
        print(f"  {index}  {display_path(project):<38} {project_language(project)}")
    try:
        answer = input("select a project (number or name): ").strip()
    except (EOFError, KeyboardInterrupt):
        answer = ""
    if not answer:
        print("nothing picked; name one instead: pill_dev.py pick <name>", file=sys.stderr)
        return None
    if answer.isdigit():
        index = int(answer)
        if 1 <= index <= len(projects):
            return projects[index - 1]
        print(f"out of range: {answer}", file=sys.stderr)
        return None
    return resolve_project(answer)


def pick_project(args: argparse.Namespace) -> int:
    """Record the project the other actions should use.

    Two records, for two readers. The environment variable is what the host
    reads itself, so it covers a launch that never goes through this script; the
    state file is what `--project` falls back to, so the pick takes effect
    without waiting for a new terminal. They are written together and both are
    reported, because they can disagree.
    """
    picked = resolve_project(args.name) if args.name else prompt_for_project(discover_projects())
    if picked is None:
        return 2

    # POSIX form, as everywhere else this file hands a path to the host: it
    # needs no quoting in the launch batch, and cargo and the host accept it.
    location = picked.as_posix()
    PROJECT_STATE.parent.mkdir(parents=True, exist_ok=True)
    PROJECT_STATE.write_text(location + "\n", encoding="utf-8")

    command = ["setx", "PROJECT_PATH", location]
    announce(command)
    persisted = subprocess.run(command, capture_output=True, text=True)

    print()
    print(f"picked {display_path(picked)}")
    if persisted.returncode == 0:
        print("  PROJECT_PATH is set for processes started from now on")
    else:
        # Still a successful pick: the state file governs every action in this
        # script, so a missing setx costs the direct-launch path only.
        print(
            f"  setx failed with {persisted.returncode};"
            " a host launched outside this script will not see the pick",
            file=sys.stderr,
        )

    exported = os.environ.get("PROJECT_PATH")
    if exported and Path(exported) != picked:
        # setx cannot reach the terminal that ran it, so say it here rather than
        # letting the previous project come up unexplained.
        print(f"  this terminal still exports PROJECT_PATH={exported}, which wins")
        print("  for a direct launch until the terminal is reopened")
    print()
    print("then: pill_dev.py run --no-build")
    return 0


def stop_host(quiet: bool = False) -> int:
    """Kill every running host. Always succeeds; a missing host is not an error.

    Stopping first is not politeness. The host holds its DLLs mapped, so a
    rebuild that replaces one fails with `os error 5`, and the error surfaces
    as a confusing linker message rather than as a lock.
    """
    result = subprocess.run(
        ["taskkill", "/IM", HOST_EXECUTABLE, "/F"],
        capture_output=True,
        text=True,
    )
    # Exit code, not output: taskkill's messages are localised.
    if result.returncode == 0:
        print(f"stopped {HOST_EXECUTABLE}")
    elif not quiet:
        print(f"{HOST_EXECUTABLE} was not running")
    return 0


def build_host(args: argparse.Namespace) -> int:
    """Build the host, stopping a running instance first unless told not to."""
    if not args.keep_running:
        stop_host(quiet=True)

    command = ["cargo", "build", "--package", HOST_PACKAGE]
    if not args.headless:
        command += ["--features", WINDOWED_FEATURE]
    if args.release:
        command.append("--release")
    if args.offline:
        command.append("--offline")

    announce(command)
    return subprocess.run(
        command,
        cwd=MODULES_DIRECTORY,
        env=host_environment(args.project),
    ).returncode


def run_headless(args: argparse.Namespace) -> int:
    """Run the host in the foreground: no window, so no session gymnastics.

    A headless host never touches the graphics stack, which means it is happy
    in the session 0 shell SSH gives you, and its output streams straight here
    instead of being funnelled through a log file.
    """
    executable = executable_path(args.release)
    if not executable.exists():
        print(f"host is not built: {executable}", file=sys.stderr)
        print("build it first: pill_dev.py build --headless", file=sys.stderr)
        return 2

    command = [str(executable)]
    announce(command)
    return subprocess.run(command, env=host_environment(args.project)).returncode


def run_windowed(args: argparse.Namespace) -> int:
    """Launch the host into the interactive session, detached.

    Nothing comes back from the launched process, so the log path is reported
    instead: that file is the only view of the host's output.
    """
    if not PSEXEC.exists():
        print(f"PsExec not found at {PSEXEC}", file=sys.stderr)
        return 2

    executable = executable_path(args.release)
    if not executable.exists():
        print(f"host is not built: {executable}", file=sys.stderr)
        print("build it first: pill_dev.py build", file=sys.stderr)
        return 2

    write_launch_script(args.project, executable, args.log)

    command = [
        str(PSEXEC),
        "-accepteula",
        "-nobanner",
        "-i",
        str(args.session),
        "-d",
        "cmd.exe",
        "/c",
        str(LAUNCH_SCRIPT),
    ]
    announce(command)
    # PsExec's exit status is not usable as a success signal: with -d it returns
    # its own code (observed 188) even when the launch worked, because it
    # reports on the service handshake rather than on the process it started.
    # The launch is therefore reported unconditionally, and the log is what
    # actually proves the host came up.
    subprocess.run(command)

    print()
    print(f"launched into session {args.session}, log: {args.log}")
    if args.follow:
        # Nothing about the launched process can reach this terminal: `psexec
        # -i` does not forward stdio. Reading the redirected log back is the
        # only way to see the host's output.
        return show_log(args, lines=args.lines, follow=True)
    print("watch it with: pill_dev.py log --follow")
    print("verify the session with: pill_dev.py status")
    return 0


STATUS_SCRIPT = r"""
$shellSession = (Get-Process -Id $PID).SessionId
$desktopSession = Get-Process explorer -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty SessionId
$locked = [bool](Get-Process LogonUI -ErrorAction SilentlyContinue)
$hostProcess = Get-Process pill_standalone -ErrorAction SilentlyContinue |
    Select-Object -First 1

"this shell session  : $shellSession"
if ($desktopSession) {
    "desktop session    : $desktopSession"
} else {
    "desktop session    : none logged in"
}
# VNC captures the active desktop, so a locked session means the window is
# rendering invisibly behind the lock screen.
"desktop locked     : $locked"
if ($hostProcess) {
    "host               : pid $($hostProcess.Id) in session $($hostProcess.SessionId)"
} else {
    "host               : not running"
}
""".strip()


def show_status(args: argparse.Namespace) -> int:
    """Report whether a windowed host could work, and what it is doing.

    The session and lock lines exist because both are silent failure modes: a
    host in session 0 blocks, and a host behind a locked desktop renders where
    nobody can see it.
    """
    subprocess.run(["powershell", "-NoProfile", "-Command", STATUS_SCRIPT])
    print()
    print(f"log                : {args.log}")
    return show_log(args, lines=6, follow=False, header=False)


def show_log(args: argparse.Namespace, *, lines: int, follow: bool, header: bool = True) -> int:
    """Print the tail of the host log, or stream it until interrupted."""
    if not args.log.exists():
        print(f"no log yet: {args.log}", file=sys.stderr)
        return 1

    if follow:
        print(f"following {args.log} (Ctrl-C to stop)", flush=True)
        # Delegated to PowerShell so there is no dependency on a tail binary.
        try:
            return subprocess.run(
                [
                    "powershell",
                    "-NoProfile",
                    "-Command",
                    f"Get-Content -LiteralPath '{args.log}' -Wait -Tail {lines}",
                ]
            ).returncode
        except KeyboardInterrupt:
            # Ctrl-C ends the follow, not the host: the windowed process belongs
            # to PsExec, so nothing here is waiting on it. Say so, rather than
            # letting the interrupt surface as a traceback that reads like the
            # launcher failed.
            print()
            print("stopped following; the host is still running")
            print("kill it with: pill_dev.py stop")
            return 0

    if header:
        print(f"--- last {lines} lines of {args.log} ---")
    text = args.log.read_text(encoding="utf-8", errors="replace")
    for line in text.splitlines()[-lines:]:
        print(line)
    return 0


def cycle(args: argparse.Namespace) -> int:
    """stop, build, run - the loop actually used while editing this project."""
    code = stop_host(quiet=True)
    if code != 0:
        return code
    code = build_host(args)
    if code != 0:
        return code
    return run_headless(args) if args.headless else run_windowed(args)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def parse_arguments(argv: list[str]) -> argparse.Namespace:
    # Options every action accepts live on a parent parser inherited by each
    # subcommand, rather than on the top-level parser. argparse only recognises
    # top-level options *before* the action name, so defining them there would
    # make `pill_dev.py run --headless` fail with "unrecognized arguments"
    # while `pill_dev.py --headless run` worked. Action-first is the form that
    # reads naturally and the form every caller uses.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument(
        "--project",
        type=Path,
        default=default_project(),
        help="project module for PROJECT_PATH (default: %(default)s)",
    )
    common.add_argument(
        "--session",
        type=int,
        default=DEFAULT_SESSION,
        help="interactive session for a windowed launch (default: %(default)s)",
    )
    common.add_argument(
        "--log",
        type=Path,
        default=DEFAULT_LOG,
        help="file a windowed host's output goes to (default: %(default)s)",
    )
    common.add_argument(
        "--headless",
        action="store_true",
        help="omit the rendering feature and stay in the foreground",
    )
    common.add_argument(
        "--release",
        action="store_true",
        help="use the release profile and executable",
    )
    common.add_argument(
        "--no-offline",
        dest="offline",
        action="store_false",
        help="let cargo touch the registry (default is --offline)",
    )
    # Only meaningful for the actions that build, but accepted everywhere so a
    # flag never has to be moved in front of the action.
    common.add_argument(
        "--keep-running",
        action="store_true",
        help="do not stop a running host first (build, cycle)",
    )
    # Also shared, because the log flags are useful to `run` and `cycle` too:
    # after a windowed launch, following the log is the only way to see the
    # host's output.
    common.add_argument(
        "--lines",
        type=int,
        default=40,
        help="trailing log lines to print, or to seed a follow (default: %(default)s)",
    )
    common.add_argument(
        "--follow",
        action="store_true",
        help="stream the host log until Ctrl-C (log, run, cycle)",
    )

    parser = argparse.ArgumentParser(
        prog="pill_dev.py",
        description="Local development loop for the Pill engine host.",
    )
    actions = parser.add_subparsers(dest="action", required=True)

    actions.add_parser(
        "build",
        parents=[common],
        help="stop a running host, then build it",
    )
    run = actions.add_parser(
        "run",
        parents=[common],
        help="launch the host (windowed, or headless in the foreground)",
    )
    run.add_argument(
        "--no-build",
        dest="build_first",
        action="store_false",
        help="launch the existing executable without building",
    )
    actions.add_parser("stop", parents=[common], help="kill any running host")
    actions.add_parser(
        "status",
        parents=[common],
        help="session, lock and process state, plus the log tail",
    )
    actions.add_parser(
        "log",
        parents=[common],
        help="print or follow the host log",
    )
    actions.add_parser("cycle", parents=[common], help="stop, build, run")
    # The project is the positional argument here rather than `--project`: this
    # action's whole job is to decide what that default should be.
    pick = actions.add_parser(
        "pick",
        parents=[common],
        help="record the project the other actions use, and set PROJECT_PATH",
    )
    pick.add_argument(
        "name",
        nargs="?",
        help="project directory name or path; asked for when omitted",
    )

    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_arguments(argv)

    # After parsing so that `-h` still answers anywhere, and before dispatch so
    # that no action can half-run against the wrong box.
    machine = host_mismatch()
    if machine:
        action = argv[0] if argv else "<action>"
        print(f"pill_dev.py drives {HOST_NAME} and this shell is on {machine}.", file=sys.stderr)
        print(f"from a shell on {HOST_NAME}:", file=sys.stderr)
        print(f"  python devops\\tools\\pill_dev.py {action}", file=sys.stderr)
        print("from anywhere else:", file=sys.stderr)
        print(
            f"  ssh {HOST_NAME} 'cd /d {HOST_REPO}"
            f" && python devops\\tools\\pill_dev.py {action}'",
            file=sys.stderr,
        )
        return 2

    if args.action == "pick":
        return pick_project(args)
    if args.action == "stop":
        return stop_host()
    if args.action == "build":
        return build_host(args)
    if args.action == "log":
        return show_log(args, lines=args.lines, follow=args.follow)
    if args.action == "status":
        return show_status(args)
    if args.action == "cycle":
        return cycle(args)
    if args.action == "run":
        if args.build_first and (code := build_host(args)) != 0:
            return code
        return run_headless(args) if args.headless else run_windowed(args)

    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
