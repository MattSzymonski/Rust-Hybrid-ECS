// DESCRIPTION: The per-measurement environment panels shown below the pickers
//   (one for the run, one for the baseline) plus the shared collapsible-panel
//   helper. The panels start expanded so the two runs' settings can be
//   compared at a glance; a link icon between them flags whether the settings
//   (excluding git) match, and every individual row whose value differs
//   between the two runs is highlighted red in both panels.
//
// --- SCRIPT ---

import { element } from '../lib/dom';
import { formatTimestamp } from '../lib/format';
import type { EnvironmentMetadata, Measurement } from '../types/measurement';

/** Display order and labels for the environment fields worth showing. */
const ENVIRONMENT_FIELDS: [keyof EnvironmentMetadata, string][] = [
  ['os', 'OS'],
  ['os_version', 'OS Version'],
  ['architecture', 'Architecture'],
  ['hostname', 'Hostname'],
  ['cpu', 'CPU'],
  ['physical_cores', 'Physical Cores'],
  ['logical_cpus', 'Logical Processors'],
  ['cpu_max_mhz', 'Max Clock (MHz)'],
  ['l2_cache_kb', 'L2 Cache (KB)'],
  ['l3_cache_kb', 'L3 Cache (KB)'],
  ['l2_cache', 'L2 Cache'],
  ['l3_cache', 'L3 Cache'],
  ['ram_gb', 'RAM (GB)'],
  ['host_triple', 'Host Triple'],
  ['rustc', 'rustc'],
  ['rustc_commit', 'rustc Commit'],
  ['cargo', 'cargo'],
  ['active_toolchain', 'Toolchain'],
  ['dotnet', '.NET SDK'],
  ['python', 'Python'],
];

/** Creates a collapsible panel with a header that toggles its body. */
export function collapsiblePanel(
  title: string,
  hint: string,
  body: HTMLElement,
  startCollapsed = true,
): HTMLElement {
  const panel = element('div', { class: `info-panel${startCollapsed ? ' collapsed' : ''}` });
  const header = element(
    'div',
    { class: 'info-panel-header' },
    element('span', { class: 'collapse-icon', text: '▾' }),
    element('span', { text: title }),
    hint ? element('span', { class: 'panel-hint', text: hint }) : null,
  );
  header.addEventListener('click', () => panel.classList.toggle('collapsed'));
  panel.appendChild(header);
  panel.appendChild(element('div', { class: 'info-panel-body' }, body));
  return panel;
}

/** Adds one key/value row to a grid, flagged red when it differs between runs. */
function appendRow(
  grid: HTMLElement,
  key: string,
  label: string,
  value: string,
  diffKeys: Set<string>,
): void {
  const differs = diffKeys.has(key);
  grid.appendChild(
    element(
      'div',
      { class: `kv-row${differs ? ' differs' : ''}` },
      element('span', { class: 'kv-key', text: label }),
      element('span', { class: 'kv-value', text: value }),
    ),
  );
}

/**
 * Renders one measurement's environment as a panel titled "environment".
 * `role` tells the reader which of the compared runs this is (run vs
 * baseline). Git context (commit / branch / tree) lives here too - it is how
 * the exact measured code is pinned down. Rows whose key is in `diffKeys`
 * differ from the compared run and are highlighted.
 */
export function renderEnvironmentInfo(
  measurement: Measurement,
  role: 'run' | 'baseline',
  diffKeys: Set<string> = new Set(),
): HTMLElement {
  const body = element('div');

  // Git context: which exact code this measurement ran against. The commit
  // hash is the strongest fingerprint, so it comes first.
  const git = measurement.git;
  if (git.available) {
    const gitGrid = element('div', { class: 'kv-grid' });
    appendRow(gitGrid, 'commit', 'Commit', git.commit_short ?? git.commit ?? '', diffKeys);
    appendRow(gitGrid, 'branch', 'Branch', git.branch ?? '', diffKeys);
    appendRow(
      gitGrid,
      'tree',
      'Tree',
      git.dirty ? `dirty (${git.dirty_file_count ?? 0} files)` : 'clean',
      diffKeys,
    );
    if (git.subject) appendRow(gitGrid, 'subject', 'Subject', `"${git.subject}"`, diffKeys);
    body.appendChild(gitGrid);
  }

  const grid = element('div', { class: 'kv-grid' });
  for (const [key, label] of ENVIRONMENT_FIELDS) {
    const value = measurement.environment[key];
    if (value === undefined || value === null || value === '') continue;
    appendRow(grid, `env:${key}`, label, String(value), diffKeys);
  }
  body.appendChild(grid);

  // The exact command is what makes a measurement reproducible, so it is
  // shown verbatim rather than summarized.
  const command = measurement.command;
  if (command?.argv?.length) {
    const commandGrid = element('div', { class: 'kv-grid' });
    appendRow(commandGrid, 'command', 'Command', command.argv.join(' '), diffKeys);
    if (command.cwd) appendRow(commandGrid, 'cwd', 'Working dir', command.cwd, diffKeys);
    if (command.rustflags) appendRow(commandGrid, 'rustflags', 'RUSTFLAGS', command.rustflags, diffKeys);
    if (command.duration_seconds !== undefined) {
      appendRow(
        commandGrid,
        'duration',
        'Command duration',
        `${command.duration_seconds.toFixed(1)} s`,
        diffKeys,
      );
    }
    if (command.driver) appendRow(commandGrid, 'driver', 'Driver', command.driver, diffKeys);
    body.appendChild(commandGrid);
  }

  if (measurement.notes?.length) {
    const list = element('ul', { class: 'note-list' });
    for (const note of measurement.notes) {
      list.appendChild(element('li', { text: note }));
    }
    body.appendChild(list);
  }

  const hint = [
    role === 'baseline' ? 'baseline' : 'run',
    formatTimestamp(measurement.timestamp),
    measurement.environment.rustc?.split(' ').slice(0, 2).join(' '),
    measurement.tool ? `pill_lab ${measurement.tool.version}` : '',
    `schema v${measurement.schema_version}`,
  ]
    .filter(Boolean)
    .join(' · ');

  // Folded by default so the panels do not crowd the setup area; one click on
  // the header expands them for a side-by-side comparison.
  return collapsiblePanel('environment', hint, body, true);
}

/** Collects every visible environment row as a stable key + display value. */
function environmentEntries(measurement: Measurement): { key: string; value: string }[] {
  const entries: { key: string; value: string }[] = [];
  const git = measurement.git;
  if (git.available) {
    entries.push({ key: 'commit', value: git.commit_short ?? git.commit ?? '' });
    entries.push({ key: 'branch', value: git.branch ?? '' });
    entries.push({
      key: 'tree',
      value: git.dirty ? `dirty (${git.dirty_file_count ?? 0} files)` : 'clean',
    });
    if (git.subject) entries.push({ key: 'subject', value: `"${git.subject}"` });
  }
  for (const [key] of ENVIRONMENT_FIELDS) {
    const value = measurement.environment[key];
    if (value === undefined || value === null || value === '') continue;
    entries.push({ key: `env:${key}`, value: String(value) });
  }
  const command = measurement.command;
  if (command?.argv?.length) {
    entries.push({ key: 'command', value: command.argv.join(' ') });
    if (command.cwd) entries.push({ key: 'cwd', value: command.cwd });
    if (command.rustflags) entries.push({ key: 'rustflags', value: command.rustflags });
    if (command.duration_seconds !== undefined) {
      entries.push({ key: 'duration', value: `${command.duration_seconds.toFixed(1)} s` });
    }
    if (command.driver) entries.push({ key: 'driver', value: command.driver });
  }
  return entries;
}

/** Keys whose display value differs between the two runs (a missing side counts as ''). */
function differingKeys(a: Measurement, b: Measurement): Set<string> {
  const aValues = new Map(environmentEntries(a).map((entry) => [entry.key, entry.value]));
  const bValues = new Map(environmentEntries(b).map((entry) => [entry.key, entry.value]));
  const keys = new Set([...aValues.keys(), ...bValues.keys()]);
  const differ = new Set<string>();
  for (const key of keys) {
    if ((aValues.get(key) ?? '') !== (bValues.get(key) ?? '')) differ.add(key);
  }
  return differ;
}

/**
 * Compares the environment settings + command of two measurements, ignoring
 * git (a different commit is fine - the point is whether the machine and
 * toolchain the runs were made under match). Returns true when identical.
 */
function environmentsMatch(a: Measurement, b: Measurement): boolean {
  return (
    canonical(a.environment) === canonical(b.environment) &&
    canonical(a.command) === canonical(b.command)
  );
}

/** Sorts object keys so JSON.stringify is insensitive to field order. */
function canonical(value: unknown): string {
  return JSON.stringify(value, (_key, val) =>
    val && typeof val === 'object' && !Array.isArray(val)
      ? Object.fromEntries(Object.entries(val).sort(([x], [y]) => x.localeCompare(y)))
      : val,
  );
}

/** Link / broken-link SVG used between the two environment panels. */
function linkIcon(same: boolean): string {
  const paths = same
    ? '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>'
    : '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/><line x1="21" y1="21" x2="3" y2="3"/>';
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" ` +
    `fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`
  );
}

/**
 * Renders the environment panels for the run and (when present) its baseline,
 * side by side. A link icon between them shows whether the two runs were made
 * under identical settings: solid link when they match, broken link when they
 * differ. The git commit is not part of that icon's comparison, but rows that
 * differ (including git) are highlighted red inside both panels.
 */
export function renderEnvironments(
  run: Measurement,
  baseline: Measurement | null,
): HTMLElement {
  const diffKeys = baseline ? differingKeys(run, baseline) : new Set<string>();
  const grid = element('div', { class: 'environments-grid' });
  grid.appendChild(renderEnvironmentInfo(run, 'run', diffKeys));
  if (baseline) {
    const same = environmentsMatch(run, baseline);
    grid.appendChild(
      element('div', {
        class: `environment-link ${same ? 'same' : 'different'}`,
        title: same
          ? 'Same environment settings (excluding commit)'
          : 'Different environment settings (excluding commit)',
        html: linkIcon(same),
      }),
    );
    grid.appendChild(renderEnvironmentInfo(baseline, 'baseline', diffKeys));
  }
  return grid;
}
