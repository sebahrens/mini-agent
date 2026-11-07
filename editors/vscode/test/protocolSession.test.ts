import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import { MAX_COMMANDS, SessionUpdateRouter } from '../src/protocolSession';
import { COMMANDS_PAGE_SIZE, describeServerCommands } from '../src/commandList';

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

describe('permission modes, commands and content-block prompts', () => {
  const modes: acp.SessionModeState = {
    currentModeId: 'default',
    availableModes: [{ id: 'default', name: 'default' }, { id: 'plan', name: 'plan' }],
  };

  it('sends content blocks verbatim and tracks mode and command updates', async () => {
    const router = new SessionUpdateRouter();
    router.begin();
    const request = vi.fn(async () => ({ stopReason: 'end_turn' }));
    const session = router.finish({ request, notify: vi.fn() } as unknown as acp.ClientContext, 's1', [], modes);
    router.receive({ sessionId: 's1', update: { sessionUpdate: 'current_mode_update', currentModeId: 'plan' } });
    router.receive({ sessionId: 's1', update: { sessionUpdate: 'available_commands_update', availableCommands: [{ name: 'compact', description: 'Compact' }] } });
    expect(session.modes()?.currentModeId).toBe('plan');
    expect(session.commands().map(command => command.name)).toEqual(['compact']);
    const blocks: acp.ContentBlock[] = [{ type: 'text', text: 'hi' }, { type: 'resource_link', name: 'a', uri: 'file:///w/a' }];
    await session.prompt(blocks, vi.fn(), new AbortController().signal);
    expect(request).toHaveBeenCalledWith('session/prompt', { sessionId: 's1', prompt: blocks });
  });

  it('switches mode only to an advertised id', async () => {
    const router = new SessionUpdateRouter();
    router.begin();
    const request = vi.fn(async () => ({}));
    const session = router.finish({ request, notify: vi.fn() } as unknown as acp.ClientContext, 's1', [], modes);
    await expect(session.setMode('bypass')).rejects.toThrow(/does not offer/);
    expect(request).not.toHaveBeenCalled();
    await session.setMode('plan');
    expect(request).toHaveBeenCalledWith('session/set_mode', { sessionId: 's1', modeId: 'plan' }, expect.anything());
    expect(session.modes()?.currentModeId).toBe('plan');
  });
});

describe('long command lists', () => {
  const advertised = (count: number): acp.AvailableCommand[] =>
    Array.from({ length: count }, (_, index) => ({ name: `cmd${index}`, description: `Command ${index}` }));

  it('lists 150 advertised commands in pages and says how many are not shown', () => {
    const first = describeServerCommands(advertised(150));
    expect(first.match(/^- `\//gmu)).toHaveLength(COMMANDS_PAGE_SIZE);
    expect(first).toContain('Showing commands 1-100 of 150; 50 more commands not shown on this page.');
    expect(first).toContain('`/commands 2`');
    expect(first).not.toContain('cmd100`');
    const second = describeServerCommands(advertised(150), 0, '2');
    expect(second.match(/^- `\//gmu)).toHaveLength(50);
    expect(second).toContain('/cmd149');
    expect(second).toContain('Showing commands 101-150 of 150');
    expect(describeServerCommands(advertised(150), 0, '99')).toContain('Showing commands 101-150');
    expect(describeServerCommands(advertised(150), 0, 'junk')).toContain('Showing commands 1-100');
  });

  // mini-agent-oow6z: the listed name is the name /run accepts.
  it('lists namespaced command names verbatim', () => {
    const listed = describeServerCommands([{ name: 'ns:name', description: 'd' }, { name: 'a.b', description: 'd' }]);
    expect(listed).toContain('`/ns:name`');
    expect(listed).toContain('`/a.b`');
    expect(describeServerCommands([{ name: 'bad\u0007name', description: 'd' }])).toContain('`/bad?name`');
  });

  it('neutralises markdown in a command name so it cannot break out of the code span', () => {
    const listed = describeServerCommands([{ name: 'x`[a](http://evil)', description: 'd' }, { name: 'caf\u00e9-\u65e5\u672c', description: 'd' }]);
    expect(listed).toContain('`/x??a??http:??evil?`');
    expect(listed).not.toContain('](http');
    expect(listed).toContain('`/caf\u00e9-\u65e5\u672c`');
  });

  it('stays silent for a list that fits and reports commands the session could not keep', () => {
    expect(describeServerCommands(advertised(5))).not.toContain('not shown');
    expect(describeServerCommands(advertised(5))).not.toContain('could not be kept');
    expect(describeServerCommands(advertised(256), 44)).toContain('44 more commands advertised by the server could not be kept');
  });

  it('counts commands beyond the storage limit instead of dropping them silently', () => {
    const router = new SessionUpdateRouter();
    router.begin();
    const session = router.finish({ request: vi.fn(), notify: vi.fn() } as unknown as acp.ClientContext, 's1', []);
    router.receive({ sessionId: 's1', update: { sessionUpdate: 'available_commands_update', availableCommands: advertised(MAX_COMMANDS + 44) } });
    expect(session.commands()).toHaveLength(MAX_COMMANDS);
    expect(session.droppedCommands()).toBe(44);
  });
});
