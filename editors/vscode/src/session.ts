import * as vscode from 'vscode';
import * as cp from 'node:child_process';
import { Readable, Writable } from 'node:stream';
import * as acp from '@agentclientprotocol/sdk';
import {
  Conversation,
  type ConversationSession,
  SerialTransitionQueue,
} from './conversation';
import {
  LineRingBuffer,
  formatStderrBlock,
  probeFailureMessage,
  unexpectedExitMessage,
  withRecentStderr,
} from './diagnostics';
import { log } from './log';
import { SessionUpdateRouter, type PublicProtocolSession } from './protocolSession';

type SessionState = 'stopped' | 'starting' | 'running' | 'stopping';

export type PermissionHandler = (
  request: acp.RequestPermissionRequest,
  signal: AbortSignal,
) => Promise<acp.RequestPermissionResponse>;

function trustedWorkspaceError(): Error {
  return new Error('Mini Agent requires a trusted workspace.');
}

function requireTrustedWorkspace(): void {
  if (!vscode.workspace.isTrusted) { throw trustedWorkspaceError(); }
}


function extensionVersion(context: vscode.ExtensionContext): string {
  const manifest = context.extension.packageJSON as Record<string, unknown>;
  const version = manifest['version'];
  return typeof version === 'string' ? version : '0.0.0';
}

/** Supervises one mini-agent ACP process and one reusable workspace conversation. */
export class AgentSession {
  private proc: cp.ChildProcessWithoutNullStreams | undefined;
  private connection: acp.ClientConnection | undefined;
  private client: acp.ClientContext | undefined;
  private readonly updates = new SessionUpdateRouter();
  private supportsLoad = false;
  private state: SessionState = 'stopped';
  private readonly transitions = new SerialTransitionQueue();
  private readonly statusBar: vscode.StatusBarItem;
  private readonly conversation: Conversation;
  /** Last stderr lines of the current child, surfaced on unexpected exit. */
  private stderr = new LineRingBuffer();

  constructor(
    private readonly executablePath: string,
    private readonly folder: vscode.WorkspaceFolder,
    private readonly context: vscode.ExtensionContext,
    private readonly requestPermission: PermissionHandler,
  ) {
    this.statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.statusBar.command = 'mini-agent.stop';
    this.conversation = new Conversation(() => this.createProtocolSession());
  }

  get workspaceFolder(): vscode.WorkspaceFolder { return this.folder; }

  async start(): Promise<void> {
    return this.transitions.run(async () => {
      if (!vscode.workspace.isTrusted) {
        await this.doStop();
        throw trustedWorkspaceError();
      }
      if (this.state !== 'running') { await this.doStart(); }
    });
  }

  async prompt(
    text: string,
    onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal,
  ): Promise<acp.StopReason> {
    await this.start();
    if (!vscode.workspace.isTrusted) {
      await this.stop();
      throw trustedWorkspaceError();
    }
    if (this.state !== 'running') { throw new Error('Mini Agent failed to start.'); }
    try {
      return await this.conversation.prompt(text, onUpdate, signal);
    } finally {
      if (!vscode.workspace.isTrusted) { await this.stop(); }
    }
  }

  async stop(): Promise<void> {
    return this.transitions.run(() => this.doStop());
  }

  /** Release UI resources. Call after stop() when discarding the session. */
  dispose(): void {
    this.statusBar.dispose();
  }

  private async doStop(): Promise<void> {
    if (this.state === 'stopped') { return; }
    this.setState('stopping');

    try {
      await this.conversation.close();
    } catch (error) {
      log.warn(`Could not close ACP session cleanly: ${errorMessage(error)}`);
    }

    this.connection?.close();
    this.connection = undefined;
    this.client = undefined;

    const proc = this.proc;
    if (proc && proc.exitCode === null) { await terminate(proc); }
    this.proc = undefined;
    this.setState('stopped');
  }

  private async doStart(): Promise<void> {
    if (this.state === 'starting') { return; }
    requireTrustedWorkspace();
    this.setState('starting');
    let verified: boolean;
    try {
      verified = await this.verify();
      requireTrustedWorkspace();
    } catch (error) {
      this.setState('stopped');
      throw error;
    }
    if (!verified) {
      this.setState('stopped');
      throw new Error('The configured Mini Agent executable is not runnable.');
    }

    const cwd = this.folder.uri.fsPath;
    if (!vscode.workspace.isTrusted) {
      this.setState('stopped');
      throw trustedWorkspaceError();
    }
    log.info(`Spawning mini-agent: ${this.executablePath} --acp (cwd: ${cwd})`);
    const proc = cp.spawn(this.executablePath, ['--acp'], {
      shell: false,
      cwd,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.proc = proc;
    const stderr = new LineRingBuffer();
    this.stderr = stderr;
    proc.once('error', error => this.onError(proc, error));
    proc.once('exit', (code, signal) => this.onExit(proc, code, signal));
    // Info, not debug: the default log level is info and a startup failure
    // (e.g. a missing provider key) is only ever reported on stderr.
    proc.stderr.on('data', (chunk: Buffer) => {
      for (const line of stderr.push(chunk)) { log.info(`[stderr] ${line}`); }
    });
    proc.stderr.once('end', () => {
      for (const line of stderr.flush()) { log.info(`[stderr] ${line}`); }
    });

    const output = Writable.toWeb(proc.stdin) as WritableStream<Uint8Array>;
    const input = Readable.toWeb(proc.stdout) as ReadableStream<Uint8Array>;
    const app = acp.client({ name: 'mini-agent-vscode' })
      .onNotification(acp.methods.client.session.update, notification => this.updates.receive(notification.params))
      .onRequest(acp.methods.client.session.requestPermission, request => (
        this.requestPermission(request.params, request.signal)
      ));
    const connection = app.connect(acp.ndJsonStream(output, input));
    this.connection = connection;
    this.client = connection.agent;

    try {
      const initialized = await connection.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: {},
        clientInfo: { name: 'mini-agent-vscode', version: extensionVersion(this.context) },
      }, { cancellationSignal: AbortSignal.timeout(10_000) });
      this.supportsLoad = initialized.agentCapabilities?.loadSession === true;
      log.info(`ACP initialized at protocol ${initialized.protocolVersion}`);
      requireTrustedWorkspace();
      this.setState('running');
    } catch (error) {
      connection.close(error);
      this.connection = undefined;
      this.client = undefined;
      this.setState('stopping');
      if (proc.exitCode === null) { await terminate(proc); }
      this.proc = undefined;
      this.setState('stopped');
      const recent = stderr.lines();
      log.error(`ACP initialize failed for ${this.executablePath}: ${errorMessage(error)}\n${formatStderrBlock(recent)}`);
      throw new Error(withRecentStderr(`ACP initialize failed: ${errorMessage(error)}.`, recent), { cause: error });
    }
  }

  private async createProtocolSession(): Promise<ConversationSession> {
    const client = this.client;
    if (!client || this.state !== 'running') { throw new Error('ACP connection is not running.'); }
    if (!vscode.workspace.isTrusted) {
      await this.stop();
      throw trustedWorkspaceError();
    }
    this.updates.begin();
    try {
      const response = await client.request(acp.methods.agent.session.new, { cwd: this.folder.uri.fsPath, mcpServers: [] },
        { cancellationSignal: AbortSignal.timeout(10_000) });
      requireTrustedWorkspace();
      const session = this.updates.finish(client, response.sessionId);
      log.info(`ACP session created: ${session.sessionId}`);
      return session;
    } catch (error) { this.updates.reset(); throw error; }
  }

  async resumeSavedSession(): Promise<void> {
    await this.start();
    return this.transitions.run(async () => {
      requireTrustedWorkspace();
      if (this.conversation.isPrompting) { throw new Error('Wait for the current prompt to settle before resuming a saved session.'); }
      const client = this.client;
      if (!client || !this.supportsLoad) { throw new Error('This agent does not support saved ACP sessions.'); }
      const response = await client.request(acp.methods.agent.session.list, { cwd: this.folder.uri.fsPath },
        { cancellationSignal: AbortSignal.timeout(10_000) });
      const items = response.sessions.slice(0, 100).map(row => ({
        label: safeSessionLabel(row.title ?? row.sessionId),
        description: row.sessionId,
        sessionId: row.sessionId,
      }));
      if (items.length === 0) { void vscode.window.showInformationMessage('No saved Mini Agent sessions exist for this workspace.'); return; }
      const selected = await vscode.window.showQuickPick(items, { placeHolder: 'Resume a saved Mini Agent session in this workspace' });
      if (!selected) { return; }
      requireTrustedWorkspace();
      if (this.conversation.isPrompting) { throw new Error('The current prompt is still active; saved session was not loaded.'); }
      await this.conversation.close();
      this.updates.begin(selected.sessionId);
      let attached: PublicProtocolSession | undefined;
      try {
        await client.request(acp.methods.agent.session.load, { sessionId: selected.sessionId, cwd: this.folder.uri.fsPath, mcpServers: [] },
          { cancellationSignal: AbortSignal.timeout(10_000) });
        requireTrustedWorkspace();
        attached = this.updates.finish(client, selected.sessionId);
        await this.conversation.adopt(attached);
        log.info(`ACP saved session resumed: ${selected.sessionId}`);
      } catch (error) {
        this.updates.reset();
        if (attached) { await attached.close().catch(() => undefined); }
        else { await client.request(acp.methods.agent.session.close, { sessionId: selected.sessionId }, { cancellationSignal: AbortSignal.timeout(5000) }).catch(() => undefined); }
        throw error;
      }
    });
  }

  private onError(proc: cp.ChildProcessWithoutNullStreams, error: Error): void {
    if (this.proc !== proc) { return; }
    log.error(`mini-agent process error (${this.executablePath}): ${error.message}`);
    void vscode.window.showErrorMessage(
      `Mini Agent process error (${this.executablePath}): ${error.message}`,
    );
    this.resetAfterExit(proc);
  }

  private onExit(
    proc: cp.ChildProcessWithoutNullStreams,
    code: number | null,
    signal: NodeJS.Signals | null,
  ): void {
    if (this.proc !== proc) { return; }
    if (this.state !== 'stopping') {
      const recent = this.stderr.lines();
      log.warn(
        `mini-agent exited unexpectedly: code=${code ?? 'null'} signal=${signal ?? 'none'}\n`
        + formatStderrBlock(recent),
      );
      void vscode.window.showWarningMessage(unexpectedExitMessage(code, signal, recent));
    }
    this.resetAfterExit(proc);
  }

  private resetAfterExit(proc: cp.ChildProcessWithoutNullStreams): void {
    if (this.proc !== proc) { return; }
    this.connection?.close();
    this.connection = undefined;
    this.client = undefined;
    this.proc = undefined;
    this.updates.reset();
    this.conversation.invalidate();
    this.setState('stopped');
  }

  private async verify(): Promise<boolean> {
    return new Promise(resolve => {
      requireTrustedWorkspace();
      const probe = cp.spawn(this.executablePath, ['--version'], {
        shell: false,
        timeout: 5000,
      });
      let settled = false;
      let stdout = '';
      const stderr = new LineRingBuffer();
      const finish = (result: boolean): void => {
        if (settled) { return; }
        settled = true;
        resolve(result);
      };
      probe.stdout?.on('data', (data: Buffer) => { stdout += data.toString(); });
      probe.stderr?.on('data', (data: Buffer) => { stderr.push(data); });
      probe.once('error', error => {
        log.error(`mini-agent --version failed (${this.executablePath}): ${error.message}`);
        void vscode.window.showErrorMessage(
          `Cannot start Mini Agent at "${this.executablePath}": ${error.message}`,
        );
        finish(false);
      });
      probe.once('exit', (code, signal) => {
        stderr.flush();
        if (code !== 0) {
          const message = probeFailureMessage(this.executablePath, code, signal, stderr.lines());
          log.error(`${message}\n${formatStderrBlock(stderr.lines())}`);
          void vscode.window.showErrorMessage(message);
          finish(false);
          return;
        }
        log.info(`mini-agent version: ${stdout.trim()}`);
        finish(true);
      });
    });
  }

  private setState(state: SessionState): void {
    this.state = state;
    switch (state) {
      case 'stopped':
        this.statusBar.hide();
        break;
      case 'starting':
        this.statusBar.text = '$(loading~spin) Mini Agent';
        this.statusBar.tooltip = 'Mini Agent starting…';
        this.statusBar.show();
        break;
      case 'running':
        this.statusBar.text = '$(circle-filled) Mini Agent';
        this.statusBar.tooltip = `Mini Agent running (${this.folder.name}) — click to stop`;
        this.statusBar.show();
        break;
      case 'stopping':
        this.statusBar.text = '$(loading~spin) Mini Agent';
        this.statusBar.tooltip = 'Mini Agent stopping…';
        break;
    }
  }
}


function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function terminate(proc: cp.ChildProcess): Promise<void> {
  await new Promise<void>(resolve => {
    let settled = false;
    const finish = (): void => {
      if (settled) { return; }
      settled = true;
      clearTimeout(timer);
      clearTimeout(giveUpTimer);
      resolve();
    };
    const timer = setTimeout(() => {
      log.warn('mini-agent did not exit in time; forcing termination');
      proc.kill('SIGKILL');
    }, 5000);
    const giveUpTimer = setTimeout(finish, 7000);
    proc.once('exit', finish);
    proc.kill('SIGTERM');
    if (proc.exitCode !== null) { finish(); }
  });
}

function safeSessionLabel(value: string): string {
  return Array.from(value).slice(0, 200).map(character => {
    const code = character.codePointAt(0) ?? 0;
    return code <= 0x1f || (code >= 0x7f && code <= 0x9f) || code === 0x61c
      || code === 0x200e || code === 0x200f || (code >= 0x202a && code <= 0x202e)
      || (code >= 0x2066 && code <= 0x2069) ? '?' : character;
  }).join('').replace(/\$\(/gu, '$ (');
}
