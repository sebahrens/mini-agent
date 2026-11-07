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
import { EFFORT_CONFIG_ID, MODEL_CONFIG_ID, PERSONA_CONFIG_ID, personaSelector } from './persona';
import { clientCapabilities, readTextFile, writeTextFile } from './clientFs';
import { clientFileAccessEnabled, vscodeClientFsHost } from './vscodeClientFs';
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
/** Matches the Rust session catalogue cap (MAX_ROWS). */
const MAX_RESUME_ROWS = 100;

type ResumeItem = vscode.QuickPickItem & { sessionId?: string; enterSessionId?: boolean };
const ENTER_SESSION_ID_LABEL = 'Enter session id\u2026';

/** What `session/list` reports about the part of the catalogue it did not return. */
export interface CatalogueMeta { truncated: boolean; skipped: number }

/** Fold one page's `_meta` (`catalogue_truncated`, `skipped_entries`) into the running total. */
export function mergeCatalogueMeta(
  into: CatalogueMeta,
  meta: { [key: string]: unknown } | null | undefined,
): CatalogueMeta {
  const skipped = meta?.['skipped_entries'];
  return {
    truncated: into.truncated || meta?.['catalogue_truncated'] === true,
    skipped: typeof skipped === 'number' && Number.isFinite(skipped) && skipped > into.skipped ? Math.floor(skipped) : into.skipped,
  };
}

/** The same wording as the Rust `Catalogue::notice`, minus the CLI-only `--session` hint. */
export function catalogueNotice(meta: CatalogueMeta): string | undefined {
  if (!meta.truncated && meta.skipped <= 0) { return undefined; }
  const parts: string[] = [];
  if (meta.truncated) {
    parts.push(`Older saved sessions exist but are not listed: the listing shows at most ${MAX_RESUME_ROWS} of the newest sessions within a bounded scan budget. Choose "${ENTER_SESSION_ID_LABEL}" to load any session directly by its id; sessions are indexed as they are listed, so repeated listings reach further back.`);
  }
  if (meta.skipped > 0) { parts.push(`${meta.skipped} unsafe, invalid or oversized entries were skipped.`); }
  return parts.join(' ');
}

function validateSessionId(value: string): string | undefined {
  const id = value.trim();
  if (!id) { return 'Enter a session id.'; }
  if (id.length > 200 || [...id].some(ch => ch.charCodeAt(0) < 0x20 || ch.charCodeAt(0) === 0x7f)) { return 'Not a valid session id.'; }
  return undefined;
}

export class AgentSession {
  private proc: cp.ChildProcessWithoutNullStreams | undefined;
  private connection: acp.ClientConnection | undefined;
  private client: acp.ClientContext | undefined;
  private readonly updates = new SessionUpdateRouter();
  private supportsLoad = false;
  private embeddedContext = false;
  private imageContent = false;
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
    // Server-started turns (for example /cron) have no prompt to render into;
    // surface them in the output channel as they happen. The chat also shows
    // them with the next reply.
    this.updates.reportIdle = (_id, update) => {
      if (update.sessionUpdate === 'agent_message_chunk' && update.content.type === 'text') {
        log.info(`Server-started turn output: ${update.content.text}`);
      } else if (update.sessionUpdate === 'tool_call') {
        log.info(`Server-started turn tool call: ${update.title}`);
      }
    };
  }

  get workspaceFolder(): vscode.WorkspaceFolder { return this.folder; }

  /** Whether the running agent accepts embedded resource blocks in prompts. */
  get acceptsEmbeddedContext(): boolean { return this.embeddedContext; }
  get acceptsImages(): boolean { return this.imageContent; }

  /** Slash commands the agent advertised (empty until it has sent them). */
  async availableCommands(): Promise<readonly acp.AvailableCommand[]> {
    await this.start();
    const active = await this.conversation.current();
    const commands = active.commands?.() ?? [];
    this.commandsDropped = active.droppedCommands?.() ?? 0;
    if (this.commandsDropped > 0 && this.commandsDropped !== this.commandsDroppedLogged) {
      this.commandsDroppedLogged = this.commandsDropped;
      log.warn(`The Mini Agent server advertised ${commands.length + this.commandsDropped} slash commands; only the first ${commands.length} are kept, so ${this.commandsDropped} cannot be listed or run from this extension.`);
    }
    return commands;
  }

  private commandsDropped = 0;
  private commandsDroppedLogged = 0;

  /** Advertised commands the session could not keep, as of the last `availableCommands()` call. */
  droppedCommands(): number { return this.commandsDropped; }

  /** Pick the permission mode (the agent's ACP session modes) for the workspace conversation. */
  async selectMode(): Promise<void> {
    await this.start();
    return this.transitions.run(async () => {
      requireTrustedWorkspace();
      if (this.state !== 'running') { throw new Error('Mini Agent failed to start.'); }
      if (this.conversation.isPrompting) { throw new Error('Wait for the current prompt to settle before switching permission mode.'); }
      const active = await this.conversation.current();
      const modes = active.modes?.();
      if (!modes || !active.setMode || modes.availableModes.length === 0) {
        void vscode.window.showInformationMessage('This Mini Agent session offers no permission modes.');
        return;
      }
      const items = modes.availableModes.slice(0, 64).map(mode => ({
        label: safeSessionLabel(mode.name),
        description: safeSessionLabel(mode.id === modes.currentModeId ? `${mode.id} (current)` : mode.id),
        ...(mode.description ? { detail: safeSessionLabel(mode.description) } : {}),
        modeId: mode.id,
      }));
      const picked = await vscode.window.showQuickPick(items, { placeHolder: 'Select the Mini Agent permission mode' });
      if (!picked || picked.modeId === modes.currentModeId) { return; }
      requireTrustedWorkspace();
      if (this.conversation.isPrompting) { throw new Error('A prompt started meanwhile; the permission mode was not changed.'); }
      await active.setMode(picked.modeId);
      log.info(`ACP permission mode selected: ${picked.modeId}`);
      void vscode.window.showInformationMessage(`Mini Agent permission mode: ${picked.label}.`);
    });
  }

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
    text: string | readonly acp.ContentBlock[],
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
    // Editor-backed file access is opt-in (machine scope) and closed by default.
    const clientFiles = clientFileAccessEnabled();
    const fsContext = () => ({
      workspaceRoot: cwd, trusted: vscode.workspace.isTrusted, host: vscodeClientFsHost,
    });
    let app = acp.client({ name: 'mini-agent-vscode' })
      .onNotification(acp.methods.client.session.update, notification => this.updates.receive(notification.params))
      .onRequest(acp.methods.client.session.requestPermission, request => (
        this.requestPermission(request.params, request.signal)
      ));
    if (clientFiles) {
      app = app
        .onRequest(acp.methods.client.fs.readTextFile, request => readTextFile(fsContext(), request.params))
        .onRequest(acp.methods.client.fs.writeTextFile, request => writeTextFile(fsContext(), request.params));
    }
    const connection = app.connect(acp.ndJsonStream(output, input));
    this.connection = connection;
    this.client = connection.agent;

    try {
      const initialized = await connection.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: clientCapabilities(clientFiles),
        clientInfo: { name: 'mini-agent-vscode', version: extensionVersion(this.context) },
      }, { cancellationSignal: AbortSignal.timeout(10_000) });
      this.supportsLoad = initialized.agentCapabilities?.loadSession === true;
      this.embeddedContext = initialized.agentCapabilities?.promptCapabilities?.embeddedContext === true;
      this.imageContent = initialized.agentCapabilities?.promptCapabilities?.image === true;
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
      const session = this.updates.finish(client, response.sessionId, response.configOptions ?? [], response.modes);
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
      const rows: { sessionId: string; title?: string | null }[] = [];
      const seenCursors = new Set<string>();
      let cursor: string | null | undefined;
      let listMeta: CatalogueMeta = { truncated: false, skipped: 0 };
      // Follow nextCursor until exhausted, bounded by the catalogue's 100-row cap and a page limit.
      for (let page = 0; page < 50 && rows.length < MAX_RESUME_ROWS; page += 1) {
        const response = await client.request(acp.methods.agent.session.list,
          cursor ? { cwd: this.folder.uri.fsPath, cursor } : { cwd: this.folder.uri.fsPath },
          { cancellationSignal: AbortSignal.timeout(10_000) });
        rows.push(...response.sessions);
        listMeta = mergeCatalogueMeta(listMeta, response._meta);
        cursor = response.nextCursor;
        if (!cursor || seenCursors.has(cursor)) { break; }
        seenCursors.add(cursor);
      }
      // The agent caps the catalogue and skips unreadable entries; say so instead of
      // presenting a complete-looking list, and offer a way to load any session by id.
      const notice = catalogueNotice(listMeta);
      const items: ResumeItem[] = rows.slice(0, MAX_RESUME_ROWS).map(row => ({
        label: safeSessionLabel(row.title ?? row.sessionId),
        description: row.sessionId,
        sessionId: row.sessionId,
      }));
      if (notice) {
        items.push({
          label: ENTER_SESSION_ID_LABEL,
          detail: notice,
          alwaysShow: true,
          enterSessionId: true,
        });
      } else if (items.length === 0) { void vscode.window.showInformationMessage('No saved Mini Agent sessions exist for this workspace.'); return; }
      const selected = await vscode.window.showQuickPick(items, {
        placeHolder: notice
          ? 'Resume a saved Mini Agent session (older sessions exist that are not listed)'
          : 'Resume a saved Mini Agent session in this workspace',
      });
      if (!selected) { return; }
      let sessionId = selected.sessionId;
      if (selected.enterSessionId) {
        const entered = await vscode.window.showInputBox({
          prompt: 'Saved Mini Agent session id (listed sessions are the newest only)',
          placeHolder: 'session id',
          validateInput: value => validateSessionId(value),
        });
        const id = entered?.trim();
        if (!id || validateSessionId(id)) { return; }
        sessionId = id;
      }
      if (!sessionId) { return; }
      requireTrustedWorkspace();
      if (this.conversation.isPrompting) { throw new Error('The current prompt is still active; saved session was not loaded.'); }
      await this.conversation.close();
      this.updates.begin(sessionId);
      let attached: PublicProtocolSession | undefined;
      try {
        const loaded = await client.request(acp.methods.agent.session.load, { sessionId: sessionId, cwd: this.folder.uri.fsPath, mcpServers: [] },
          { cancellationSignal: AbortSignal.timeout(10_000) });
        requireTrustedWorkspace();
        attached = this.updates.finish(client, sessionId, loaded.configOptions ?? [], loaded.modes);
        await this.conversation.adopt(attached);
        log.info(`ACP saved session resumed: ${sessionId}`);
      } catch (error) {
        this.updates.reset();
        if (attached) { await attached.close().catch(() => undefined); }
        else { await client.request(acp.methods.agent.session.close, { sessionId: sessionId }, { cancellationSignal: AbortSignal.timeout(5000) }).catch(() => undefined); }
        throw error;
      }
    });
  }

  /** Pick the persona (prompt and routed model) for the workspace conversation. */
  async selectPersona(): Promise<void> {
    return this.selectOption(PERSONA_CONFIG_ID, 'persona', 'Select the Mini Agent persona for this conversation');
  }

  /** Pick the model (a configured quick model) for the workspace conversation. */
  async selectModel(): Promise<void> {
    return this.selectOption(MODEL_CONFIG_ID, 'model', 'Select the Mini Agent model for this conversation');
  }

  /** Pick the reasoning effort for the workspace conversation. */
  async selectEffort(): Promise<void> {
    return this.selectOption(EFFORT_CONFIG_ID, 'reasoning effort', 'Select the Mini Agent reasoning effort for this conversation');
  }

  private async selectOption(configId: string, noun: string, placeHolder: string): Promise<void> {
    await this.start();
    return this.transitions.run(async () => {
      requireTrustedWorkspace();
      if (this.state !== 'running') { throw new Error('Mini Agent failed to start.'); }
      if (this.conversation.isPrompting) { throw new Error(`Wait for the current prompt to settle before switching ${noun}.`); }
      const active = await this.conversation.current();
      const selector = personaSelector(active.configOptions?.(), configId);
      if (!selector || !active.setConfigOption) {
        void vscode.window.showInformationMessage(`This Mini Agent session offers no ${noun} selection.`);
        return;
      }
      const items = selector.choices.map(choice => ({
        label: safeSessionLabel(choice.name),
        description: safeSessionLabel(choice.value === selector.current ? `${choice.value} (current)` : choice.value),
        ...(choice.description ? { detail: safeSessionLabel(choice.description) } : {}),
        value: choice.value,
      }));
      const picked = await vscode.window.showQuickPick(items, { placeHolder });
      if (!picked || picked.value === selector.current) { return; }
      requireTrustedWorkspace();
      if (this.conversation.isPrompting) { throw new Error(`A prompt started meanwhile; the ${noun} was not changed.`); }
      await active.setConfigOption(configId, picked.value);
      log.info(`ACP ${noun} selected: ${picked.value}`);
      void vscode.window.showInformationMessage(`Mini Agent ${noun}: ${picked.label}. It applies from the next prompt.`);
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
