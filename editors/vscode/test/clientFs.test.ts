import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import {
  MAX_CLIENT_FILE_BYTES, clientCapabilities, readTextFile, writeTextFile,
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
    },
    writes,
    ...overrides,
  };
}
const request = (p: string, extra: object = {}): acp.ReadTextFileRequest => ({ sessionId: 's', path: p, ...extra });

describe('client file access', () => {
  it('is closed by default and opens only on request', () => {
    expect(clientCapabilities(false)).toEqual({});
    expect(clientCapabilities(true)).toEqual({ fs: { readTextFile: true, writeTextFile: true } });
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
      await expect(writeTextFile(ctx, { sessionId: 's', path: bad, content: 'x' }), bad).rejects.toThrow();
    }
    expect(ctx.writes).toEqual([]);
    const untrusted = context({ trusted: false });
    await expect(readTextFile(untrusted, request('/work/space/a'))).rejects.toThrow(/not trusted/);
    await expect(writeTextFile(untrusted, { sessionId: 's', path: '/work/space/a', content: 'x' })).rejects.toThrow(/not trusted/);
  });

  it('writes through the editor, creating files only inside existing workspace directories', async () => {
    const ctx = context();
    await writeTextFile(ctx, { sessionId: 's', path: '/work/space/new.txt', content: 'hi' });
    expect(ctx.writes).toEqual(['/work/space/new.txt=hi']);
    await expect(readTextFile(ctx, request('/work/space/missing.txt'))).rejects.toThrow(/not found/);
    await expect(writeTextFile(ctx, { sessionId: 's', path: '/work/space/missing/dir/x.txt', content: 'x' })).rejects.toThrow(/directory not found/);
  });

  it('bounds read and write sizes', async () => {
    const big = 'x'.repeat(MAX_CLIENT_FILE_BYTES + 1);
    const ctx = context({}, { '/work/space/big': big });
    await expect(readTextFile(ctx, request('/work/space/big'))).rejects.toThrow(/larger than/);
    const write = vi.fn();
    await expect(writeTextFile({ ...ctx, host: { ...ctx.host, writeText: write } },
      { sessionId: 's', path: '/work/space/big', content: big })).rejects.toThrow(/larger than/);
    expect(write).not.toHaveBeenCalled();
  });
});
