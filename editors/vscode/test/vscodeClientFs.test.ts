import { describe, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  saveResult: true,
  isDirty: false,
}));

vi.mock('vscode', () => {
  class Range { constructor(..._args: unknown[]) {} }
  class WorkspaceEdit { replace(): void {} }
  return {
    Range,
    WorkspaceEdit,
    Uri: { file: (fsPath: string) => ({ fsPath, scheme: 'file' }) },
    workspace: {
      fs: { stat: vi.fn(async () => ({})), writeFile: vi.fn(async () => undefined) },
      openTextDocument: vi.fn(async () => ({
        lineCount: 1,
        get isDirty() { return state.isDirty; },
        save: async () => state.saveResult,
      })),
      applyEdit: vi.fn(async () => true),
      getConfiguration: () => ({ inspect: () => undefined }),
    },
  };
});

import { vscodeClientFsHost } from '../src/vscodeClientFs';

describe('vscodeClientFsHost.writeText', () => {
  // mini-agent-j4jwk
  it('treats save() === false on a clean document as success', async () => {
    state.saveResult = false;
    state.isDirty = false;
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'same')).resolves.toBeUndefined();
  });

  it('still fails when the document is dirty after a refused save', async () => {
    state.saveResult = false;
    state.isDirty = true;
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'x')).rejects.toThrow(/could not save/);
  });
});
