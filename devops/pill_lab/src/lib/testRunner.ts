// DESCRIPTION: Tests tab data plumbing - the list endpoint plus the live run
//   stream. Runs come back as Server-Sent Events (`meta`, then one `line`
//   event per output line, then `done` with the exit code). This module owns
//   turning raw lines into the step list the UI spins, so the view stays a
//   dumb renderer:
//
//     * `[TEST]` / `[PREP]` / `[CLEANUP]` lines start a running step;
//     * `[OK]` / `[FAIL]` lines complete it (matched by label, else the most
//       recent running step - the tagged suites print both sides);
//     * ANSI-bold section headers start a step for the untagged suites
//       (test_basic, test_coding_standards, test_examples);
//     * everything else is appended to the bounded log.
//
// --- SCRIPT ---

import type { TestInfo, TestRunState, TestStep, TestStepState } from '../types/test';

const TESTS_BASE = 'api/tests';

export class TestsApiError extends Error {}

const MAX_LOG_LINES = 800;

/** Fetches the suite list. Only the dev server provides `/api/tests`. */
export async function listTests(): Promise<TestInfo[]> {
  const response = await fetch(`${TESTS_BASE}?t=${Date.now()}`);
  if (!response.ok) {
    throw new TestsApiError(
      `Could not list tests (HTTP ${response.status}). Tests run through the ` +
        `Pill Lab dev server - start it with "python devops/pill_lab/pill_lab.py serve".`,
    );
  }
  return (await response.json()) as TestInfo[];
}

/** Removes ANSI SGR sequences so the log and step labels stay readable. */
function stripAnsi(text: string): string {
  return text.replace(/\u001b\[[0-9;]*m/g, '');
}

export interface TestRunnerEvents {
  /** Called whenever the run state (steps, logs, phase) changes. */
  onState(state: TestRunState): void;
}

/**
 * Starts one test and streams its output through `events.onState`.
 *
 * Returns a stop function that closes the stream (the server also kills the
 * child when the connection drops). Only one run may be active at a time - the
 * view enforces that, not this module.
 */
export function startTest(name: string, events: TestRunnerEvents): () => void {
  const steps: TestStep[] = [];
  const logs: string[] = [];
  let phase: TestRunState['phase'] = 'running';
  let exitCode: number | null = null;
  let error: string | undefined;
  let finished = false;
  let nextStepId = 1;

  const pushState = (): void => {
    events.onState({
      phase,
      steps: steps.map((step) => ({ ...step })),
      logs: logs.slice(),
      exitCode,
      error,
    });
  };

  /** Marks the most recent running step complete (the next step supersedes it). */
  const closeRunning = (state: TestStepState): void => {
    for (let index = steps.length - 1; index >= 0; index--) {
      if (steps[index].state === 'running') {
        steps[index].state = state;
        break;
      }
    }
  };

  /** Starts a new running step, implicitly closing the previous one. */
  const addStep = (label: string): void => {
    closeRunning('ok');
    steps.push({ id: nextStepId, label, state: 'running' });
    nextStepId += 1;
    pushState();
  };

  /** Completes the step that matches `label`, else the most recent running one. */
  const finishStep = (label: string | undefined, state: TestStepState): void => {
    if (label) {
      const match = [...steps]
        .reverse()
        .find((step) => step.state === 'running' && step.label === label)
        ?? [...steps].reverse().find((step) => step.state === 'running');
      if (match) {
        match.state = state;
        pushState();
        return;
      }
    }
    closeRunning(state);
    pushState();
  };

  const tagPattern = /^\[([A-Z]+)\]\s*(.*)$/;
  // Untagged suites mark their sections either with ANSI bold (colorize/
  // section helpers), a `(N/M) Title` counter, or a short title line sitting
  // directly under a `=====`/`-----` separator (core.cli's banner).
  const boldHeader = /\u001b\[1m/;
  const separator = /^[=\-]{10,}$/;
  const numberedHeader = /^\(\d+\/\d+\)\s/;
  let lastWasSeparator = false;

  const handleLine = (rawLine: string): void => {
    const line = stripAnsi(rawLine);
    const trimmed = line.trim();
    if (trimmed.length === 0) return;

    const tagMatch = trimmed.match(tagPattern);
    if (tagMatch) {
      lastWasSeparator = false;
      const tag = tagMatch[1];
      const rest = tagMatch[2].trim();
      switch (tag) {
        case 'TEST':
        case 'PREP':
        case 'CLEANUP':
          addStep(rest || tag);
          break;
        case 'OK':
          finishStep(rest || undefined, 'ok');
          break;
        case 'FAIL':
          finishStep(rest || undefined, 'failed');
          break;
        case 'PASS':
          finishStep(rest || undefined, 'ok');
          break;
        default:
          // WARN / SKIP / std / ... are informative, not step boundaries.
          break;
      }
    } else if (separator.test(trimmed)) {
      lastWasSeparator = true;
    } else if (
      boldHeader.test(rawLine) ||
      numberedHeader.test(trimmed) ||
      (lastWasSeparator && trimmed.length < 80)
    ) {
      lastWasSeparator = false;
      addStep(trimmed);
    } else {
      lastWasSeparator = false;
    }

    logs.push(line);
    if (logs.length > MAX_LOG_LINES) logs.splice(0, logs.length - MAX_LOG_LINES);
    pushState();
  };

  const source = new EventSource(`${TESTS_BASE}/run?name=${encodeURIComponent(name)}`);

  source.addEventListener('line', (event) => {
    try {
      const payload = JSON.parse((event as MessageEvent).data) as { text?: string };
      handleLine(String(payload.text ?? ''));
    } catch {
      // A malformed event should not tear the whole run down.
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
    exitCode = code;
    error = message;
    phase = message ? 'error' : code === 0 ? 'passed' : 'failed';
    closeRunning(phase === 'passed' ? 'ok' : 'failed');
    finished = true;
    source.close();
    pushState();
  });

  source.onerror = () => {
    if (finished || source.readyState !== EventSource.CLOSED) return;
    phase = 'error';
    error =
      'The test stream ended before reporting a result. Tests run through the ' +
      'Pill Lab dev server - start it with "python devops/pill_lab/pill_lab.py serve".';
    closeRunning('failed');
    finished = true;
    pushState();
  };

  pushState();
  return () => {
    finished = true;
    source.close();
  };
}
