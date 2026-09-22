"""Exercise the native cooker watcher: modify, remove, fail, preserve manifest."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("cooker", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    fixture = root / "examples/project_rs/assets/render_source"
    scratch = root / "local/renderer-validation" / f"watch-{time.time_ns()}"
    source = scratch / "source"
    output = scratch / "output"
    source.mkdir(parents=True)
    image = source / "image.png"
    image.write_bytes((fixture / "checker.png").read_bytes())
    log_path = scratch / "watch.log"
    def manifest():
        try:
            return json.loads((output / "manifest.json").read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            return None
    def until(predicate):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"watcher exited {process.returncode}: {log_path.read_text()}")
            value = predicate()
            if value:
                return value
            time.sleep(0.1)
        raise TimeoutError(f"watcher timed out: {log_path}")
    with log_path.open("w") as log:
        process = subprocess.Popen([str(args.cooker.resolve()), str(source), str(output), "--watch"],
            stdout=log, stderr=subprocess.STDOUT, creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0))
        try:
            first = until(manifest)
            old_id = first["assets"][0]["id"]
            image.write_bytes((fixture / "normal.png").read_bytes())
            changed = until(lambda: (m if (m := manifest()) and m["generation"] != first["generation"] else None))
            assert changed["assets"][0]["id"] == old_id
            image.write_bytes(b"invalid PNG")
            until(lambda: "[assets]" in log_path.read_text())
            assert manifest() == changed, "failed cook replaced the live manifest"
            image.unlink()
            empty = until(lambda: (m if (m := manifest()) and m["generation"] != changed["generation"] else None))
            assert empty["assets"] == []
            print("PASS: watcher starts without host DLLs; edit, stable ID, atomic failure, deletion")
        finally:
            process.terminate()
            process.wait(timeout=10)


if __name__ == "__main__":
    main()
