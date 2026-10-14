// DESCRIPTION: The Editor Shell view (W-0004 Stage 23). The four D32
//   targets render as tiles with a met/missed state, and every measured case
//   renders as a card: its value in the case's own unit, the run-to-run
//   spread where samples exist, and a baseline delta when one is selected.
//
// --- SCRIPT ---

import { computeDelta, indexBy, type Delta } from '../lib/compare';
import { element, tableRow } from '../lib/dom';
import { humanizeIdentifier } from '../lib/format';
import type { EditorShellCase, EditorShellMeasurement } from '../types/measurement';
import type { ReportView } from './ReportView';

/** The measured number of one case, whichever field carries it. */
function caseValue(entry: EditorShellCase): number {
  return entry.value ?? entry.duration_ms ?? 0;
}

/** Formats a case's value in its own unit (milliseconds by default). */
function formatCaseValue(entry: EditorShellCase): string {
  const value = caseValue(entry);
  switch (entry.unit ?? 'ms') {
    case '%':
      return `${value.toFixed(2)}%`;
    case 'x':
      return `${value.toFixed(3)}x`;
    case 'fps':
      return `${value.toFixed(1)} fps`;
    default:
      return value >= 1000
        ? `${(value / 1000).toFixed(2)} s`
        : `${value.toFixed(1)} ms`;
  }
}

/** The spread of the case's samples, when there are any to spread. */
function formatSpread(entry: EditorShellCase): string {
  const samples = entry.samples_ms ?? entry.samples;
  if (!samples || samples.length < 2) return '—';
  const mean = samples.reduce((sum, value) => sum + value, 0) / samples.length;
  const variance =
    samples.reduce((sum, value) => sum + (value - mean) * (value - mean), 0) /
    (samples.length - 1);
  const std = Math.sqrt(variance);
  const unit = entry.unit ?? 'ms';
  if (unit === 'ms' && std >= 1000) {
    return `± ${(std / 1000).toFixed(2)} s (n=${samples.length})`;
  }
  return `± ${std.toFixed(unit === 'ms' ? 2 : 3)} (n=${samples.length})`;
}

export function renderEditorShellReport(
  measurement: EditorShellMeasurement,
  baseline: EditorShellMeasurement | null,
): ReportView {
  const payload = measurement.measurement;
  const baselineCases = baseline
    ? indexBy(baseline.measurement.cases, (entry) => entry.name)
    : new Map<string, EditorShellCase>();

  const content = element('div');
  content.appendChild(renderTargetTiles(measurement, baselineCases));
  content.appendChild(renderCasesSection(payload.cases, baselineCases));

  if (payload.standalone_binary === null) {
    content.appendChild(
      element('p', {
        class: 'section-hint',
        text: 'The standalone comparison was skipped; the fps and startup ratio cases are not part of this run.',
      }),
    );
  }

  return { content, sidebar: null };
}

// =============================================================================
// D32 targets
// =============================================================================

function renderTargetTiles(
  measurement: EditorShellMeasurement,
  baselineCases: Map<string, EditorShellCase>,
): HTMLElement {
  const row = element('div', { class: 'stat-row' });
  const d32 = measurement.measurement.d32 ?? {};
  for (const [name, target] of Object.entries(d32)) {
    const entry = measurement.measurement.cases.find((candidate) => candidate.name === name);
    const tile = element('div', { class: 'stat-tile' });
    tile.appendChild(element('div', { class: 'stat-label', text: humanizeIdentifier(name) }));
    tile.appendChild(
      element('div', {
        class: 'stat-value',
        text: entry ? formatCaseValue(entry) : '—',
      }),
    );
    const baselineEntry = baselineCases.get(name);
    const delta =
      entry && baselineEntry
        ? computeDelta(
            caseValue(entry),
            caseValue(baselineEntry),
            entry.better_when ?? 'lower',
          )
        : null;
    const deltaText = [
      `${target.met ? 'met' : 'MISSED'} · ${target.name}`,
      delta ? `${delta.label}` : '',
    ]
      .filter((part) => part.length > 0)
      .join(' · ');
    tile.appendChild(
      element('div', {
        class: `stat-delta ${target.met ? 'change-improved' : 'change-regressed'}`,
        text: deltaText,
      }),
    );
    row.appendChild(tile);
  }
  return row;
}

// =============================================================================
// Case cards
// =============================================================================

function renderCasesSection(
  cases: EditorShellCase[],
  baselineCases: Map<string, EditorShellCase>,
): HTMLElement {
  const section = element('div', { class: 'section' });
  section.appendChild(element('h2', { text: 'Cases' }));
  section.appendChild(
    element('p', {
      class: 'section-hint',
      text: 'Every number the run recorded, in its own unit. Ratio cases put the editor against the standalone on the same project and machine.',
    }),
  );
  for (const entry of cases) {
    const baselineEntry = baselineCases.get(entry.name) ?? null;
    const delta = baselineEntry
      ? computeDelta(
          caseValue(entry),
          caseValue(baselineEntry),
          entry.better_when ?? 'lower',
        )
      : null;
    section.appendChild(renderCaseCard(entry, baselineEntry, delta));
  }
  return section;
}

function renderCaseCard(
  entry: EditorShellCase,
  baselineEntry: EditorShellCase | null,
  delta: Delta | null,
): HTMLElement {
  const classes = ['case-card'];
  if (delta && delta.direction !== 'unchanged') classes.push(delta.direction);
  const card = element('div', { class: classes.join(' '), id: `case-${entry.name}` });

  card.appendChild(
    element(
      'div',
      { class: 'case-card-header' },
      element('h3', { text: humanizeIdentifier(entry.name) }),
      element('span', { class: 'case-session', text: entry.kind }),
      element('span', { class: 'case-headline', text: formatCaseValue(entry) }),
    ),
  );
  if (entry.description) {
    card.appendChild(element('p', { class: 'case-description', text: entry.description }));
  }

  const facts = element('table', { class: 'data-table' });
  const factBody = element('tbody');
  const addFact = (label: string, value: string | HTMLElement): void => {
    factBody.appendChild(tableRow([label, { text: value, class: 'numeric' }]));
  };

  addFact('Measured', formatCaseValue(entry));
  const spread = formatSpread(entry);
  if (spread !== '—') addFact('Spread', spread);
  if (entry.frames_per_second) {
    addFact('Frame rate', `${entry.frames_per_second.toFixed(1)} fps`);
  }
  if (entry.panels !== undefined) addFact('Tabsets painted', String(entry.panels));
  if (entry.steady_seconds !== undefined) {
    addFact('Window', `${entry.steady_seconds.toFixed(1)} s`);
  }
  if (entry.target) {
    addFact(
      'D32 target',
      element('span', {
        class: `change-${entry.target.met ? 'improved' : 'regressed'}`,
        text: `${entry.target.met ? 'met' : 'MISSED'} · ${entry.target.name}`,
      }),
    );
  }
  if (baselineEntry && delta) {
    addFact(
      'Baseline',
      element(
        'span',
        {},
        `${formatCaseValue(baselineEntry)} · `,
        element('span', {
          class: `change-${delta.direction}`,
          text: delta.label,
        }),
      ),
    );
  }
  facts.appendChild(factBody);
  card.appendChild(element('div', { class: 'table-scroll' }, facts));
  return card;
}
