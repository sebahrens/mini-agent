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
      value: 'Tool call-7: Read Cargo.toml (completed)',
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
