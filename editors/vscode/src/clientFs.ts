import * as path from 'node:path';
import type * as acp from '@agentclientprotocol/sdk';

export const MAX_CLIENT_FILE_BYTES = 2 * 1024 * 1024;

/** The editor operations the handlers need; the VS Code binding lives in extension code. */
export interface ClientFsHost {
  /** Current text of the file, including unsaved editor changes. */
  readText(fsPath: string): Promise<string>;
  /** Replace the file's text through the editor (undo stack, dirty state) and save it. */
  writeText(fsPath: string, text: string): Promise<void>;
  realpath(fsPath: string): Promise<string>;
}

export interface ClientFsContext {
  readonly workspaceRoot: string;
  readonly trusted: boolean;
  readonly host: ClientFsHost;
}

export class ClientFsError extends Error {}

/** Capabilities to advertise: closed unless the user turned `mini-agent.clientFileAccess` on. */
export function clientCapabilities(enabled: boolean): acp.ClientCapabilities {
  return enabled ? { fs: { readTextFile: true, writeTextFile: true } } : {};
}

function inside(root: string, target: string): boolean {
  const relative = path.relative(root, target);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
}

/**
 * Resolve a path the agent asked for to a real path inside the workspace. A
 * missing file is allowed for writes (its directory must exist and resolve
 * inside the root); symlinks are followed before the containment check.
 */
async function containedPath(context: ClientFsContext, requested: string, mayCreate: boolean): Promise<string> {
  if (!context.trusted) { throw new ClientFsError('the workspace is not trusted'); }
  if (!path.isAbsolute(requested) || requested.includes('\0')) {
    throw new ClientFsError('path must be absolute');
  }
  const root = await context.host.realpath(context.workspaceRoot);
  let resolved: string;
  try {
    resolved = await context.host.realpath(requested);
  } catch {
    if (!mayCreate) { throw new ClientFsError('file not found'); }
    const parent = await context.host.realpath(path.dirname(requested)).catch(() => {
      throw new ClientFsError('directory not found');
    });
    resolved = path.join(parent, path.basename(requested));
  }
  if (!inside(root, resolved)) { throw new ClientFsError('path is outside the workspace'); }
  return resolved;
}

/** `fs/read_text_file`: `line` is 1-based, `limit` a line count; text is bounded. */
export async function readTextFile(
  context: ClientFsContext, params: acp.ReadTextFileRequest,
): Promise<acp.ReadTextFileResponse> {
  const target = await containedPath(context, params.path, false);
  const text = await context.host.readText(target);
  if (Buffer.byteLength(text, 'utf8') > MAX_CLIENT_FILE_BYTES) {
    throw new ClientFsError(`file is larger than ${MAX_CLIENT_FILE_BYTES} bytes`);
  }
  const line = params.line ?? undefined;
  const limit = params.limit ?? undefined;
  if (line === undefined && limit === undefined) { return { content: text }; }
  if ((line !== undefined && (!Number.isInteger(line) || line < 1))
    || (limit !== undefined && (!Number.isInteger(limit) || limit < 0))) {
    throw new ClientFsError('line must be a positive integer and limit a non-negative integer');
  }
  const lines = text.split('\n');
  const start = (line ?? 1) - 1;
  const end = limit === undefined ? lines.length : start + limit;
  return { content: lines.slice(start, end).join('\n') };
}

/** `fs/write_text_file`: replaces the whole file through the editor. */
export async function writeTextFile(
  context: ClientFsContext, params: acp.WriteTextFileRequest,
): Promise<acp.WriteTextFileResponse> {
  if (Buffer.byteLength(params.content, 'utf8') > MAX_CLIENT_FILE_BYTES) {
    throw new ClientFsError(`content is larger than ${MAX_CLIENT_FILE_BYTES} bytes`);
  }
  const target = await containedPath(context, params.path, true);
  await context.host.writeText(target, params.content);
  return {};
}
