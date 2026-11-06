import { describe, expect, it } from 'vitest';
import {
  MAX_REFERENCES, MAX_SNIPPET_BYTES, buildPromptBlocks, type PromptBuildOptions, type ReferenceInput,
} from '../src/references';

const root = '/work/space';
const realpath = async (target: string): Promise<string> => {
  if (target.includes('missing')) { throw new Error('ENOENT'); }
  // Model a symlink inside the workspace that points outside it.
  return target.includes('escape-link') ? '/etc/passwd' : target;
};

function options(references: ReferenceInput[], extra: Partial<PromptBuildOptions> = {}): PromptBuildOptions {
  return { text: 'hi', references, workspaceRoot: root, trusted: true, embeddedContext: true, realpath, ...extra };
}

describe('buildPromptBlocks', () => {
  it('links an in-workspace file as a resource_link after the prompt text', async () => {
    const built = await buildPromptBlocks(options([{ kind: 'file', fsPath: '/work/space/src/a.rs' }]));
    expect(built.blocks).toEqual([
      { type: 'text', text: 'hi' },
      { type: 'resource_link', name: 'a.rs', uri: 'file:///work/space/src/a.rs' },
    ]);
    expect(built.notes).toEqual([]);
  });

  it('embeds a ranged snippet with a line fragment, or inlines it without embeddedContext', async () => {
    const snippet: ReferenceInput = { kind: 'snippet', fsPath: '/work/space/a.ts', text: 'let x', startLine: 4, endLine: 6 };
    const embedded = await buildPromptBlocks(options([snippet]));
    expect(embedded.blocks[1]).toEqual({
      type: 'resource', resource: { uri: 'file:///work/space/a.ts#L5-7', mimeType: 'text/plain', text: 'let x' },
    });
    const inline = await buildPromptBlocks(options([snippet], { embeddedContext: false }));
    expect(inline.blocks[1]).toEqual({ type: 'text', text: 'Reference a.ts:5-7:\nlet x' });
  });

  it('refuses out-of-workspace paths, traversal, symlink escapes, relative paths and missing files', async () => {
    const built = await buildPromptBlocks(options([
      { kind: 'file', fsPath: '/etc/passwd' },
      { kind: 'file', fsPath: '/work/space/../secret' },
      { kind: 'file', fsPath: '/work/space-evil/a' },
      { kind: 'file', fsPath: '/work/space/escape-link' },
      { kind: 'file', fsPath: 'relative/a' },
      { kind: 'file', fsPath: '/work/space/missing' },
      { kind: 'snippet', fsPath: '/etc/shadow', text: 'root:x' },
    ]));
    expect(built.blocks).toEqual([{ type: 'text', text: 'hi' }]);
    expect(built.notes).toHaveLength(7);
  });

  it('refuses everything in an untrusted workspace', async () => {
    const built = await buildPromptBlocks(options([{ kind: 'file', fsPath: '/work/space/a' }], { trusted: false }));
    expect(built.blocks).toHaveLength(1);
    expect(built.notes[0]).toMatch(/not trusted/);
  });

  it('bounds the reference count and reports the drop', async () => {
    const many: ReferenceInput[] = Array.from({ length: MAX_REFERENCES + 5 },
      (_, index) => ({ kind: 'file', fsPath: `/work/space/f${index}` }));
    const built = await buildPromptBlocks(options(many));
    expect(built.blocks).toHaveLength(1 + MAX_REFERENCES);
    expect(built.notes.join()).toMatch(/5 more were dropped/);
  });

  it('clips oversized snippets visibly at a character boundary and bounds the total', async () => {
    const big = '雪'.repeat(MAX_SNIPPET_BYTES);
    const built = await buildPromptBlocks(options(Array.from({ length: 6 },
      () => ({ kind: 'snippet', fsPath: '/work/space/a', text: big }) as ReferenceInput)));
    const embedded = built.blocks.filter(block => block.type === 'resource');
    expect(embedded.length).toBeLessThan(6);
    for (const block of embedded) {
      if (block.type !== 'resource' || !('text' in block.resource)) { throw new Error('unexpected block'); }
      expect(block.resource.text).toMatch(/\[truncated: \d+ bytes omitted\]$/);
      expect(Buffer.byteLength(block.resource.text, 'utf8')).toBeLessThan(MAX_SNIPPET_BYTES + 64);
    }
    expect(built.notes.length).toBeGreaterThan(0);
  });

  it('passes plain text references through without a path', async () => {
    const built = await buildPromptBlocks(options([{ kind: 'text', text: 'value', name: 'var' }]));
    expect(built.blocks[1]).toEqual({ type: 'text', text: 'Reference var:\nvalue' });
  });
});
