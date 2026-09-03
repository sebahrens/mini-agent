import * as vscode from 'vscode';
import * as cp from 'node:child_process';
import { Readable, Writable } from 'node:stream';
import * as acp from '@agentclientprotocol/sdk';
import {
  Conversation,
  type ConversationSession,
  SerialTransitionQueue,
  SessionUnavailableError,
} from './conversation';
import {
  LineRingBuffer,
  formatStderrBlock,
  probeFailureMessage,
  unexpectedExitMessage,
  withRecentStderr,
} from './diagnostics';
import { log } from './log';

type SessionState = 'stopped' | 'starting' | 'running' | 'stopping';

export type PermissionHandler = (
  request: acp.RequestPermissionRequest,
  signal: AbortSignal,
) => Promise<acp.RequestPermissionResponse>;

class AcpProtocolSession implements ConversationSession {
  private prompting = false;

  constructor(
    private readonly session: acp.ActiveSession,
    private readonly client: acp.ClientContext,
  ) {}

  async prompt(
    text: string,
    onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal,
  ): Promise<acp.StopReason> {
    if (signal.aborted) { return 'cancelled'; }
    this.prompting = true;
    let cancellationSent = false;
    const cancel = (): void => {
      if (cancellationSent) { return; }
      cancellationSent = true;
      void this.client.notify(acp.methods.agent.session.cancel, {
        sessionId: this.session.sessionId,
      }).catch(error => log.warn(`ACP cancellation failed: ${errorMessage(error)}`));
    };
    signal.addEventListener('abort', cancel, { once: true });

    try {
      // ActiveSession forwards typed updates and queues the final prompt response.
      void this.session.prompt(text, { cancellationSignal: signal }).catch(() => undefined);
      for (;;) {
        const message = await this.session.nextUpdate();
        if (message.kind === 'stop') { return message.stopReason; }
        onUpdate(message.update);
      }
    } catch (error) {
      if (isUnknownSession(error)) { throw new SessionUnavailableError(); }
      throw error;
    } finally {
      signal.removeEventListener('abort', cancel);
      this.prompting = false;
    }
  }

  async close(): Promise<void> {
    if (this.prompting) {
      try {
        await this.client.notify(acp.methods.agent.session.cancel, {
          sessionId: this.session.sessionId,
        });
      } catch (error) {
        log.warn(`ACP cancellation during close failed: ${errorMessage(error)}`);
      }
    }
    try {
      await this.client.request(acp.methods.agent.session.close, {
        sessionId: this.session.sessionId,
      }, { cancellationSignal: AbortSignal.timeout(5000) });
    } finally {
      this.session.dispose();
    }
  }
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
      if (this.state !== 'running') { await this.doStart(); }
    });
  }

  async prompt(
    text: string,
    onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal,
  ): Promise<acp.StopReason> {
    await this.start();
    if (this.state !== 'running') { throw new Error('Mini Agent failed to start.'); }
    return this.conversation.prompt(text, onUpdate, signal);
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
    this.setState('starting');
    if (!await this.verify()) {
      this.setState('stopped');
      throw new Error('The configured Mini Agent executable is not runnable.');
    }

    const cwd = this.folder.uri.fsPath;
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
      log.info(`ACP initialized at protocol ${initialized.protocolVersion}`);
      this.setState('running');
    } catch (error) {
      connection.close(error);
      if (proc.exitCode === null) { proc.kill(); }
      this.proc = undefined;
      this.connection = undefined;
      this.client = undefined;
      this.setState('stopped');
      const recent = stderr.lines();
      log.error(`ACP initialize failed for ${this.executablePath}: ${errorMessage(error)}\n${formatStderrBlock(recent)}`);
      throw new Error(withRecentStderr(`ACP initialize failed: ${errorMessage(error)}.`, recent), { cause: error });
    }
  }

  private async createProtocolSession(): Promise<ConversationSession> {
    const client = this.client;
    if (!client || this.state !== 'running') { throw new Error('ACP connection is not running.'); }
    const session = await client.buildSession(this.folder.uri.fsPath).start({
      cancellationSignal: AbortSignal.timeout(10_000),
    });
    log.info(`ACP session created: ${session.sessionId}`);
    return new AcpProtocolSession(session, client);
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
    this.conversation.invalidate();
    this.setState('stopped');
  }

  private async verify(): Promise<boolean> {
    return new Promise(resolve => {
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

function isUnknownSession(error: unknown): boolean {
  return error instanceof acp.RequestError
    && error.code === -32602
    && error.message.includes('unknown ACP session');
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
