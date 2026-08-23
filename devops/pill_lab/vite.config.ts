// REQUIREMENTS: Node.js 18+, Vite 6.
//
// DESCRIPTION: Vite configuration for the Pill Lab frontend. The only
//   non-default piece is the `measurements` plugin, which exposes
//   `devops/pill_lab/measurements/` at the `/measurements/` URL prefix in both
//   `vite dev` and `vite build`. Keeping the same URL in both modes means the
//   loader never branches on the environment, and measurement files stay where
//   `pill_lab.py` writes them instead of being copied into `public/`.
//
// USAGE: driven by `npm run dev` / `npm run build`, or through
//   `python devops/pill_lab/pill_lab.py serve`.
//
// --- SCRIPT ---

import { createWriteStream, mkdirSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, relative, resolve, sep } from 'node:path';
import { spawn } from 'node:child_process';
import { defineConfig, type Plugin } from 'vite';

const measurementsDirectory = resolve(__dirname, 'measurements');
const urlPrefix = '/measurements/';
// The functional test suites live next to this project, under `devops/tests/`.
const testsDirectory = resolve(__dirname, '../tests');
// Spawned test scripts run from the repository root so their path resolution
// (workspace manifests, example projects, optional modules) matches a normal
// console invocation.
const repositoryRoot = resolve(__dirname, '../..');
// The Pill Lab CLI that runs the benchmark categories.
const pillLabScript = resolve(__dirname, 'pill_lab.py');

// Collects every JSON file under the measurements tree as posix-style paths
// relative to the measurements root, so both middleware and build emit agree
// on the URL shape.
function collectMeasurementFiles(directory: string): string[] {
  const collected: string[] = [];
  const walk = (current: string): void => {
    let entries: string[];
    try {
      entries = readdirSync(current);
    } catch {
      return;
    }
    for (const entry of entries) {
      const absolute = join(current, entry);
      if (statSync(absolute).isDirectory()) {
        walk(absolute);
      } else if (entry.endsWith('.json')) {
        collected.push(relative(measurementsDirectory, absolute).split(sep).join('/'));
      }
    }
  };
  walk(directory);
  return collected;
}

// Serves measurement JSON in dev and emits it into the bundle for builds.
// Dev responses are explicitly uncacheable so a measurement written while the
// dev server runs shows up on the next reload.
function measurementsPlugin(): Plugin {
  return {
    name: 'pill-lab-measurements',
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const url = (request.url ?? '').split('?')[0];
        if (!url.startsWith(urlPrefix)) {
          next();
          return;
        }
        const relativePath = decodeURIComponent(url.slice(urlPrefix.length));
        // Reject traversal before touching the filesystem.
        const absolute = resolve(measurementsDirectory, relativePath);
        if (!absolute.startsWith(measurementsDirectory)) {
          response.statusCode = 403;
          response.end('Forbidden');
          return;
        }
        try {
          const content = readFileSync(absolute);
          response.setHeader('Content-Type', 'application/json; charset=utf-8');
          response.setHeader('Cache-Control', 'no-store');
          response.end(content);
        } catch {
          response.statusCode = 404;
          response.setHeader('Content-Type', 'application/json; charset=utf-8');
          response.end(JSON.stringify({ error: 'measurement not found', path: relativePath }));
        }
      });
    },
    generateBundle() {
      for (const file of collectMeasurementFiles(measurementsDirectory)) {
        this.emitFile({
          type: 'asset',
          fileName: `measurements/${file}`,
          source: readFileSync(join(measurementsDirectory, file)),
        });
      }
    },
  };
}

// =============================================================================
// Runs API (dev server only)
//
// Both the Tests tab and the benchmark Run buttons start a Python process.
// Python cannot run in the browser, so the dev server spawns it and streams
// the output back over Server-Sent Events. In addition the run is *also* made
// visible in a real console:
//
//   * on Windows a console window opens that tails the run's log file live
//     (`Get-Content -Wait`), so it looks exactly like launching the command
//     from a terminal - output included, no browser required;
//   * elsewhere the lines are echoed to the dev server's own stdout instead
//     (there is no portable way to pop a second console).
//
// Endpoints:
//
//   GET /api/tests             JSON list of the suites under devops/tests/.
//   GET /api/tests/run?name=   run one suite (SSE stream).
//   GET /api/benchmarks        JSON list of the measurement categories.
//   GET /api/benchmarks/run?category=   run one category via pill_lab.py.
//
// The static `build` output has no backend, so these 404 there and the UI
// shows the "start the dev server" hint.
// =============================================================================

interface TestInfo {
  name: string;
  title: string;
  description: string;
}

interface BenchmarkInfo {
  category: string;
  label: string;
  /** The `pill_lab.py` subcommand, e.g. `hot-reload` (dash-named). */
  subcommand: string;
}

// The three measurement categories `pill_lab.py` accepts as subcommands. The
// `category` id (underscore-named, matching the compare identifiers) is what
// the UI and the API URL use; `subcommand` is the actual CLI verb.
const BENCHMARKS: BenchmarkInfo[] = [
  { category: 'engine', label: 'Engine', subcommand: 'engine' },
  { category: 'hot_reload', label: 'Hot Reload', subcommand: 'hot-reload' },
  { category: 'cold_start', label: 'Cold Start', subcommand: 'cold-start' },
];

/** Extracts the one-line summary from a test script's module docstring. */
function docstringSummary(filePath: string): { title: string; description: string } {
  try {
    const content = readFileSync(filePath, 'utf-8');
    const opener = content.indexOf('"""');
    if (opener < 0) return { title: '', description: '' };
    const closer = content.indexOf('"""', opener + 3);
    if (closer < 0) return { title: '', description: '' };
    const lines = content
      .slice(opener + 3, closer)
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
    const title = lines[0] ?? '';
    // The remaining prose is the description; skip the REQUIREMENTS / USAGE /
    // DESCRIPTION label lines so the card reads like prose.
    const description = lines
      .slice(1)
      .filter((line) => !/^(REQUIREMENTS|DESCRIPTION|USAGE|EXAMPLE USAGE|SCRIPT|---)/i.test(line))
      .slice(0, 3)
      .join(' ');
    return { title, description };
  } catch {
    return { title: '', description: '' };
  }
}

/** Lists every `test_*.py` suite in `devops/tests/`, in file-name order. */
function collectTests(): TestInfo[] {
  const tests: TestInfo[] = [];
  let entries: string[];
  try {
    entries = readdirSync(testsDirectory);
  } catch {
    return tests;
  }
  for (const entry of entries.sort()) {
    if (!entry.startsWith('test_') || !entry.endsWith('.py')) continue;
    const { title, description } = docstringSummary(join(testsDirectory, entry));
    tests.push({ name: entry, title, description });
  }
  return tests;
}

/**
 * Opens a visible console window that tails `logPath` live, so a run started
 * from the UI looks exactly like a console invocation. Non-Windows platforms
 * have no portable way to pop a second console, so they fall back to echoing
 * the stream to the dev server's own stdout instead (see `streamRun`).
 */
function openConsoleWindow(title: string, logPath: string): void {
  if (process.platform !== 'win32') return;
  // `start` opens a new console window whose only job is to follow the log
  // file. `-NoExit` keeps the window open after the run so the final output
  // stays readable; the user closes it when done.
  const tailCommand = `Get-Content -LiteralPath '${logPath}' -Wait -Tail 300`;
  const commandLine =
    `start "${title}" powershell.exe -NoExit -NoProfile -ExecutionPolicy Bypass ` +
    `-Command "${tailCommand}"`;
  try {
    // `windowsVerbatimArguments` is essential: without it Node re-quotes the
    // nested `"` for cmd.exe and the `start` window never opens.
    spawn('cmd.exe', ['/d', '/s', '/c', commandLine], {
      windowsVerbatimArguments: true,
      windowsHide: false,
      stdio: 'ignore',
      detached: true,
    }).unref();
  } catch {
    // The run must survive a failed attempt to open the window.
  }
}

/**
 * Runs one Python command and fans its output out to every consumer at once:
 * the browser (SSE `line` events), a log file (tailed by a console window on
 * Windows, echoed to the dev server's stdout elsewhere), and finally a `done`
 * event with the exit code. The child is killed if the browser disconnects.
 */
function streamRun(
  request: { label: string; args: string[]; cwd: string },
  response: import('node:http').ServerResponse,
): void {
  response.writeHead(200, {
    'Content-Type': 'text/event-stream; charset=utf-8',
    'Cache-Control': 'no-store',
    Connection: 'keep-alive',
  });
  response.write(`event: meta\ndata: ${JSON.stringify({ name: request.label })}\n\n`);

  // Every run gets its own log file under the OS temp dir; the console window
  // tails it. The log stream stays open until the child exits.
  const logDirectory = join(tmpdir(), 'pill_lab');
  mkdirSync(logDirectory, { recursive: true });
  const safeLabel = request.label.replace(/[^a-z0-9_.-]+/gi, '_');
  const logPath = join(logDirectory, `${safeLabel}_${Date.now()}.log`);
  const logStream = createWriteStream(logPath, { flags: 'a' });
  const echoToConsole = process.platform !== 'win32';

  const child = spawn(process.env.PYTHON || 'python', request.args, { cwd: request.cwd });
  let closed = false;
  let startError: string | undefined;

  const send = (event: string, data: unknown): void => {
    if (!closed) response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  };
  // Lines are forwarded verbatim: the frontend strips ANSI for the log but
  // needs the raw bytes to recognise bold section headers as steps.
  const forward = (stream: 'stdout' | 'stderr') => (chunk: Buffer): void => {
    for (const line of chunk.toString().split(/\r?\n/)) {
      if (line.length === 0) continue;
      logStream.write(`${line}\n`);
      if (echoToConsole) process.stdout.write(`${line}\n`);
      send('line', { text: line, stream });
    }
  };
  child.stdout.on('data', forward('stdout'));
  child.stderr.on('data', forward('stderr'));
  child.on('close', (code) => {
    logStream.end(`--- ${request.label} finished with exit code ${code} ---\n`);
    if (echoToConsole) process.stdout.write(`--- ${request.label} finished (exit ${code}) ---\n`);
    send('done', { exit_code: code, error: startError });
    response.end();
    closed = true;
  });
  child.on('error', (error) => {
    startError = String(error);
    logStream.end(`--- could not start ${request.label}: ${startError} ---\n`);
    if (echoToConsole) process.stdout.write(`--- could not start: ${startError} ---\n`);
    send('done', { exit_code: -1, error: startError });
    response.end();
    closed = true;
  });

  openConsoleWindow(`Pill Lab: ${request.label}`, logPath);

  // The browser navigating away or closing the tab should not leave a run
  // hanging in the background.
  response.on('close', () => {
    if (!closed) {
      child.kill();
      logStream.end(`--- ${request.label} stopped (browser disconnected) ---\n`);
      closed = true;
    }
  });
}

function runsPlugin(): Plugin {
  return {
    name: 'pill-lab-runs',
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const url = (request.url ?? '').split('?')[0];
        const query = new URL(request.url ?? 'http://localhost', 'http://localhost')
          .searchParams;

        if (url === '/api/tests') {
          response.setHeader('Content-Type', 'application/json; charset=utf-8');
          response.setHeader('Cache-Control', 'no-store');
          response.end(JSON.stringify(collectTests()));
          return;
        }

        if (url === '/api/tests/run') {
          const name = query.get('name') ?? '';
          // Only accept a plain basename from the tests directory - never a
          // path, so a request cannot escape the directory.
          if (!name.endsWith('.py') || basename(name) !== name) {
            response.statusCode = 403;
            response.end('Forbidden');
            return;
          }
          const filePath = resolve(testsDirectory, name);
          if (!filePath.startsWith(testsDirectory)) {
            response.statusCode = 403;
            response.end('Forbidden');
            return;
          }
          streamRun({ label: name, args: [filePath], cwd: repositoryRoot }, response);
          return;
        }

        if (url === '/api/benchmarks') {
          response.setHeader('Content-Type', 'application/json; charset=utf-8');
          response.setHeader('Cache-Control', 'no-store');
          response.end(JSON.stringify(BENCHMARKS));
          return;
        }

        if (url === '/api/benchmarks/run') {
          const category = query.get('category') ?? '';
          const benchmark = BENCHMARKS.find((candidate) => candidate.category === category);
          if (!benchmark) {
            response.statusCode = 400;
            response.end('Unknown benchmark category');
            return;
          }
          streamRun(
            {
              label: benchmark.label,
              // `pill_lab.py <subcommand>` runs with the benchmark's defaults,
              // exactly as from a console (cold-start defaults to the
              // non-interactive `packages` clean scope).
              args: [pillLabScript, benchmark.subcommand],
              cwd: repositoryRoot,
            },
            response,
          );
          return;
        }

        next();
      });
    },
  };
}

export default defineConfig({
  // Relative base so a built `dist/` also works when opened from a
  // subdirectory or served by any static file server.
  base: './',
  plugins: [measurementsPlugin(), runsPlugin()],
  server: {
    port: 5180,
    strictPort: false,
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
