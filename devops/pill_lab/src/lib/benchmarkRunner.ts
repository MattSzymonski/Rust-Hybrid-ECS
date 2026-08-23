// DESCRIPTION: Benchmark Run plumbing for the Benchmarks tab. The dev server
//   runs `pill_lab.py <category>` on demand (with a visible console window on
//   Windows) and streams Server-Sent Events back. The run lifecycle (running /
//   passed / failed + exit code) drives the Run button and state chips, and
//   the raw lines feed the terminal shown in the main panel.
//
// --- SCRIPT ---

import type { BenchmarkRunPhase } from '../types/test';

export interface BenchmarkInfo {
  /** Underscore-named id used by the UI and the run API, e.g. `hot_reload`. */
  category: string;
  label: string;
}

export interface BenchmarkRunState {
  phase: BenchmarkRunPhase;
  exitCode: number | null;
  error?: string;
  /** Raw output lines for the terminal, most recent last. */
  logs: string[];
}

const BENCHMARKS_BASE = 'api/benchmarks';
const MAX_LOG_LINES = 800;

export class BenchmarksApiError extends Error {}

/** Fetches the measurement categories the UI can start. */
export async function listBenchmarks(): Promise<BenchmarkInfo[]> {
  const response = await fetch(`${BENCHMARKS_BASE}?t=${Date.now()}`);
  if (!response.ok) {
    throw new BenchmarksApiError(
      `Could not list benchmarks (HTTP ${response.status}). Benchmarks run ` +
        `through the Pill Lab dev server - start it with "python ` +
        `devops/pill_lab/pill_lab.py serve".`,
    );
  }
  return (await response.json()) as BenchmarkInfo[];
}

export interface BenchmarkRunnerEvents {
  onState(state: BenchmarkRunState): void;
}

/**
 * Starts one benchmark category and reports lifecycle changes plus the output
 * lines for the terminal. Returns a stop function.
 */
export function startBenchmark(category: string, events: BenchmarkRunnerEvents): () => void {
  const logs: string[] = [];
  const state: BenchmarkRunState = { phase: 'running', exitCode: null, logs: [] };
  let finished = false;

  const push = (): void => {
    events.onState({ ...state, logs: logs.slice() });
  };

  const source = new EventSource(
    `${BENCHMARKS_BASE}/run?category=${encodeURIComponent(category)}`,
  );

  source.addEventListener('line', (event) => {
    try {
      const payload = JSON.parse((event as MessageEvent).data) as { text?: string };
      const text = String(payload.text ?? '');
      logs.push(text);
      if (logs.length > MAX_LOG_LINES) logs.splice(0, logs.length - MAX_LOG_LINES);
    } catch {
      // A malformed line should not tear the run down.
    }
  });

  source.addEventListener('done', (event) => {
    let code = -1;
    let message: string | undefined;
    try {
      const payload = JSON.parse((event as MessageEvent).data) as {
        exit_code?: number;
        error?: string;
      };
      code = payload.exit_code ?? -1;
      message = payload.error;
    } catch {
      // Fall through with the defaults.
    }
    state.exitCode = code;
    state.error = message;
    state.phase = message ? 'error' : code === 0 ? 'passed' : 'failed';
    finished = true;
    source.close();
    push();
  });

  source.onerror = () => {
    if (finished || source.readyState !== EventSource.CLOSED) return;
    state.phase = 'error';
    state.error =
      'The run stream ended before reporting a result. Benchmarks run through ' +
      'the Pill Lab dev server - start it with "python devops/pill_lab/pill_lab.py serve".';
    finished = true;
    push();
  };

  push();
  return () => {
    finished = true;
    source.close();
  };
}
