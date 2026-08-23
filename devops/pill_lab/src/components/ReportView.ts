// DESCRIPTION: The single contract every category report implements. A report
//   returns its main content, optional sidebar contribution, and lifecycle
//   hooks - `onMount` for work that needs the nodes to be in the document
//   (chart sizing), `onUnmount` to release chart instances when the view is
//   replaced.
//
// --- SCRIPT ---

export interface ReportView {
  content: HTMLElement;
  sidebar: HTMLElement | null;
  onMount?: () => void;
  onUnmount?: () => void;
}
