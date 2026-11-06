import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import type * as acp from '@agentclientprotocol/sdk';

export const MAX_REFERENCES = 20;
export const MAX_SNIPPET_BYTES = 64 * 1024;
export const MAX_TOTAL_REFERENCE_BYTES = 256 * 1024;
const MIN_SNIPPET_BUDGET_BYTES = 256;

/** A user-attached reference, normalised from vscode.ChatPromptReference by the caller. */
export type ReferenceInput =
  | { readonly kind: 'file'; readonly fsPath: string; readonly name?: string }
  | {
    readonly kind: 'snippet'; readonly fsPath: string; readonly text: string;
    readonly startLine?: number; readonly endLine?: number; readonly name?: string;
  }
  | { readonly kind: 'text'; readonly text: string; readonly name?: string };

export interface PromptBuildOptions {
  readonly text: string;
  readonly references: readonly ReferenceInput[];
  readonly workspaceRoot: string;
  /** False for an untrusted workspace: every reference is refused. */
  readonly trusted: boolean;
  /** Agent advertised promptCapabilities.embeddedContext. */
  readonly embeddedContext: boolean;
  /** Resolves symlinks; injected so the containment check is testable. */
  readonly realpath: (target: string) => Promise<string>;
}

export interface PromptBuild {
  readonly blocks: acp.ContentBlock[];
  /** Human-readable reasons for every dropped or clipped reference. */
  readonly notes: string[];
}

function contained(root: string, target: string): boolean {
  const relative = path.relative(root, target);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
}

/** Resolved path when it exists inside the workspace root (symlinks followed), else undefined. */
async function containedRealPath(
  options: PromptBuildOptions, fsPath: string,
): Promise<string | undefined> {
  if (!path.isAbsolute(fsPath)) { return undefined; }
  try {
    const root = await options.realpath(options.workspaceRoot);
    const resolved = await options.realpath(fsPath);
    return contained(root, resolved) ? resolved : undefined;
  } catch {
    return undefined;
  }
}

function clipBytes(text: string, limit: number): { text: string; omitted: number } {
  const bytes = Buffer.from(text, 'utf8');
  if (bytes.length <= limit) { return { text, omitted: 0 }; }
  let end = Math.max(0, limit);
  // Back up to a UTF-8 character boundary so no character is split.
  while (end > 0 && ((bytes[end] ?? 0) & 0xc0) === 0x80) { end -= 1; }
  return { text: bytes.subarray(0, end).toString('utf8'), omitted: bytes.length - end };
}

function label(reference: ReferenceInput): string {
  const name = reference.name?.trim();
  if (name) { return name.slice(0, 200); }
  return reference.kind === 'text' ? 'reference' : path.basename(reference.fsPath);
}

/**
 * Turns the prompt text plus attached references into ACP content blocks.
 * Count, per-snippet and total bytes are bounded, only existing files inside the
 * workspace are linked, and every omission is reported in `notes` so it is visible.
 */
export async function buildPromptBlocks(options: PromptBuildOptions): Promise<PromptBuild> {
  const blocks: acp.ContentBlock[] = [{ type: 'text', text: options.text }];
  const notes: string[] = [];
  if (options.references.length > 0 && !options.trusted) {
    notes.push('References were not sent: the workspace is not trusted.');
    return { blocks, notes };
  }
  let total = 0;
  let accepted = 0;
  for (const reference of options.references) {
    if (accepted >= MAX_REFERENCES) {
      notes.push(`Only the first ${MAX_REFERENCES} references were sent; ${options.references.length - accepted} more were dropped.`);
      break;
    }
    const name = label(reference);
    if (reference.kind === 'file') {
      const resolved = await containedRealPath(options, reference.fsPath);
      if (!resolved) { notes.push(`Skipped "${name}": not an existing file inside the workspace.`); continue; }
      blocks.push({ type: 'resource_link', name, uri: pathToFileURL(resolved).href });
      accepted += 1;
      continue;
    }
    let uri: string | undefined;
    if (reference.kind === 'snippet') {
      const resolved = await containedRealPath(options, reference.fsPath);
      if (!resolved) { notes.push(`Skipped "${name}": not inside the workspace.`); continue; }
      uri = pathToFileURL(resolved).href;
    }
    const budget = Math.min(MAX_SNIPPET_BYTES, MAX_TOTAL_REFERENCE_BYTES - total);
    if (budget < MIN_SNIPPET_BUDGET_BYTES) { notes.push(`Skipped "${name}": the ${MAX_TOTAL_REFERENCES_KIB} KiB reference budget is used up.`); continue; }
    const clipped = clipBytes(reference.text, budget);
    total += Buffer.byteLength(clipped.text, 'utf8');
    if (clipped.omitted > 0) {
      notes.push(`"${name}" was cut after ${clipped.text.length} characters (${clipped.omitted} bytes omitted).`);
    }
    const range = reference.kind === 'snippet' && reference.startLine !== undefined
      ? `${name}:${reference.startLine + 1}${reference.endLine !== undefined && reference.endLine !== reference.startLine ? `-${reference.endLine + 1}` : ''}`
      : name;
    const body = clipped.omitted > 0 ? `${clipped.text}\n[truncated: ${clipped.omitted} bytes omitted]` : clipped.text;
    if (uri !== undefined && options.embeddedContext) {
      const fragment = reference.kind === 'snippet' && reference.startLine !== undefined
        ? `#L${reference.startLine + 1}${reference.endLine !== undefined && reference.endLine !== reference.startLine ? `-${reference.endLine + 1}` : ''}`
        : '';
      blocks.push({ type: 'resource', resource: { uri: `${uri}${fragment}`, mimeType: 'text/plain', text: body } });
    } else {
      blocks.push({ type: 'text', text: `Reference ${range}:\n${body}` });
    }
    accepted += 1;
  }
  return { blocks, notes };
}

const MAX_TOTAL_REFERENCES_KIB = MAX_TOTAL_REFERENCE_BYTES / 1024;
