import { describe, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  saveResult: true,
  isDirty: false,
  text: 'before',
  version: 1,
  saves: 0,
  /** Runs inside openTextDocument, i.e. while the write "awaits" the document. */
  duringOpen: undefined as undefined | (() => void),
  /** Runs when the edit is handed to the editor, before it lands. */
  duringApply: undefined as undefined | (() => void),
}));

vi.mock('vscode', () => {
  class Range { constructor(..._args: unknown[]) {} }
  class WorkspaceEdit {
    next = '';
    replace(_uri: unknown, _range: unknown, text: string): void { this.next = text; }
  }
  return {
    Range,
    WorkspaceEdit,
    Uri: { file: (fsPath: string) => ({ fsPath, scheme: 'file' }) },
    workspace: {
      fs: { stat: vi.fn(async () => ({})), writeFile: vi.fn(async () => undefined) },
      openTextDocument: vi.fn(async () => {
        state.duringOpen?.();
        return {
          lineCount: 1,
          get version() { return state.version; },
          get isDirty() { return state.isDirty; },
          getText: () => state.text,
          save: async () => { state.saves++; return state.saveResult; },
        };
      }),
      applyEdit: vi.fn(async (edit: { next: string }) => {
        state.duringApply?.();
        if (state.text !== edit.next) { state.text = edit.next; state.version++; }
        return true;
      }),
      getConfiguration: () => ({ inspect: () => undefined }),
    },
  };
});

import { ClientFsConflict, sha256Hex } from '../src/clientFs';
import { vscodeClientFsHost } from '../src/vscodeClientFs';

const reset = () => {
  state.saveResult = true; state.isDirty = false; state.text = 'before'; state.version = 1;
  state.saves = 0; state.duringOpen = undefined; state.duringApply = undefined;
};
const before = sha256Hex('before');

describe('vscodeClientFsHost.writeText', () => {
  // mini-agent-j4jwk
  it('treats save() === false on a clean document as success', async () => {
    reset();
    state.saveResult = false;
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'same', before)).resolves.toBeUndefined();
  });

  it('still fails when the document is dirty after a refused save', async () => {
    reset();
    state.saveResult = false;
    state.isDirty = true;
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'x', before)).rejects.toThrow(/could not save/);
  });

  // mini-agent-xorxl.14
  it('writes and saves when the buffer still holds the expected text', async () => {
    reset();
    await vscodeClientFsHost.writeText('/w/a.ts', 'agent replacement', before);
    expect(state.text).toBe('agent replacement');
    expect(state.saves).toBe(1);
  });

  it('refuses, keeps the user text and does not save when the buffer changed before the request', async () => {
    reset();
    state.text = 'before + user text';
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'agent replacement', before)).rejects.toBeInstanceOf(ClientFsConflict);
    expect(state.text).toBe('before + user text');
    expect(state.saves).toBe(0);
  });

  it('refuses when the buffer changes while the write awaits openTextDocument', async () => {
    reset();
    state.duringOpen = () => { state.text = 'before + user text'; state.version++; };
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'agent replacement', before)).rejects.toBeInstanceOf(ClientFsConflict);
    expect(state.text).toBe('before + user text');
    expect(state.saves).toBe(0);
  });

  it('does not save over a replacement the user typed into while it was being applied', async () => {
    reset();
    state.duringApply = () => { state.version += 2; };
    await expect(vscodeClientFsHost.writeText('/w/a.ts', 'agent replacement', before)).rejects.toBeInstanceOf(ClientFsConflict);
    expect(state.saves).toBe(0);
  });
});
