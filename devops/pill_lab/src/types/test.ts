// DESCRIPTION: Types for the Tests tab. A test is one Python suite under
//   `devops/tests/`; a run is a live stream of steps (spinner rows) plus a
//   bounded log. Step states map straight onto the spinner CSS: running spins,
//   ok shows a green check, failed a red cross, pending a dim circle.
//
// --- SCRIPT ---

export interface TestInfo {
  /** File name, e.g. `test_hot_reload_suite.py` - the run API key. */
  name: string;
  /** One-line summary from the suite's module docstring. */
  title: string;
  /** Short prose describing what the suite covers. */
  description: string;
}

export type TestStepState = 'running' | 'ok' | 'failed' | 'pending';

export interface TestStep {
  id: number;
  label: string;
  state: TestStepState;
}

export type TestRunPhase = 'idle' | 'running' | 'passed' | 'failed' | 'error';

/** Benchmark runs share the same lifecycle as test runs. */
export type BenchmarkRunPhase = TestRunPhase;

export interface TestRunState {
  phase: TestRunPhase;
  steps: TestStep[];
  logs: string[];
  exitCode: number | null;
  error?: string;
}
