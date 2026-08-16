import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import {
  Conversation,
  SerialTransitionQueue,
  type ConversationSession,
  SessionUnavailableError,
} from '../src/conversation';

function fakeSession(results: Array<acp.StopReason | Error>): ConversationSession {
  return {
    prompt: vi.fn(async () => {
      const result = results.shift() ?? 'end_turn';
      if (result instanceof Error) { throw result; }
      return result;
    }),
    close: vi.fn(async () => undefined),
  };
}

describe('Conversation', () => {
  it('creates one session for the first prompt and reuses it for subsequent prompts', async () => {
    const session = fakeSession(['end_turn', 'end_turn']);
    const create = vi.fn(async () => session);
    const conversation = new Conversation(create);

    await conversation.prompt('first', () => undefined, new AbortController().signal);
    await conversation.prompt('second', () => undefined, new AbortController().signal);

    expect(create).toHaveBeenCalledTimes(1);
    expect(session.prompt).toHaveBeenCalledTimes(2);
  });

  it('recreates a closed session and retries the prompt once', async () => {
    const closed = fakeSession([new SessionUnavailableError()]);
    const replacement = fakeSession(['end_turn']);
    const create = vi.fn()
      .mockResolvedValueOnce(closed)
      .mockResolvedValueOnce(replacement);
    const conversation = new Conversation(create);

    await expect(conversation.prompt('retry me', () => undefined, new AbortController().signal))
      .resolves.toBe('end_turn');
    expect(create).toHaveBeenCalledTimes(2);
    expect(closed.close).toHaveBeenCalledOnce();
  });

  it('does not create a session when cancellation wins the start race', async () => {
    const create = vi.fn(async () => fakeSession(['end_turn']));
    const conversation = new Conversation(create);
    const controller = new AbortController();
    controller.abort();

    await expect(conversation.prompt('cancelled', () => undefined, controller.signal))
      .resolves.toBe('cancelled');
    expect(create).not.toHaveBeenCalled();
  });

  it('forwards cancellation that arrives during an active prompt', async () => {
    let markPromptStarted: (() => void) | undefined;
    const promptStarted = new Promise<void>(resolve => { markPromptStarted = resolve; });
    const session: ConversationSession = {
      prompt: vi.fn(async (_text, _onUpdate, signal) => new Promise<acp.StopReason>(resolve => {
        markPromptStarted?.();
        signal.addEventListener('abort', () => resolve('cancelled'), { once: true });
      })),
      close: vi.fn(async () => undefined),
    };
    const conversation = new Conversation(async () => session);
    const controller = new AbortController();
    const prompt = conversation.prompt('cancel me', () => undefined, controller.signal);

    await promptStarted;
    controller.abort();

    await expect(prompt).resolves.toBe('cancelled');
    expect(session.prompt).toHaveBeenCalledOnce();
  });

  it('does not prompt when cancellation arrives while a session is being created', async () => {
    const session = fakeSession(['end_turn']);
    let finishCreation: (() => void) | undefined;
    const creationBlocked = new Promise<void>(resolve => { finishCreation = resolve; });
    const conversation = new Conversation(async () => {
      await creationBlocked;
      return session;
    });
    const controller = new AbortController();
    const prompt = conversation.prompt('cancel while starting', () => undefined, controller.signal);

    controller.abort();
    finishCreation?.();

    await expect(prompt).resolves.toBe('cancelled');
    expect(session.prompt).not.toHaveBeenCalled();
  });

  it('closes the active session and creates a fresh one on the next prompt', async () => {
    const first = fakeSession(['end_turn']);
    const second = fakeSession(['end_turn']);
    const create = vi.fn()
      .mockResolvedValueOnce(first)
      .mockResolvedValueOnce(second);
    const conversation = new Conversation(create);

    await conversation.prompt('first', () => undefined, new AbortController().signal);
    await conversation.close();
    await conversation.prompt('second', () => undefined, new AbortController().signal);

    expect(first.close).toHaveBeenCalledOnce();
    expect(create).toHaveBeenCalledTimes(2);
  });

  it('keeps separate workspace conversations isolated', async () => {
    const firstWorkspace = fakeSession(['end_turn']);
    const secondWorkspace = fakeSession(['end_turn']);
    const first = new Conversation(async () => firstWorkspace);
    const second = new Conversation(async () => secondWorkspace);

    await Promise.all([
      first.prompt('workspace one', () => undefined, new AbortController().signal),
      second.prompt('workspace two', () => undefined, new AbortController().signal),
    ]);

    expect(firstWorkspace.prompt).toHaveBeenCalledWith(
      'workspace one', expect.any(Function), expect.any(AbortSignal),
    );
    expect(secondWorkspace.prompt).toHaveBeenCalledWith(
      'workspace two', expect.any(Function), expect.any(AbortSignal),
    );
  });
});

describe('SerialTransitionQueue', () => {
  it('runs transitions in call order even when the first is still pending', async () => {
    const queue = new SerialTransitionQueue();
    const events: string[] = [];
    let release!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; });

    const start = queue.run(async () => {
      events.push('start-begin');
      await gate;
      events.push('start-end');
    });
    const stop = queue.run(async () => { events.push('stop'); });

    await Promise.resolve();
    expect(events).toEqual(['start-begin']);
    release();
    await Promise.all([start, stop]);
    expect(events).toEqual(['start-begin', 'start-end', 'stop']);
  });

  it('continues after a failed transition', async () => {
    const queue = new SerialTransitionQueue();
    await expect(queue.run(async () => { throw new Error('startup failed'); })).rejects.toThrow('startup failed');
    let stopped = false;
    await queue.run(async () => { stopped = true; });
    expect(stopped).toBe(true);
  });
});
