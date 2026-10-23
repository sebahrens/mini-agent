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
function replayWithBytes(bytes: number): acp.SessionNotification {
  const envelopeBytes = Buffer.byteLength(JSON.stringify(update('saved', '')), 'utf8');
  const notification = update('saved', 'x'.repeat(bytes - envelopeBytes));
  expect(Buffer.byteLength(JSON.stringify(notification), 'utf8')).toBe(bytes);
  return notification;
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

  it('refuses identity mismatch without attaching a conversation', () => {
    const client = {} as acp.ClientContext;
    const mismatch = new SessionUpdateRouter();
    mismatch.begin('saved');
    expect(() => mismatch.finish(client, 'other')).toThrow(/identity/);
  });

  it.each([48 * 1024 * 1024 - 1, 48 * 1024 * 1024])('replays a %i-byte notification including its ACP envelope', async bytes => {
    const router = new SessionUpdateRouter();
    router.begin('saved');
    const notification = replayWithBytes(bytes);
    router.receive(notification);
    const client = { request: vi.fn(async () => ({ stopReason: 'end_turn' })) } as unknown as acp.ClientContext;
    const session = router.finish(client, 'saved');
    const seen = vi.fn();
    await expect(session.prompt('continue', seen, new AbortController().signal)).resolves.toBe('end_turn');
    expect(seen).toHaveBeenCalledExactlyOnceWith(notification.update);
  });

  it('refuses replay one byte above 48 MiB including its ACP envelope without attaching a conversation', () => {
    const client = {} as acp.ClientContext;
    const large = new SessionUpdateRouter();
    large.begin('saved');
    large.receive(replayWithBytes(48 * 1024 * 1024 + 1));
    expect(() => large.finish(client, 'saved')).toThrow('The saved ACP conversation exceeded its bounded replay or identity contract.');
    large.begin('fresh');
    expect(large.finish(client, 'fresh').sessionId).toBe('fresh');
  });

  it.each([100_000, 100_001])('retains the independent 100000-update limit for %i updates', async count => {
    const router = new SessionUpdateRouter();
    router.begin('saved');
    const notification = update('saved', 'small');
    for (let index = 0; index < count; index++) { router.receive(notification); }
    const client = { request: vi.fn(async () => ({ stopReason: 'end_turn' })) } as unknown as acp.ClientContext;
    if (count > 100_000) {
      expect(() => router.finish(client, 'saved')).toThrow(/bounded replay or identity contract/);
    } else {
      const session = router.finish(client, 'saved');
      let replayed = 0;
      await session.prompt('continue', () => { replayed++; }, new AbortController().signal);
      expect(replayed).toBe(count);
    }
  });
});
