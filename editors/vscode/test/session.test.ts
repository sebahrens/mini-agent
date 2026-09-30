import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';

const statusBar = vi.hoisted(() => ({
  command: undefined as string | undefined,
  dispose: vi.fn(),
  hide: vi.fn(),
  show: vi.fn(),
}));

const ui = vi.hoisted(() => ({
  showErrorMessage: vi.fn(),
  showWarningMessage: vi.fn(),
}));
const logMock = vi.hoisted(() => ({
  info: vi.fn(), warn: vi.fn(), error: vi.fn(), debug: vi.fn(),
}));
const spawnMock = vi.hoisted(() => vi.fn());
const trust = vi.hoisted(() => ({ isTrusted: true }));
const protocol = vi.hoisted(() => ({ connect: vi.fn() }));

vi.mock('vscode', () => ({
  StatusBarAlignment: { Right: 2 },
  workspace: { get isTrusted() { return trust.isTrusted; } },
  window: {
    createStatusBarItem: vi.fn(() => statusBar),
    showErrorMessage: ui.showErrorMessage,
    showWarningMessage: ui.showWarningMessage,
  },
}));
vi.mock('../src/log', () => ({ log: logMock }));
vi.mock('node:child_process', async () => {
  const actual = await vi.importActual<typeof import('node:child_process')>('node:child_process');
  return { ...actual, spawn: spawnMock };
});
vi.mock('@agentclientprotocol/sdk', async () => {
  const actual = await vi.importActual<typeof import('@agentclientprotocol/sdk')>('@agentclientprotocol/sdk');
  return {
    ...actual,
    client: vi.fn(() => ({ onRequest: () => ({ connect: protocol.connect }) })),
    ndJsonStream: vi.fn(() => ({})),
  };
});

import { AgentSession } from '../src/session';

class FakeProcess extends EventEmitter {
  readonly stdout = new EventEmitter();
  readonly stderr = new EventEmitter();
  exitCode: number | null = null;
  kill = vi.fn();
}

class FakeProtocolProcess extends EventEmitter {
  constructor(private readonly ignoreSigterm = false) { super(); }
  readonly stdin = new PassThrough();
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  exitCode: number | null = null;
  readonly kill = vi.fn((signal: NodeJS.Signals = 'SIGTERM') => {
    if (this.ignoreSigterm && signal === 'SIGTERM') { return true; }
    this.exitCode = 0;
    queueMicrotask(() => this.emit('exit', 0, signal));
    return true;
  });
}

function held<T>() {
  let release!: (value: T) => void;
  let fail!: (reason: Error) => void;
  const promise = new Promise<T>((resolve, reject) => { release = resolve; fail = reject; });
  return { promise, release, fail };
}

function successfulProbe(): void {
  const probe = new FakeProcess();
  spawnMock.mockImplementationOnce(() => {
    queueMicrotask(() => {
      probe.stdout.emit('data', Buffer.from('mini-agent 1.8.0\n'));
      probe.exitCode = 0;
      probe.emit('exit', 0, null);
    });
    return probe;
  });
}

function makeSession(executable = '/usr/bin/mini-agent'): AgentSession {
  const context = {
    subscriptions: [],
    extension: { packageJSON: { version: '1.8.0' } },
  } as never;
  const folder = { name: 'workspace', uri: { fsPath: '/workspace' } } as never;
  return new AgentSession(
    executable,
    folder,
    context,
    vi.fn(async (): Promise<acp.RequestPermissionResponse> => ({ outcome: { outcome: 'cancelled' } })),
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  trust.isTrusted = true;
});

describe('AgentSession resource ownership', () => {
  it('owns and disposes its status item without retaining it in extension subscriptions', () => {
    const subscriptions: unknown[] = [];
    const context = {
      subscriptions,
      extension: { packageJSON: { version: '1.8.0' } },
    } as never;
    const folder = {
      name: 'workspace',
      uri: { fsPath: '/workspace' },
    } as never;
    const session = new AgentSession(
      '/usr/bin/mini-agent',
      folder,
      context,
      vi.fn(async (): Promise<acp.RequestPermissionResponse> => ({
        outcome: { outcome: 'cancelled' },
      })),
    );

    expect(subscriptions).toHaveLength(0);
    expect(statusBar.command).toBe('mini-agent.stop');
    session.dispose();
    expect(statusBar.dispose).toHaveBeenCalledOnce();
  });
});

describe('AgentSession --version probe', () => {
  it('does not probe an executable when trust is already revoked', async () => {
    trust.isTrusted = false;
    const session = makeSession();

    await expect(session.start()).rejects.toThrow(/trusted workspace/);

    expect(spawnMock).not.toHaveBeenCalled();
    session.dispose();
  });

  it('rechecks trust immediately before the version probe', async () => {
    statusBar.show.mockImplementationOnce(() => { trust.isTrusted = false; });
    const session = makeSession();

    await expect(session.start()).rejects.toThrow(/trusted workspace/);

    expect(spawnMock).not.toHaveBeenCalled();
    expect(statusBar.hide).toHaveBeenCalled();
    session.dispose();
  });

  it('refuses ACP launch and a queued start when trust changes during version verification', async () => {
    const probe = new FakeProcess();
    spawnMock.mockReturnValueOnce(probe);
    const session = makeSession();
    const first = session.start();
    await vi.waitFor(() => expect(spawnMock).toHaveBeenCalledOnce());
    const queued = session.start();

    trust.isTrusted = false;
    probe.stdout.emit('data', Buffer.from('mini-agent 1.8.0\n'));
    probe.exitCode = 0;
    probe.emit('exit', 0, null);

    await expect(first).rejects.toThrow(/trusted workspace/);
    await expect(queued).rejects.toThrow(/trusted workspace/);
    expect(spawnMock).toHaveBeenCalledOnce();
    expect(spawnMock.mock.calls[0]?.[1]).toEqual(['--version']);
    expect(statusBar.hide).toHaveBeenCalled();
    session.dispose();
  });

  it('rechecks trust after start resolves and before creating a prompt', async () => {
    const session = makeSession();
    vi.spyOn(session, 'start').mockImplementation(async () => { trust.isTrusted = false; });

    await expect(session.prompt('hello', vi.fn(), new AbortController().signal))
      .rejects.toThrow(/trusted workspace/);

    expect(spawnMock).not.toHaveBeenCalled();
    session.dispose();
  });

  it('surfaces the probe stderr and executable path when the binary fails to start', async () => {
    const probe = new FakeProcess();
    spawnMock.mockImplementationOnce(() => {
      queueMicrotask(() => {
        probe.stderr.emit('data', Buffer.from('Error: Startup::init failed: no provider '));
        probe.stderr.emit('data', Buffer.from('API key configured\n'));
        probe.exitCode = 1;
        probe.emit('exit', 1, null);
      });
      return probe;
    });
    const session = makeSession('/opt/mini-agent');

    await expect(session.start()).rejects.toThrow(/not runnable/);

    expect(spawnMock).toHaveBeenCalledOnce();
    expect(spawnMock).toHaveBeenCalledWith('/opt/mini-agent', ['--version'], expect.anything());
    expect(ui.showErrorMessage).toHaveBeenCalledOnce();
    const message = ui.showErrorMessage.mock.calls[0]?.[0] as string;
    expect(message).toContain('"/opt/mini-agent"');
    expect(message).toContain('exit code 1');
    expect(message).toContain('Startup::init failed: no provider API key configured');
    expect(logMock.error).toHaveBeenCalledWith(expect.stringContaining('[stderr] Error: Startup::init failed'));
    session.dispose();
  });

  it('probes and launches the same resolved executable', async () => {
    const probe = new FakeProcess();
    spawnMock.mockImplementationOnce(() => {
      queueMicrotask(() => {
        probe.stdout.emit('data', Buffer.from('mini-agent 1.8.0\n'));
        probe.exitCode = 0;
        probe.emit('exit', 0, null);
      });
      return probe;
    });
    spawnMock.mockImplementationOnce(() => { throw new Error('launch stopped by test'); });
    const session = makeSession('C:\\Tools\\mini-agent.exe');

    await expect(session.start()).rejects.toThrow(/launch stopped by test/);

    expect(spawnMock).toHaveBeenCalledTimes(2);
    const [probeCall, launchCall] = spawnMock.mock.calls;
    expect(probeCall?.[0]).toBe('C:\\Tools\\mini-agent.exe');
    expect(probeCall?.[1]).toEqual(['--version']);
    expect(launchCall?.[0]).toBe(probeCall?.[0]);
    expect(launchCall?.[1]).toEqual(['--acp']);
    expect(launchCall?.[2]).toMatchObject({ cwd: '/workspace', shell: false });
    session.dispose();
  });

  it('includes the executable path in spawn errors', async () => {
    const probe = new FakeProcess();
    spawnMock.mockImplementationOnce(() => {
      queueMicrotask(() => probe.emit('error', new Error('spawn ENOENT')));
      return probe;
    });
    const session = makeSession('/missing/mini-agent');

    await expect(session.start()).rejects.toThrow(/not runnable/);
    const message = ui.showErrorMessage.mock.calls[0]?.[0] as string;
    expect(message).toContain('"/missing/mini-agent"');
    expect(message).toContain('spawn ENOENT');
    session.dispose();
  });
});

describe('AgentSession ACP trust transitions', () => {
  it('reaps the owned child when trust is revoked during ACP initialize', async () => {
    successfulProbe();
    const child = new FakeProtocolProcess();
    spawnMock.mockReturnValueOnce(child);
    const initialize = held<{ protocolVersion: number }>();
    const agent = { request: vi.fn(() => initialize.promise), buildSession: vi.fn() };
    const connection = { agent, close: vi.fn() };
    protocol.connect.mockReturnValue(connection);
    const session = makeSession();

    const starting = session.start();
    await vi.waitFor(() => expect(agent.request).toHaveBeenCalledOnce());
    trust.isTrusted = false;
    initialize.release({ protocolVersion: 1 });

    await expect(starting).rejects.toThrow(/trusted workspace/);
    expect(connection.close).toHaveBeenCalledOnce();
    expect(child.kill).toHaveBeenCalledOnce();
    expect(agent.buildSession).not.toHaveBeenCalled();
    await session.stop();
    expect(child.exitCode).toBe(0);
    expect(statusBar.hide).toHaveBeenCalled();
    session.dispose();
  });

  it.each(['initialize failure', 'trust loss'] as const)(
    'waits for forced child exit after %s before settling startup', async scenario => {
      successfulProbe();
      const child = new FakeProtocolProcess(true);
      spawnMock.mockReturnValueOnce(child);
      const initialize = held<{ protocolVersion: number }>();
      const agent = { request: vi.fn(() => initialize.promise), buildSession: vi.fn() };
      const connection = { agent, close: vi.fn() };
      protocol.connect.mockReturnValue(connection);
      const session = makeSession();
      const starting = session.start();
      await vi.waitFor(() => expect(agent.request).toHaveBeenCalledOnce());
      const queued = scenario === 'trust loss' ? session.start() : undefined;
      const queuedRejected = queued ? expect(queued).rejects.toThrow(/trusted workspace/) : undefined;
      let settled = false;
      void starting.then(() => { settled = true; }, () => { settled = true; });

      vi.useFakeTimers();
      try {
        if (scenario === 'trust loss') {
          trust.isTrusted = false;
          initialize.release({ protocolVersion: 1 });
        } else {
          initialize.fail(new Error('fixture initialize failed'));
        }
        await vi.advanceTimersByTimeAsync(4_999);
        expect(child.kill).toHaveBeenCalledExactlyOnceWith('SIGTERM');
        expect(child.exitCode).toBeNull();
        expect(settled).toBe(false);
        expect(statusBar.hide).not.toHaveBeenCalled();
        expect(spawnMock).toHaveBeenCalledTimes(2);

        await vi.advanceTimersByTimeAsync(1);
        expect(child.kill).toHaveBeenNthCalledWith(2, 'SIGKILL');
        await expect(starting).rejects.toThrow(
          scenario === 'trust loss' ? /trusted workspace/ : /fixture initialize failed/,
        );
        if (queuedRejected) { await queuedRejected; }
        expect(connection.close).toHaveBeenCalledOnce();
        expect(child.exitCode).toBe(0);
        expect(statusBar.hide).toHaveBeenCalled();
        expect(agent.buildSession).not.toHaveBeenCalled();
        expect(spawnMock).toHaveBeenCalledTimes(2);
        await session.stop();
        session.dispose();
      } finally {
        vi.useRealTimers();
      }
    },
  );

  it('disposes an in-flight ACP session and stops its child before any prompt', async () => {
    successfulProbe();
    const child = new FakeProtocolProcess();
    spawnMock.mockReturnValueOnce(child);
    const newSession = held<{ sessionId: string; dispose: ReturnType<typeof vi.fn>; prompt: ReturnType<typeof vi.fn> }>();
    const active = { sessionId: 'held-session', dispose: vi.fn(), prompt: vi.fn() };
    const startSession = vi.fn(() => newSession.promise);
    const agent = {
      request: vi.fn(async () => ({ protocolVersion: 1 })),
      buildSession: vi.fn(() => ({ start: startSession })),
    };
    const connection = { agent, close: vi.fn() };
    protocol.connect.mockReturnValue(connection);
    const session = makeSession();
    await session.start();

    const prompting = session.prompt('should not dispatch', vi.fn(), new AbortController().signal);
    await vi.waitFor(() => expect(startSession).toHaveBeenCalledOnce());
    trust.isTrusted = false;
    newSession.release(active);

    await expect(prompting).rejects.toThrow(/trusted workspace/);
    expect(active.dispose).toHaveBeenCalledOnce();
    expect(active.prompt).not.toHaveBeenCalled();
    expect(connection.close).toHaveBeenCalled();
    expect(child.kill).toHaveBeenCalled();
    await session.stop();
    expect(child.exitCode).toBe(0);
    expect(statusBar.hide).toHaveBeenCalled();
    session.dispose();
  });
});
