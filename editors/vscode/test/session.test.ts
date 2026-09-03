import { EventEmitter } from 'node:events';
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

vi.mock('vscode', () => ({
  StatusBarAlignment: { Right: 2 },
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

import { AgentSession } from '../src/session';

class FakeProcess extends EventEmitter {
  readonly stdout = new EventEmitter();
  readonly stderr = new EventEmitter();
  exitCode: number | null = null;
  kill = vi.fn();
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
