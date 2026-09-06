import { beforeEach, describe, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  gatedFolderPick: vi.fn(),
  showQuickPick: vi.fn(),
  instances: [] as Array<{
    workspaceFolder: unknown;
    start: ReturnType<typeof vi.fn>;
    stop: ReturnType<typeof vi.fn>;
    dispose: ReturnType<typeof vi.fn>;
  }>,
}));

vi.mock('vscode', () => ({
  workspace: {
    isTrusted: true,
    getConfiguration: vi.fn(() => ({
      get: vi.fn((_name: string, fallback: unknown) => fallback),
      inspect: vi.fn(() => ({ globalValue: '/usr/bin/mini-agent' })),
    })),
    onDidChangeConfiguration: vi.fn(() => ({ dispose: vi.fn() })),
    onDidChangeWorkspaceFolders: vi.fn(() => ({ dispose: vi.fn() })),
  },
  window: {
    showWarningMessage: vi.fn(),
    showQuickPick: state.showQuickPick,
    showInformationMessage: vi.fn(),
    showErrorMessage: vi.fn(),
  },
  commands: {
    registerCommand: vi.fn(() => ({ dispose: vi.fn() })),
  },
  chat: {
    createChatParticipant: vi.fn(() => ({ dispose: vi.fn() })),
  },
  Uri: {
    joinPath: vi.fn(() => ({ fsPath: '/bundled/mini-agent' })),
  },
  ThemeIcon: class ThemeIcon {},
  CancellationTokenSource: class CancellationTokenSource {
    private cancelled = false;
    private listeners: Array<() => void> = [];
    readonly token: {
      readonly isCancellationRequested: boolean;
      onCancellationRequested: (listener: () => void) => { dispose: ReturnType<typeof vi.fn> };
    };
    constructor() {
      const source = this;
      this.token = {
        get isCancellationRequested() { return source.cancelled; },
        onCancellationRequested: (listener: () => void) => {
          this.listeners.push(listener);
          return { dispose: vi.fn() };
        },
      };
    }
    cancel(): void {
      this.cancelled = true;
      for (const listener of this.listeners) { listener(); }
    }
    dispose(): void { this.listeners = []; }
  },
}));

vi.mock('../src/chat', () => ({
  ChatUpdateRenderer: class ChatUpdateRenderer {},
  stopResult: vi.fn(),
}));
vi.mock('../src/config', () => ({
  ensureConfigFile: vi.fn(),
  resolveConfigDirectory: vi.fn(),
}));
vi.mock('../src/log', () => ({
  log: { channel: { dispose: vi.fn() }, info: vi.fn(), error: vi.fn() },
  setLogLevel: vi.fn(),
  showOutput: vi.fn(),
}));
vi.mock('../src/trust', () => ({
  assertExecutableScope: vi.fn(),
  gatedFolderPick: state.gatedFolderPick,
}));
vi.mock('../src/session', () => ({
  AgentSession: class AgentSession {
    readonly start = vi.fn(async () => undefined);
    readonly stop = vi.fn(async () => undefined);
    readonly dispose = vi.fn();
    readonly workspaceFolder: unknown;

    constructor(_executable: string, folder: unknown) {
      this.workspaceFolder = folder;
      state.instances.push(this);
    }
  },
}));

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(finish => { resolve = finish; });
  return { promise, resolve };
}

const context = {
  extensionUri: {},
  subscriptions: [],
  extension: { packageJSON: { version: '1.8.0' } },
} as never;
const folder = {
  name: 'workspace',
  uri: { fsPath: '/workspace', scheme: 'file', toString: () => 'file:///workspace' },
};

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  state.instances.length = 0;
});

describe('extension session creation', () => {
  it('shares one creation between a chat request and the start command', async () => {
    const pick = deferred<typeof folder>();
    state.gatedFolderPick.mockReturnValueOnce(pick.promise);
    const extension = await import('../src/extension');

    const chatCreation = extension.ensureSession(context);
    const commandCreation = extension.cmdStart(context);
    expect(state.gatedFolderPick).toHaveBeenCalledOnce();

    pick.resolve(folder);
    const active = await chatCreation;
    await commandCreation;

    expect(state.instances).toHaveLength(1);
    expect(active).toBe(state.instances[0]);
    expect(state.instances[0]?.start).toHaveBeenCalledOnce();
  });

  it('invalidates a folder pick that resolves after discard', async () => {
    const pick = deferred<typeof folder>();
    state.gatedFolderPick.mockReturnValueOnce(pick.promise);
    const extension = await import('../src/extension');

    const creation = extension.ensureSession(context);
    await extension.discardSession();
    pick.resolve(folder);

    await expect(creation).resolves.toBeUndefined();
    expect(state.instances).toHaveLength(0);
  });

  it('disposes the session even when stopping it fails', async () => {
    state.gatedFolderPick.mockResolvedValueOnce(folder);
    const extension = await import('../src/extension');
    await extension.ensureSession(context);
    const active = state.instances[0];
    active?.stop.mockRejectedValueOnce(new Error('stop failed'));

    await expect(extension.discardSession()).rejects.toThrow('stop failed');
    expect(active?.dispose).toHaveBeenCalledOnce();
  });
});

describe('permission cancellation', () => {
  const request = {
    sessionId: 'session-1',
    toolCall: { toolCallId: 'call-1', title: 'bash', rawInput: 'cargo test' },
    options: [
      { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
      { optionId: 'deny', name: 'Deny', kind: 'reject_once' },
    ],
  } as never;

  it('resolves cancelled and cancels the picker when the turn aborts', async () => {
    state.showQuickPick.mockReturnValueOnce(new Promise(() => undefined));
    const extension = await import('../src/extension');
    const cancellation = new AbortController();
    const result = extension.requestPermission(request, cancellation.signal);

    cancellation.abort();

    await expect(result).resolves.toEqual({ outcome: { outcome: 'cancelled' } });
    expect(state.showQuickPick).toHaveBeenCalledOnce();
    const token = state.showQuickPick.mock.calls[0]?.[2];
    expect(token?.isCancellationRequested).toBe(true);
  });
});
