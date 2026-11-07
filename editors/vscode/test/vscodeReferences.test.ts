import { describe, expect, it, vi } from 'vitest';

vi.mock('vscode', () => ({
  workspace: { openTextDocument: vi.fn(async () => ({ getText: () => 'code' })) },
  window: { activeTextEditor: undefined },
}));

import { collectReferences } from '../src/vscodeReferences';

const uri = (scheme: string, fsPath = '/x') => ({ scheme, fsPath });

describe('collectReferences', () => {
  it('turns non-file and unknown references into unsupported entries instead of dropping them', async () => {
    const out = await collectReferences({
      references: [
        { id: 'a', name: 'Untitled-1', value: uri('untitled') },
        { id: 'b', name: 'old.ts', value: { uri: uri('git'), range: { start: { line: 0 }, end: { line: 1 } } } },
        { id: 'c', name: 'weird', value: { something: true } },
        { id: 'd', name: 'ok.ts', value: uri('file', '/w/ok.ts') },
      ] as never,
    }, false);
    expect(out.filter(r => r.kind === 'unsupported').map(r => (r as { name: string }).name)).toEqual(['Untitled-1', 'old.ts', 'weird']);
    expect(out.at(-1)).toMatchObject({ kind: 'file', fsPath: '/w/ok.ts' });
  });

  // mini-agent-pcpu7
  it('skips a location whose document cannot be opened and keeps the rest', async () => {
    const vscode = await import('vscode');
    vi.mocked(vscode.workspace.openTextDocument).mockRejectedValueOnce(new Error('file not found'));
    const out = await collectReferences({
      references: [
        { id: 'a', name: 'gone.ts', value: { uri: uri('file', '/w/gone.ts'), range: { start: { line: 0 }, end: { line: 1 } } } },
        { id: 'b', name: 'ok.ts', value: uri('file', '/w/ok.ts') },
      ] as never,
    }, false);
    expect(out[0]).toMatchObject({ kind: 'unsupported', name: 'gone.ts' });
    expect(out[1]).toMatchObject({ kind: 'file', fsPath: '/w/ok.ts' });
  });

  it('reads image binary references and flags non-image binaries', async () => {
    const out = await collectReferences({
      references: [
        { id: 'i', name: 'shot.png', value: { mimeType: 'image/png', data: async () => new Uint8Array([9]) } },
        { id: 'p', name: 'doc.pdf', value: { mimeType: 'application/pdf', data: async () => new Uint8Array([9]) } },
      ] as never,
    }, false);
    expect(out[0]).toMatchObject({ kind: 'image', mimeType: 'image/png' });
    expect(out[1]).toMatchObject({ kind: 'unsupported', name: 'doc.pdf' });
  });

  // mini-agent-yjxk5
  it('keeps every attached reference so the builder can report the real drop count', async () => {
    const references = Array.from({ length: 40 }, (_, index) => (
      { id: `r${index}`, name: `f${index}.ts`, value: uri('file', `/w/f${index}.ts`) }));
    const out = await collectReferences({ references: references as never }, false);
    expect(out).toHaveLength(40);
  });
});
