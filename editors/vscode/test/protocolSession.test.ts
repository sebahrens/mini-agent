import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import { SessionUpdateRouter } from '../src/protocolSession';

function held<T>() {
  let release!: (value: T) => void;
  const promise = new Promise<T>(resolve => { release = resolve; });
  return { promise, release };
}
function update(sessionId: string, text: string): acp.SessionNotification {
  return { sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text } } };
}

describe('public ACP conversation adapter', () => {
  it('routes saved replay and live updates only to the exact loaded identity', async () => {
    const router = new SessionUpdateRouter();
    router.begin('saved');
    router.receive(update('foreign', 'must not appear'));
    router.receive(update('saved', 'retained'));
    const pending = held<acp.PromptResponse>();
    const request = vi.fn(() => pending.promise);
    const client = { request, notify: vi.fn(async () => undefined) } as unknown as acp.ClientContext;
    const session = router.finish(client, 'saved');
    const seen = vi.fn();
    const turn = session.prompt('new literal', seen, new AbortController().signal);
    router.receive(update('foreign', 'still foreign'));
    router.receive(update('saved', 'live'));
    pending.release({ stopReason: 'end_turn' });
    await expect(turn).resolves.toBe('end_turn');
    expect(seen.mock.calls.map(call => (call[0].content as { text: string }).text)).toEqual(['retained', 'live']);
    expect(request).toHaveBeenCalledWith('session/prompt', { sessionId: 'saved', prompt: [{ type: 'text', text: 'new literal' }] });
    router.reset();
    router.receive(update('saved', 'late'));
    expect(seen).toHaveBeenCalledTimes(2);
  });

  it('keeps cancellation and close waiting for the actual prompt response', async () => {
    const router = new SessionUpdateRouter();
    router.begin('saved');
    const pending = held<acp.PromptResponse>();
    const request = vi.fn((method: string) => method === 'session/prompt' ? pending.promise : Promise.resolve({}));
    const notify = vi.fn(async () => undefined);
    const session = router.finish({ request, notify } as unknown as acp.ClientContext, 'saved');
    const controller = new AbortController();
    const turn = session.prompt('cancel me', vi.fn(), controller.signal);
    controller.abort();
    const closing = session.close();
    await Promise.resolve();
    expect(notify).toHaveBeenCalledWith('session/cancel', { sessionId: 'saved' });
    expect(request.mock.calls.filter(call => call[0] === 'session/close')).toHaveLength(0);
    pending.release({ stopReason: 'cancelled' });
    await expect(turn).resolves.toBe('cancelled');
    await closing;
    expect(request.mock.calls.filter(call => call[0] === 'session/close')).toHaveLength(1);
    router.begin('new-owner');
    router.receive(update('saved', 'retired'));
    router.finish({ request, notify } as unknown as acp.ClientContext, 'new-owner');
  });

  it('refuses identity mismatch and bounded replay overflow without attaching a conversation', () => {
    const client = {} as acp.ClientContext;
    const mismatch = new SessionUpdateRouter();
    mismatch.begin('saved');
    expect(() => mismatch.finish(client, 'other')).toThrow(/identity/);
    const large = new SessionUpdateRouter();
    large.begin('saved');
    large.receive(update('saved', 'x'.repeat(8 * 1024 * 1024)));
    expect(() => large.finish(client, 'saved')).toThrow(/bounded/);
    large.begin('fresh');
    expect(large.finish(client, 'fresh').sessionId).toBe('fresh');
  });
});
