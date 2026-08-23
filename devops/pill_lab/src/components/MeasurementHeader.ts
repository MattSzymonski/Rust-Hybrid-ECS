// DESCRIPTION: The title block above every report: what was measured and the
//   one-line label describing the run. Environment and git detail live in the
//   collapsible "environment" panels below the pickers, so this stays minimal.
//
// --- SCRIPT ---

import { element } from '../lib/dom';
import { CATEGORY_LABELS, type Measurement } from '../types/measurement';

export function renderMeasurementHeader(measurement: Measurement): HTMLElement {
  const container = element('div');
  container.appendChild(
    element('h1', { text: CATEGORY_LABELS[measurement.category] }),
  );
  container.appendChild(
    element('p', {
      class: 'subtitle',
      text: measurement.label || 'Measurement',
    }),
  );
  return container;
}
