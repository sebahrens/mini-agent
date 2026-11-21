import type * as acp from '@agentclientprotocol/sdk';

export const PROPOSED_SCHEME = 'mini-agent-proposed';
const MAX_DOCUMENTS = 32;
const MAX_DOCUMENT_CHARS = 2 * 1024 * 1024;

/** The old and proposed text of one edit, or undefined when it cannot be shown whole. */
export interface DiffSides { readonly path: string; readonly before: string; readonly after: string }

/**
 * Sides for the native diff editor. A diff the agent marked truncated or partial
 * does not carry the whole file, so it falls back to the text summary.
 */
export function diffSides(entry: acp.Diff): DiffSides | undefined {
  const meta = entry._meta ?? {};
  if (meta['mini-agent-truncated'] || meta['mini-agent-partial']) { return undefined; }
  const before = entry.oldText ?? '';
  if (before.length > MAX_DOCUMENT_CHARS || entry.newText.length > MAX_DOCUMENT_CHARS) { return undefined; }
  return { path: entry.path, before, after: entry.newText };
}

/** First diff of a tool call that can be opened in the diff editor. */
export function firstOpenableDiff(content: readonly acp.ToolCallContent[] | null | undefined): DiffSides | undefined {
  for (const entry of content ?? []) {
    if (entry.type === 'diff') {
      const sides = diffSides(entry);
      if (sides) { return sides; }
    }
  }
  return undefined;
}

/** Read-only text documents for `vscode.diff`, bounded in count; oldest are dropped. */
export class ProposedContent {
  private readonly documents = new Map<string, string>();
  private counter = 0;

  /** Registers text and returns the URI string it is served at. */
  add(side: 'before' | 'after', path: string, text: string): string {
    this.counter += 1;
    const base = path.split(/[\\/]/u).pop() || 'file';
    const key = `${PROPOSED_SCHEME}:/${this.counter}/${side}/${encodeURIComponent(base)}`;
    this.documents.set(key, text);
    while (this.documents.size > MAX_DOCUMENTS) {
      const oldest = this.documents.keys().next().value as string | undefined;
      if (oldest === undefined) { break; }
      this.documents.delete(oldest);
    }
    return key;
  }

  get(uri: string): string {
    return this.documents.get(uri) ?? '';
  }
}
