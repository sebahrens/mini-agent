import type { ReferenceInput } from './references';

/** What the extension host knows about the active editor, normalised by the caller. */
export interface ActiveEditorState {
  readonly scheme: string;
  readonly fsPath: string;
  readonly languageId?: string;
  /** Zero-based; undefined when there is no selection. */
  readonly selection?: { readonly startLine: number; readonly endLine: number; readonly text: string };
  readonly cursorLine?: number;
}

export interface DiagnosticItem {
  readonly fsPath: string;
  readonly line: number;
  readonly severity: 'error' | 'warning';
  readonly message: string;
}

export const MAX_DIAGNOSTIC_ITEMS = 50;
const MAX_DIAGNOSTIC_MESSAGE = 300;

/**
 * Bounded reference for the active editor: the selection when there is one,
 * otherwise a link to the open file. Disabled or non-file documents yield nothing.
 */
export function activeEditorReference(state: ActiveEditorState | undefined, enabled: boolean): ReferenceInput | undefined {
  if (!enabled || !state || state.scheme !== 'file') { return undefined; }
  if (state.selection && state.selection.text.length > 0) {
    return {
      kind: 'snippet', fsPath: state.fsPath, text: state.selection.text,
      startLine: state.selection.startLine, endLine: state.selection.endLine, name: 'active selection',
    };
  }
  return { kind: 'file', fsPath: state.fsPath, name: 'active file' };
}

/** Opt-in text reference carrying the workspace's current errors and warnings, errors first. */
export function diagnosticsReference(items: readonly DiagnosticItem[], enabled: boolean): ReferenceInput | undefined {
  if (!enabled || items.length === 0) { return undefined; }
  const sorted = [...items].sort((a, b) => (a.severity === b.severity ? 0 : a.severity === 'error' ? -1 : 1));
  const shown = sorted.slice(0, MAX_DIAGNOSTIC_ITEMS).map(item => {
    const message = item.message.replace(/\s+/gu, ' ').slice(0, MAX_DIAGNOSTIC_MESSAGE);
    return `${item.fsPath}:${item.line + 1}: ${item.severity}: ${message}`;
  });
  const more = sorted.length - shown.length;
  if (more > 0) { shown.push(`… ${more} more not shown`); }
  return { kind: 'text', text: shown.join('\n'), name: 'diagnostics' };
}
