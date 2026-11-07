import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import {
  ClientFsConflict, MAX_CLIENT_FILE_BYTES, clientCapabilities, readTextFile, sha256Hex, writeTextFile,
  type ClientFsContext,
} from '../src/clientFs';

const root = '/work/space';
function context(overrides: Partial<ClientFsContext> = {}, files: Record<string, string> = {}): ClientFsContext & { writes: string[] } {
  const writes: string[] = [];
  return {
    workspaceRoot: root,
    trusted: true,
    host: {
      readText: async p => files[p] ?? 'dirty buffer\nline 2\nline 3',
      writeText: async (p, text) => { writes.push(`${p}=${text}`); },
      realpath: async p => {
        if (p.includes('missing')) { throw new Error('ENOENT'); }
        if (p.includes('escape-link')) { return '/etc/passwd'; }
        return p;
      },
      isSymlink: async p => p.includes('dangling-link'),
    },
    writes,
    ...overrides,
  };
}
/** A write request carrying the conditional-write precondition the agent sends. */
const writeRequest = (p: string, content: string, expected = 'x'): acp.WriteTextFileRequest => ({
  sessionId: 's', path: p, content, _meta: { miniAgent: { expectedSha256: sha256Hex(expected) } },
});
const request = (p: string, extra: object = {}): acp.ReadTextFileRequest => ({ sessionId: 's', path: p, ...extra });

describe('client file access', () => {
  it('is closed by default and opens only on request', () => {
    expect(clientCapabilities(false)).toEqual({});
    expect(clientCapabilities(true)).toEqual({
      fs: { readTextFile: true, writeTextFile: true },
      _meta: { miniAgent: { conditionalWriteTextFile: true } },
    });
  });

  it('reads the editor text, with line and limit windows', async () => {
    const ctx = context();
    expect(await readTextFile(ctx, request('/work/space/a.ts'))).toEqual({ content: 'dirty buffer\nline 2\nline 3' });
    expect(await readTextFile(ctx, request('/work/space/a.ts', { line: 2, limit: 1 }))).toEqual({ content: 'line 2' });
    await expect(readTextFile(ctx, request('/work/space/a.ts', { line: 0 }))).rejects.toThrow(/positive integer/);
    await expect(readTextFile(ctx, request('/work/space/a.ts', { limit: -1 }))).rejects.toThrow(/non-negative/);
  });

  it('refuses paths outside the workspace, relative paths, symlink escapes and untrusted use', async () => {
    const ctx = context();
    for (const bad of ['/etc/passwd', '/work/space/../secret', '/work/space-evil/a', '/work/space/escape-link', 'rel/a', '/work/space/a\0b']) {
      await expect(readTextFile(ctx, request(bad)), bad).rejects.toThrow();
      await expect(writeTextFile(ctx, writeRequest(bad, 'x')), bad).rejects.toThrow();
    }
    expect(ctx.writes).toEqual([]);
    const untrusted = context({ trusted: false });
    await expect(readTextFile(untrusted, request('/work/space/a'))).rejects.toThrow(/not trusted/);
    await expect(writeTextFile(untrusted, writeRequest('/work/space/a', 'x'))).rejects.toThrow(/not trusted/);
  });

  // mini-agent-uxsfz: a child whose name merely begins with '..' is inside the workspace.
  it('accepts workspace files whose name begins with two dots', async () => {
    const ctx = context();
    expect(await readTextFile(ctx, request('/work/space/..notes.txt'))).toEqual({ content: 'dirty buffer\nline 2\nline 3' });
    await writeTextFile(ctx, writeRequest('/work/space/..notes.txt', 'ok'));
    expect(ctx.writes).toEqual(['/work/space/..notes.txt=ok']);
    await expect(readTextFile(ctx, request('/work/space/../notes.txt'))).rejects.toThrow();
  });

  it('writes through the editor, creating files only inside existing workspace directories', async () => {
    const ctx = context();
    await writeTextFile(ctx, writeRequest('/work/space/new.txt', 'hi'));
    expect(ctx.writes).toEqual(['/work/space/new.txt=hi']);
    await expect(readTextFile(ctx, request('/work/space/missing.txt'))).rejects.toThrow(/not found/);
    await expect(writeTextFile(ctx, writeRequest('/work/space/missing/dir/x.txt', 'x'))).rejects.toThrow(/directory not found/);
  });

  // mini-agent-z7zva
  it('refuses to write through a dangling symlink whose target is unknown', async () => {
    const ctx = context();
    // realpath of the link fails (its target is absent) but its directory is inside the workspace.
    const host = { ...ctx.host, realpath: async (p: string) => {
      if (p.includes('dangling-link')) { throw new Error('ENOENT'); }
      return p;
    } };
    await expect(writeTextFile({ ...ctx, host },
      writeRequest('/work/space/dangling-link', 'x'))).rejects.toThrow(/symbolic link/);
    expect(ctx.writes).toEqual([]);
  });

  it('bounds read and write sizes', async () => {
    const big = 'x'.repeat(MAX_CLIENT_FILE_BYTES + 1);
    const ctx = context({}, { '/work/space/big': big });
    await expect(readTextFile(ctx, request('/work/space/big'))).rejects.toThrow(/larger than/);
    const write = vi.fn();
    await expect(writeTextFile({ ...ctx, host: { ...ctx.host, writeText: write } },
      writeRequest('/work/space/big', big))).rejects.toThrow(/larger than/);
    expect(write).not.toHaveBeenCalled();
  });

  // mini-agent-xorxl.14: the precondition travels with the write RPC.
  it('refuses a write that carries no expected-text precondition', async () => {
    const ctx = context();
    for (const meta of [undefined, {}, { miniAgent: {} }, { miniAgent: { expectedSha256: 'abc' } }, { miniAgent: { expectedSha256: 7 } }]) {
      await expect(writeTextFile(ctx, { sessionId: 's', path: '/work/space/a.ts', content: 'x', _meta: meta as never }))
        .rejects.toThrow(/unconditional writes are refused/);
    }
    expect(ctx.writes).toEqual([]);
  });

  it('passes the precondition to the editor, which refuses and keeps the user text when the buffer changed', async () => {
    // A buffer that the user edits after the agent's read response but before
    // the write request is applied (including while it awaits realpath/open).
    let buffer = 'before';
    let saves = 0;
    const host = {
      ...context().host,
      realpath: async (p: string) => { buffer = 'before + user text'; return p; },
      writeText: async (_p: string, text: string, expectedSha256: string) => {
        if (sha256Hex(buffer) !== expectedSha256) { throw new ClientFsConflict(); }
        buffer = text; saves++;
      },
    };
    const ctx = { ...context(), host };
    await expect(writeTextFile(ctx, writeRequest('/work/space/a.ts', 'agent replacement', 'before')))
      .rejects.toThrow(/changed in the editor/);
    expect(buffer).toBe('before + user text');
    expect(saves).toBe(0);
    // An unchanged buffer succeeds.
    buffer = 'before';
    const quiet = { ...ctx, host: { ...host, realpath: async (p: string) => p } };
    await writeTextFile(quiet, writeRequest('/work/space/a.ts', 'agent replacement', 'before'));
    expect(buffer).toBe('agent replacement');
    expect(saves).toBe(1);
  });
});
