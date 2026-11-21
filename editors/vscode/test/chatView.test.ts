import { describe, expect, it } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import { ChatViewModel, MAX_ENTRIES } from '../src/chatViewModel';
import { ChatViewController, parseViewMessage, renderChatHtml } from '../src/chatView';
import { ProposedContent, diffSides, firstOpenableDiff, PROPOSED_SCHEME } from '../src/diffView';
import { activeEditorReference, diagnosticsReference } from '../src/editorContext';

const text = (value: string): acp.ContentBlock => ({ type: 'text', text: value });

describe('ChatViewModel', () => {
  it('shows the reply, a persistent tool list with diff body, the plan and the usage meter', () => {
    const model = new ChatViewModel();
    model.addUser('fix it');
    model.apply({ sessionUpdate: 'agent_message_chunk', content: text('Working') });
    model.apply({ sessionUpdate: 'agent_message_chunk', content: text(' on it') });
    model.apply({ sessionUpdate: 'tool_call', toolCallId: 't1', title: 'edit a.ts', status: 'pending' });
    model.apply({
      sessionUpdate: 'tool_call_update', toolCallId: 't1', status: 'completed',
      content: [{ type: 'diff', path: '/w/a.ts', oldText: 'a\n', newText: 'b\n' }],
      locations: [{ path: '/w/a.ts' }],
    });
    model.apply({ sessionUpdate: 'plan', entries: [{ content: 'step one', priority: 'medium', status: 'completed' }] });
    model.apply({ sessionUpdate: 'usage_update', used: 120, size: 1000 });
    model.finish('end_turn');
    const snap = model.snapshot();
    expect(snap.entries.map(e => e.kind)).toEqual(['user', 'agent', 'tool']);
    expect(snap.entries[1]).toMatchObject({ text: 'Working on it' });
    expect(snap.entries[2]).toMatchObject({ title: 'edit a.ts', status: 'completed', paths: ['/w/a.ts'] });
    expect((snap.entries[2] as { body: string }).body).toContain('-a');
    // The plan and meter stay after the turn ended.
    expect(snap.plan).toEqual([{ content: 'step one', status: 'completed' }]);
    expect(snap.usage).toEqual({ used: 120, size: 1000 });
    expect(snap.busy).toBe(false);
  });

  it('bounds the number of retained entries', () => {
    const model = new ChatViewModel();
    for (let i = 0; i < MAX_ENTRIES + 20; i += 1) { model.notice(`n${i}`); }
    expect(model.snapshot().entries).toHaveLength(MAX_ENTRIES);
  });
});

describe('chat view page and controller', () => {
  it('uses a nonce CSP and never inserts model text as HTML', () => {
    const html = renderChatHtml('abc123', 'vscode-resource:');
    expect(html).toContain("script-src 'nonce-abc123'");
    expect(html).toContain("default-src 'none'");
    expect(html).not.toContain('innerHTML');
  });

  it('validates webview messages', () => {
    expect(parseViewMessage({ type: 'send', text: 'hi' })).toEqual({ type: 'send', text: 'hi' });
    expect(parseViewMessage({ type: 'send', text: '  ' })).toBeUndefined();
    expect(parseViewMessage({ type: 'send', text: 5 })).toBeUndefined();
    expect(parseViewMessage({ type: 'exec', cmd: 'x' })).toBeUndefined();
    expect(parseViewMessage(null)).toBeUndefined();
  });

  it('sends a prompt over the shared session and posts the reply and tool call', async () => {
    const posted: unknown[] = [];
    const controller = new ChatViewController({
      post: state => { posted.push(state); },
      openPath: () => undefined,
      runPrompt: async (_text, onUpdate) => {
        onUpdate({ sessionUpdate: 'agent_message_chunk', content: text('done') });
        onUpdate({ sessionUpdate: 'tool_call', toolCallId: 't', title: 'shell', status: 'completed' });
        onUpdate({ sessionUpdate: 'plan', entries: [{ content: 'p', priority: 'low', status: 'pending' }] });
        return 'end_turn';
      },
    });
    controller.handle({ type: 'send', text: 'go' });
    await new Promise(resolve => setTimeout(resolve, 0));
    const last = posted[posted.length - 1] as ReturnType<ChatViewModel['snapshot']>;
    expect(last.entries.map(e => e.kind)).toEqual(['user', 'agent', 'tool']);
    // The plan stays visible after the turn ended.
    expect(last.plan).toHaveLength(1);
    expect(last.busy).toBe(false);
  });
});

describe('native diff', () => {
  const diff: acp.Diff = { path: '/w/a.ts', oldText: 'one\n', newText: 'two\n' };

  it('opens the whole old and proposed text', () => {
    expect(diffSides(diff)).toEqual({ path: '/w/a.ts', before: 'one\n', after: 'two\n' });
    expect(firstOpenableDiff([{ type: 'diff', ...diff }])?.after).toBe('two\n');
  });

  it('falls back to the text summary for truncated or partial previews', () => {
    expect(diffSides({ ...diff, _meta: { 'mini-agent-truncated': true } })).toBeUndefined();
    expect(diffSides({ ...diff, _meta: { 'mini-agent-partial': true } })).toBeUndefined();
  });

  it('serves registered text by URI and bounds the document count', () => {
    const store = new ProposedContent();
    const uri = store.add('after', '/w/a.ts', 'proposed');
    expect(uri.startsWith(`${PROPOSED_SCHEME}:`)).toBe(true);
    expect(store.get(uri)).toBe('proposed');
    const first = store.add('before', '/w/b.ts', 'x');
    for (let i = 0; i < 40; i += 1) { store.add('after', '/w/c.ts', 'y'); }
    expect(store.get(first)).toBe('');
  });
});

describe('active editor context', () => {
  const state = { scheme: 'file', fsPath: '/w/a.ts' };

  it('references the selection, else the open file, and honours the setting', () => {
    expect(activeEditorReference({ ...state, selection: { startLine: 1, endLine: 2, text: 'sel' } }, true))
      .toMatchObject({ kind: 'snippet', text: 'sel', startLine: 1, endLine: 2 });
    expect(activeEditorReference(state, true)).toMatchObject({ kind: 'file', fsPath: '/w/a.ts' });
    expect(activeEditorReference(state, false)).toBeUndefined();
    expect(activeEditorReference({ scheme: 'untitled', fsPath: 'x' }, true)).toBeUndefined();
    expect(activeEditorReference(undefined, true)).toBeUndefined();
  });

  it('bounds diagnostics, lists errors first and is opt-in', () => {
    const items = Array.from({ length: 60 }, (_, i) => ({
      fsPath: '/w/a.ts', line: i, severity: (i === 59 ? 'error' : 'warning') as 'error' | 'warning', message: `m${i}`,
    }));
    const reference = diagnosticsReference(items, true);
    expect(reference).toMatchObject({ kind: 'text' });
    const body = (reference as { text: string }).text;
    expect(body.split('\n')[0]).toContain('error');
    expect(body).toContain('10 more not shown');
    expect(diagnosticsReference(items, false)).toBeUndefined();
    expect(diagnosticsReference([], true)).toBeUndefined();
  });
});
