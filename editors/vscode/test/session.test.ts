import { describe, expect, it, vi } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';

const statusBar = vi.hoisted(() => ({
  command: undefined as string | undefined,
  dispose: vi.fn(),
  hide: vi.fn(),
  show: vi.fn(),
}));

vi.mock('vscode', () => ({
  StatusBarAlignment: { Right: 2 },
  window: {
    createStatusBarItem: vi.fn(() => statusBar),
  },
}));
vi.mock('../src/log', () => ({
  log: { info: vi.fn(), warn: vi.fn(), error: vi.fn(), debug: vi.fn() },
}));

import { AgentSession } from '../src/session';

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
