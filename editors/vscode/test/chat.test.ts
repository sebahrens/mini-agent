import { describe, expect, it } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import { ChatUpdateRenderer, MAX_RENDER_CHARS, stopResult } from '../src/chat';

describe('ChatUpdateRenderer', () => {
  it('preserves assistant chunk order', () => {
    const renderer = new ChatUpdateRenderer();
    const first = renderer.render({
      sessionUpdate: 'agent_message_chunk',
      content: { type: 'text', text: 'first' },
    });
    const second = renderer.render({
      sessionUpdate: 'agent_message_chunk',
      content: { type: 'text', text: 'second' },
    });

    expect([...first, ...second]).toEqual([
      { kind: 'markdown', value: 'first' },
      { kind: 'markdown', value: 'second' },
    ]);
  });

  it('correlates tool updates with the original stable ID and title', () => {
    const renderer = new ChatUpdateRenderer();
    renderer.render({
      sessionUpdate: 'tool_call',
      toolCallId: 'call-7',
      title: 'Read Cargo.toml',
      status: 'in_progress',
    });

    expect(renderer.render({
      sessionUpdate: 'tool_call_update',
      toolCallId: 'call-7',
      status: 'completed',
    })).toEqual([{
      kind: 'progress',
      value: 'Read Cargo.toml (completed)',
    }]);
  });

  it('bounds individual large updates', () => {
    const renderer = new ChatUpdateRenderer();
    const [event] = renderer.render({
      sessionUpdate: 'agent_message_chunk',
      content: { type: 'text', text: 'x'.repeat(MAX_RENDER_CHARS + 100) },
    });

    expect(event.kind).toBe('markdown');
    expect(event.value.length).toBeLessThan(MAX_RENDER_CHARS + 40);
    expect(event.value).toContain('update truncated');
  });
});

describe('stopResult', () => {
  it.each<acp.StopReason>(['end_turn', 'cancelled'])('treats %s as a non-error terminal state', reason => {
    expect(stopResult(reason).errorDetails).toBeUndefined();
  });

  it.each<acp.StopReason>(['refusal', 'max_tokens', 'max_turn_requests'])('surfaces %s as an error state', reason => {
    expect(stopResult(reason).errorDetails?.message).toBeTruthy();
  });
});

describe('ChatUpdateRenderer tool output', () => {
  it('shows titles without call ids and renders results and diffs as markdown', () => {
    const renderer = new ChatUpdateRenderer();
    renderer.render({ sessionUpdate: 'tool_call', toolCallId: 'call-9', title: 'edit src/a.rs', status: 'pending' });
    const events = renderer.render({
      sessionUpdate: 'tool_call_update', toolCallId: 'call-9', status: 'completed',
      content: [
        { type: 'diff', path: '/w/src/a.rs', oldText: 'a\nold\nz', newText: 'a\nnew\nz' },
        { type: 'content', content: { type: 'text', text: 'wrote ``` fence' } },
      ],
    });
    expect(events[0]).toEqual({ kind: 'progress', value: 'edit src/a.rs (completed)' });
    const markdown = events[1];
    expect(markdown?.kind).toBe('markdown');
    expect(markdown?.value).not.toContain('call-9');
    expect(markdown?.value).toContain('```diff');
    expect(markdown?.value).toContain('-old\n+new');
    // A fence inside tool output must not terminate the surrounding block.
    expect(markdown?.value).toContain('````\nwrote ``` fence\n````');
  });

  it('does not render result bodies for in-progress updates and bounds huge diffs', () => {
    const renderer = new ChatUpdateRenderer();
    expect(renderer.render({
      sessionUpdate: 'tool_call_update', toolCallId: 'c', status: 'in_progress',
      content: [{ type: 'content', content: { type: 'text', text: 'partial' } }],
    })).toHaveLength(1);
    const huge = Array.from({ length: 5000 }, (_, i) => `line ${i} ${'x'.repeat(500)}`).join('\n');
    const events = renderer.render({
      sessionUpdate: 'tool_call_update', toolCallId: 'c', status: 'completed',
      content: [{ type: 'diff', path: '/w/b', newText: huge }],
    });
    const body = events[1]?.value ?? '';
    expect(body.length).toBeLessThan(MAX_RENDER_CHARS);
    expect(body).toContain('more diff lines not shown');
    expect(body.trimEnd().endsWith('```')).toBe(true);
  });

  it('keeps thoughts as quoted markdown only when asked', () => {
    const chunk: acp.SessionUpdate = { sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'hmm\nok' } };
    expect(new ChatUpdateRenderer().render(chunk)[0]?.kind).toBe('progress');
    expect(new ChatUpdateRenderer({ keepThoughts: true }).render(chunk)).toEqual([{ kind: 'markdown', value: '\n> hmm\n> ok\n\n' }]);
  });
});
