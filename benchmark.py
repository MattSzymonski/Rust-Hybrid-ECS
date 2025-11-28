#!/usr/bin/env python3
"""
Benchmark script for ECS stress test
Runs cargo 30 times and computes average frame time
"""

import subprocess
import re
import statistics

def run_benchmark(num_runs=30):
    """Run cargo benchmark multiple times and collect frame times"""
    frame_times = []
    
    print(f"Running benchmark {num_runs} times...")
    print("=" * 60)
    
    for i in range(num_runs):
        print(f"\nRun {i + 1}/{num_runs}...", end=" ", flush=True)
        
        try:
            # Run cargo with release flag
            result = subprocess.run(
                ["cargo", "run", "--release"],
                capture_output=True,
                text=True,
                timeout=120  # 2 minute timeout
            )
            
            # Search for "Avg frame time:" in output
            match = re.search(r'Avg frame time:\s+([\d.]+)\s+ms', result.stdout)
            
            if match:
                frame_time = float(match.group(1))
                frame_times.append(frame_time)
                print(f"✓ {frame_time:.3f} ms")
            else:
                print("✗ Failed to parse frame time")
                
        except subprocess.TimeoutExpired:
            print("✗ Timeout")
        except Exception as e:
            print(f"✗ Error: {e}")
    
    return frame_times

def print_statistics(frame_times):
    """Print statistical summary of frame times"""
    if not frame_times:
        print("\n❌ No valid measurements collected!")
        return
    
    avg = statistics.mean(frame_times)
    med = statistics.median(frame_times)
    min_val = min(frame_times)
    max_val = max(frame_times)
    range_val = max_val - min_val
    
    print("\n" + "=" * 60)
    print("BENCHMARK RESULTS")
    print("=" * 60)
    print(f"Total runs:        {len(frame_times)}")
    print(f"Average:           {avg:.3f} ms")
    print(f"Median:            {med:.3f} ms")
    print(f"Min:               {min_val:.3f} ms")
    print(f"Max:               {max_val:.3f} ms")
    print(f"Range:             {range_val:.3f} ms")
    
    if len(frame_times) > 1:
        std_dev = statistics.stdev(frame_times)
        variance = statistics.variance(frame_times)
        print(f"Std deviation:     {std_dev:.3f} ms")
        print(f"Variance:          {variance:.3f}")
    
    print("=" * 60)
    print(f"\n📊 SUMMARY: {avg:.3f} ms ± {std_dev:.3f} ms (range: {range_val:.3f} ms)")
    print("=" * 60)
    
    # Print all measurements
    print("\nAll measurements (ms):")
    for i, ft in enumerate(frame_times, 1):
        print(f"  Run {i:2d}: {ft:.3f}")

if __name__ == "__main__":
    frame_times = run_benchmark(30)
    print_statistics(frame_times)
