"""
Builds a project for the browser and checks that it runs and draws there.

REQUIREMENTS
  - Python 3.8+ (standard library only; Pillow, when installed, adds the
    picture check)
  - Rust toolchain with the `wasm32-unknown-unknown` target and `wasm-pack` on
    PATH (see `devops/tools/build_web.py`)
  - Chrome or Edge with WebGPU (Chrome/Edge 113+); `PILL_WEB_BROWSER` may name
    the executable

DESCRIPTION
    `devops/tools/build_web.py --dev` builds `examples/master_renderer_test`, a
    project with a `res` directory, so the asset pack is exercised too. The
    build is served on a free local port and opened in a headless browser with
    WebGPU enabled, driven through the Chrome DevTools Protocol. Before the
    page loads, a listener is injected on every WebGPU device, so validation
    errors the engine does not see are still reported.

    Asserted:
      1. The embedded asset pack is mounted.
      2. The renderer presents a first frame with a camera.
      3. Frames keep running (frame statistics are reported).
      4. No engine error, WebGPU uncaptured error or page exception occurs.
      5. With Pillow installed: at least one capture of the canvas, taken once
         per second, shows a lit scene rather than an empty frame. The scene
         rotates, so not every moment shows it.

    The shipping bundle the build regenerates is restored afterwards, so the
    tree is left as it was found.

USAGE
  python devops/tests/test_web_smoke.py [--timeout-scale N] [--seconds N]

EXAMPLE USAGE
  python devops/tests/test_web_smoke.py
  python devops/tests/test_web_smoke.py --timeout-scale 2 --seconds 20

--- SCRIPT ---
"""

# Standard library
import argparse
import base64
import functools
import http.server
import io
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from core.paths import REPOSITORY_ROOT  # noqa: E402
from core.suite_common import run_suite_with_timing  # noqa: E402

# =============================================================================
# Constants
# =============================================================================

# The project built for the browser, and where its web build lands.
PROJECT = "examples/master_renderer_test"
WEB_BUILD_DIRECTORY = REPOSITORY_ROOT / PROJECT / "build" / "web"
BUILD_SCRIPT = REPOSITORY_ROOT / "devops" / "tools" / "build_web.py"
BUNDLE_DIRECTORY = REPOSITORY_ROOT / "build" / "pill_shipping_bundle"

# Tokens the page's console must show.
PACK_TOKEN = "mounted the embedded asset pack"
FIRST_FRAME_TOKEN = "[render] First frame: Presented; camera=true"
FRAME_TOKEN = "frame statistics"

# A blurred capture whose brightness varies this much shows a scene; an empty
# frame is the clear colour under the lens grain, which blurs flat.
SCENE_STANDARD_DEVIATION = 15.0

BUILD_TIMEOUT = 1800
BROWSER_START_TIMEOUT = 30

# Browsers tried in order when `PILL_WEB_BROWSER` is not set.
BROWSER_CANDIDATES = (
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
)

# Reports every WebGPU uncaptured error and device loss to the console; runs
# before the page's own scripts.
INJECTED_SCRIPT = r"""
(() => {
  if (!window.GPUAdapter) return;
  const requestDevice = GPUAdapter.prototype.requestDevice;
  GPUAdapter.prototype.requestDevice = async function (...args) {
    const device = await requestDevice.apply(this, args);
    device.addEventListener("uncapturederror", (event) =>
      console.error("WEBGPU-UNCAPTURED " + event.error.message));
    device.lost.then((info) => {
      if (info.reason !== "destroyed") console.error("WEBGPU-LOST " + info.message);
    });
    return device;
  };
})();
"""

# =============================================================================
# DevTools connection
# =============================================================================


class DevToolsSocket:
    """A minimal client-side WebSocket, enough for the DevTools protocol."""

    def __init__(self, url: str):
        address = url[len("ws://"):]
        host_port, path = address.split("/", 1)
        host, port = host_port.split(":")
        self.connection = socket.create_connection((host, int(port)))
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.connection.sendall(request.encode())
        response = b""
        while b"\r\n\r\n" not in response:
            response += self.connection.recv(1)
        self.next_identifier = 0

    def send(self, method: str, params=None) -> int:
        """Send one command as a masked text frame; returns its id."""
        self.next_identifier += 1
        payload = json.dumps(
            {"id": self.next_identifier, "method": method, "params": params or {}}
        ).encode()
        header = bytearray([0x81])
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        mask = os.urandom(4)
        header += mask
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.connection.sendall(bytes(header) + masked)
        return self.next_identifier

    def read_exactly(self, count: int) -> bytes:
        """Read `count` bytes or raise when the connection closes."""
        data = b""
        while len(data) < count:
            chunk = self.connection.recv(count - len(data))
            if not chunk:
                raise ConnectionError("the browser closed the DevTools connection")
            data += chunk
        return data

    def receive(self) -> dict:
        """Read one message, reassembling continuation frames."""
        message = b""
        while True:
            first, second = self.read_exactly(2)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self.read_exactly(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self.read_exactly(8))[0]
            message += self.read_exactly(length)
            if first & 0x80:
                return json.loads(message.decode("utf-8", errors="replace"))


# =============================================================================
# Steps
# =============================================================================


def find_browser():
    """The browser to drive: `PILL_WEB_BROWSER`, else the first one installed."""
    configured = os.environ.get("PILL_WEB_BROWSER")
    if configured:
        return configured if Path(configured).is_file() else None
    return next((path for path in BROWSER_CANDIDATES if Path(path).is_file()), None)


def snapshot_bundle() -> dict:
    """The shipping bundle's files and contents, to restore after the build."""
    return {
        path: path.read_bytes() for path in BUNDLE_DIRECTORY.rglob("*") if path.is_file()
    }


def restore_bundle(snapshot: dict) -> None:
    """Put the shipping bundle back as `snapshot` recorded it."""
    for path in BUNDLE_DIRECTORY.rglob("*"):
        if path.is_file() and path not in snapshot:
            path.unlink()
    for path, contents in snapshot.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(contents)


def serve(directory: Path) -> http.server.ThreadingHTTPServer:
    """Serve `directory` on a free local port, quietly, with the wasm MIME type."""

    class QuietHandler(http.server.SimpleHTTPRequestHandler):
        extensions_map = {
            **http.server.SimpleHTTPRequestHandler.extensions_map,
            ".wasm": "application/wasm",
        }

        def log_message(self, *arguments):
            pass

    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(QuietHandler, directory=str(directory))
    )
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def scene_variation(png_bytes: bytes):
    """How much a capture's blurred brightness varies, or None without Pillow."""
    try:
        from PIL import Image, ImageFilter, ImageStat
    except ImportError:
        return None
    image = Image.open(io.BytesIO(png_bytes)).convert("L").filter(ImageFilter.GaussianBlur(6))
    return ImageStat.Stat(image).stddev[0]


def observe_page(browser: str, url: str, seconds: float) -> tuple:
    """Open `url` and collect console text, problems and capture variations."""
    console_lines = []
    problems = []
    variations = []
    with tempfile.TemporaryDirectory() as profile:
        process = subprocess.Popen(
            [
                browser,
                "--headless=new",
                "--enable-unsafe-webgpu",
                f"--user-data-dir={profile}",
                "--no-first-run",
                "--window-size=1280,720",
                "--remote-debugging-port=0",
                "about:blank",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            # The browser writes the port it chose into the profile directory.
            port_file = Path(profile) / "DevToolsActivePort"
            deadline = time.monotonic() + BROWSER_START_TIMEOUT
            while not port_file.is_file() and time.monotonic() < deadline:
                time.sleep(0.2)
            if not port_file.is_file():
                return console_lines, ["the browser did not open its DevTools port"], variations
            time.sleep(0.5)
            port = port_file.read_text().split()[0]
            targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/json").read())
            page = next(target for target in targets if target["type"] == "page")
            devtools = DevToolsSocket(page["webSocketDebuggerUrl"])
            for method in ("Page.enable", "Runtime.enable"):
                devtools.send(method)
            devtools.send("Page.addScriptToEvaluateOnNewDocument", {"source": INJECTED_SCRIPT})
            devtools.send("Page.navigate", {"url": url})

            # Collect events, and once a second ask for a capture of the page.
            devtools.connection.settimeout(0.5)
            end = time.monotonic() + seconds
            next_capture = time.monotonic() + 1
            capture_requests = set()
            while time.monotonic() < end:
                if time.monotonic() >= next_capture:
                    capture_requests.add(devtools.send("Page.captureScreenshot", {"format": "png"}))
                    next_capture += 1
                try:
                    message = devtools.receive()
                except (socket.timeout, TimeoutError):
                    continue
                if message.get("id") in capture_requests and "result" in message:
                    variation = scene_variation(base64.b64decode(message["result"]["data"]))
                    if variation is not None:
                        variations.append(variation)
                method = message.get("method")
                if method == "Runtime.consoleAPICalled":
                    level = message["params"]["type"]
                    text = " ".join(
                        str(argument.get("value", argument.get("description", "")))
                        for argument in message["params"]["args"]
                    )
                    console_lines.append(text)
                    if level == "error":
                        problems.append(f"console error: {text[:300]}")
                elif method == "Runtime.exceptionThrown":
                    details = message["params"]["exceptionDetails"]
                    description = details.get("exception", {}).get("description", details.get("text"))
                    problems.append(f"page exception: {str(description)[:300]}")
        finally:
            process.kill()
            process.wait()
    return console_lines, problems, variations


def run_smoke_test(timeout_scale: float, seconds: float) -> int:
    """Build, serve and open the web build, then check what it did."""
    print("=" * 70)
    print("  Web Build Smoke Test")
    print("=" * 70)

    browser = find_browser()
    if browser is None:
        print("  [FAIL] no Chrome or Edge found; set PILL_WEB_BROWSER")
        return 1

    # Step 1: build, restoring the shipping bundle the build regenerates.
    print(f"\n  [BUILD] build_web.py --dev {PROJECT}")
    snapshot = snapshot_bundle()
    try:
        result = subprocess.run(
            [sys.executable, str(BUILD_SCRIPT), "--dev", PROJECT],
            cwd=str(REPOSITORY_ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=BUILD_TIMEOUT * timeout_scale,
        )
    finally:
        restore_bundle(snapshot)
    if result.returncode != 0:
        print("  [FAIL] the web build failed:")
        print((result.stdout + result.stderr)[-3000:])
        return 1
    print("  [OK] built " + str(WEB_BUILD_DIRECTORY))

    # Step 2: serve it and watch it run.
    print(f"\n  [RUN] {Path(browser).name}, headless with WebGPU, for {seconds:.0f} s")
    server = serve(WEB_BUILD_DIRECTORY)
    try:
        url = f"http://127.0.0.1:{server.server_address[1]}/"
        console_lines, problems, variations = observe_page(browser, url, seconds * timeout_scale)
    finally:
        server.shutdown()

    # Step 3: judge.
    console = "\n".join(console_lines)
    failures = list(problems)
    for token, meaning in (
        (PACK_TOKEN, "the embedded asset pack was not mounted"),
        (FIRST_FRAME_TOKEN, "no first frame was presented with a camera"),
        (FRAME_TOKEN, "frames did not keep running"),
    ):
        if token not in console:
            failures.append(meaning)
    if variations and max(variations) < SCENE_STANDARD_DEVIATION:
        failures.append(
            f"no capture showed a scene (largest variation {max(variations):.1f})"
        )
    if failures:
        print("\n  [FAIL] " + "\n  [FAIL] ".join(failures))
        print("\n  Console tail:\n" + console[-2000:])
        return 1

    print("  [OK] the embedded asset pack was mounted")
    print("  [OK] the first frame was presented with a camera")
    print(f"  [OK] frames kept running ({console.count(FRAME_TOKEN)} statistics report(s))")
    print("  [OK] no engine error, WebGPU error or page exception")
    if variations:
        print(f"  [OK] a lit scene was drawn (largest variation {max(variations):.1f})")
    else:
        print("  [SKIP] picture check: Pillow is not installed")
    print("-" * 70)
    print("  TEST PASSED")
    print("=" * 70)
    return 0


def main() -> None:
    """Parse arguments and run the smoke test."""
    parser = argparse.ArgumentParser(
        prog="test_web_smoke.py",
        description="Build a project for the browser and check it runs and draws.",
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="multiply every timeout, for slower machines (default 1.0)",
    )
    parser.add_argument(
        "--seconds",
        type=float,
        default=15.0,
        help="how long to watch the running page (default 15)",
    )
    arguments = parser.parse_args()
    sys.exit(run_smoke_test(arguments.timeout_scale, arguments.seconds))


if __name__ == "__main__":
    run_suite_with_timing(main)
