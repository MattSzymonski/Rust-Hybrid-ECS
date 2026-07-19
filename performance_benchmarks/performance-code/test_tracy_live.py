#!/usr/bin/env python3
"""Run tracy_live example for N frames and report average frame time."""
import subprocess, sys, re, os

FRAMES = int(sys.argv[1]) if len(sys.argv) > 1 else 20000
EXE = os.path.join(os.path.dirname(__file__), "..", "..", "target", "release", "examples", "tracy_live.exe")

# Build if needed
subprocess.run(["cargo", "build", "--example", "tracy_live", "--release", "--features", "profiling"],
               cwd=os.path.join(os.path.dirname(__file__), "..", ".."),
               check=True, timeout=300)

# Run and capture FPS
env = os.environ.copy()
env["ECS_SLICE_SIZE"] = os.environ.get("ECS_SLICE_SIZE", "")

proc = subprocess.Popen(
    [EXE], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    text=True, env=env, cwd=os.path.join(os.path.dirname(__file__), "..", ".."),
)

fps_values = []
total_frames = 0
started = False

try:
    for line in proc.stdout:
        # Skip startup banner
        if "Connect Tracy" in line:
            started = True
            continue
        if not started:
            continue
        # Parse "  2942 FPS | 30000 entities"
        m = re.match(r'\s*(\d+)\s+FPS', line)
        if m:
            fps = int(m.group(1))
            fps_values.append(fps)
            # Each report covers ~2 seconds, so ~fps*2 frames
            total_frames += fps * 2
            print(f"  {fps} FPS  (total ~{total_frames} frames)", flush=True)
            if total_frames >= FRAMES:
                proc.terminate()
                break
except KeyboardInterrupt:
    proc.terminate()

proc.wait(timeout=10)

if not fps_values:
    print("ERROR: No FPS data captured", file=sys.stderr)
    sys.exit(1)

avg_fps = sum(fps_values) / len(fps_values)
avg_frame_time_us = (1_000_000 / avg_fps) if avg_fps > 0 else 0

print(f"\n{'='*50}")
print(f"Frames sampled: ~{total_frames}")
print(f"FPS reports: {len(fps_values)}")
print(f"Average FPS: {avg_fps:.1f}")
print(f"Average frame time: {avg_frame_time_us:.1f} us")
print(f"FPS range: {min(fps_values)} - {max(fps_values)}")
print(f"{'='*50}")
