import { createHash } from 'node:crypto';
import * as path from 'node:path';
import type * as acp from '@agentclientprotocol/sdk';

export const MAX_CLIENT_FILE_BYTES = 2 * 1024 * 1024;

/** The editor operations the handlers need; the VS Code binding lives in extension code. */
export interface ClientFsHost {
  /** Current text of the file, including unsaved editor changes. */
  readText(fsPath: string): Promise<string>;
  /**
   * Replace the file's text through the editor (undo stack, dirty state) and
   * save it, but only if the live buffer's SHA-256 still equals
   * `expectedSha256`. The comparison must be made after the document is opened
   * and in the same step that applies the edit (no await in between), and must
   * throw {@link ClientFsConflict} on a mismatch without changing or saving
   * anything. A file that no longer exists is also a conflict.
   */
  writeText(fsPath: string, text: string, expectedSha256: string): Promise<void>;
  realpath(fsPath: string): Promise<string>;
  /** True when the final path component is itself a symlink (dangling ones included); false when absent. */
  isSymlink(fsPath: string): Promise<boolean>;
}

export interface ClientFsContext {
  readonly workspaceRoot: string;
  readonly trusted: boolean;
  readonly host: ClientFsHost;
}

export class ClientFsError extends Error {}

/** The buffer no longer holds the text the agent computed its change from. */
export class ClientFsConflict extends ClientFsError {
  constructor() {
    super('the file changed in the editor after it was read; no changes were applied. Re-read it and retry');
  }
}

/**
 * The replacement was applied to the live buffer but the user typed during the
 * apply window, so it was not saved. Unlike {@link ClientFsConflict}, the
 * buffer did change.
 */
export class ClientFsAppliedUnsaved extends ClientFsError {
  constructor() {
    super('the file changed in the editor while the replacement was being applied; the replacement is in the editor buffer but was NOT saved (undo it to restore the previous text). Re-read the file and retry');
  }
}

/** `_meta` namespace of the conditional-write extension (see the agent's acp/client_fs.rs). */
export const META_NAMESPACE = 'miniAgent';

export function sha256Hex(text: string): string {
  return createHash('sha256').update(text, 'utf8').digest('hex');
}

/** Capabilities to advertise: closed unless the user turned `mini-agent.clientFileAccess` on. */
export function clientCapabilities(enabled: boolean): acp.ClientCapabilities {
  return enabled
    ? {
      fs: { readTextFile: true, writeTextFile: true },
      // Without this the agent never sends a write: a plain writeTextFile
      // replaces the whole buffer and cannot protect text typed after the read.
      _meta: { [META_NAMESPACE]: { conditionalWriteTextFile: true } },
    }
    : {};
}

function inside(root: string, target: string): boolean {
  const relative = path.relative(root, target);
  return relative === '' || (relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
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
    // realpath failed, so the path either does not exist or is a dangling link.
    // A dangling link would be followed by the write and create its target, which
    // may lie outside the workspace; refuse it rather than guess where it points.
    if (await context.host.isSymlink(requested)) {
      throw new ClientFsError('path is a dangling symbolic link');
    }
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

/**
 * `fs/write_text_file`: replaces the whole file through the editor, only when
 * the request carries the SHA-256 of the text it was computed from
 * (`_meta.miniAgent.expectedSha256`) and the live buffer still matches it.
 */
export async function writeTextFile(
  context: ClientFsContext, params: acp.WriteTextFileRequest,
): Promise<acp.WriteTextFileResponse> {
  if (Buffer.byteLength(params.content, 'utf8') > MAX_CLIENT_FILE_BYTES) {
    throw new ClientFsError(`content is larger than ${MAX_CLIENT_FILE_BYTES} bytes`);
  }
  const meta = params._meta as Record<string, Record<string, unknown> | undefined> | null | undefined;
  const expectedSha256 = meta?.[META_NAMESPACE]?.expectedSha256;
  if (typeof expectedSha256 !== 'string' || !/^[0-9a-f]{64}$/.test(expectedSha256)) {
    throw new ClientFsError('unconditional writes are refused: the request must carry _meta.miniAgent.expectedSha256');
  }
  const target = await containedPath(context, params.path, true);
  await context.host.writeText(target, params.content, expectedSha256);
  return {};
}
