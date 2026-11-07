import { describe, expect, it } from 'vitest';
import {
  MAX_IMAGE_BYTES, MAX_REFERENCES, MAX_SNIPPET_BYTES, buildPromptBlocks, type PromptBuildOptions, type ReferenceInput,
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

  // mini-agent-uxsfz
  it('links a workspace file whose name begins with two dots', async () => {
    const built = await buildPromptBlocks(options([{ kind: 'file', fsPath: '/work/space/..notes.txt' }]));
    expect(built.notes).toEqual([]);
    expect(built.blocks).toHaveLength(2);
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

  it('counts only references cut by the cap, not earlier skipped ones', async () => {
    const refs: ReferenceInput[] = [
      ...Array.from({ length: 5 }, (_, index) => ({ kind: 'unsupported', name: `u${index}`, reason: 'nope' }) as ReferenceInput),
      ...Array.from({ length: MAX_REFERENCES + 3 }, (_, index) => ({ kind: 'file', fsPath: `/work/space/f${index}` }) as ReferenceInput),
    ];
    const built = await buildPromptBlocks(options(refs));
    expect(built.notes.join()).toMatch(/3 more were dropped/);
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

describe('buildPromptBlocks images and unsupported references', () => {
  const png = new Uint8Array([1, 2, 3]);
  it('names every unsupported reference in the notes', async () => {
    const built = await buildPromptBlocks(options([
      { kind: 'unsupported', name: 'Untitled-1', reason: 'only local files can be sent, not untitled: references' },
    ]));
    expect(built.blocks).toHaveLength(1);
    expect(built.notes).toEqual(['Skipped "Untitled-1": only local files can be sent, not untitled: references.']);
  });

  it('forwards an image as an ACP image block only when the agent advertises image support', async () => {
    const image: ReferenceInput = { kind: 'image', mimeType: 'image/png', bytes: png };
    const on = await buildPromptBlocks(options([image], { image: true }));
    expect(on.blocks[1]).toEqual({ type: 'image', mimeType: 'image/png', data: 'AQID' });
    const off = await buildPromptBlocks(options([image]));
    expect(off.blocks).toHaveLength(1);
    expect(off.notes[0]).toMatch(/does not accept images/);
  });

  it('enforces the total image budget and count, reporting skipped images', async () => {
    const big = (name: string): ReferenceInput => ({ kind: 'image', mimeType: 'image/png', bytes: new Uint8Array(MAX_IMAGE_BYTES), name });
    const result = await buildPromptBlocks(options([big('a'), big('b'), big('c')], { image: true }));
    expect(result.blocks.filter(block => block.type === 'image')).toHaveLength(2);
    expect(result.notes.join('\n')).toMatch(/Skipped image "c".*total image budget/);
    const many = Array.from({ length: 10 }, (_, i): ReferenceInput => ({ kind: 'image', mimeType: 'image/png', bytes: png, name: `i${i}` }));
    const capped = await buildPromptBlocks(options(many, { image: true }));
    expect(capped.blocks.filter(block => block.type === 'image')).toHaveLength(8);
    expect(capped.notes.join('\n')).toMatch(/at most 8 images/);
  });

  it('refuses oversize, empty and unsupported-mime images with a note', async () => {
    const built = await buildPromptBlocks(options([
      { kind: 'image', mimeType: 'image/png', bytes: new Uint8Array(MAX_IMAGE_BYTES + 1) },
      { kind: 'image', mimeType: 'image/png', bytes: new Uint8Array(0) },
      { kind: 'image', mimeType: 'image/tiff', bytes: png },
    ], { image: true }));
    expect(built.blocks).toHaveLength(1);
    expect(built.notes).toHaveLength(3);
  });
});
