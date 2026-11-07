import * as acp from '@agentclientprotocol/sdk';
import { SessionUnavailableError, type ConversationSession } from './conversation';

const MAX_REPLAY_UPDATES = 100_000;
const MAX_REPLAY_BYTES = 48 * 1024 * 1024;
export const MAX_COMMANDS = 256;

/** One connection's current conversation, with bounded load-time update routing. */
export class SessionUpdateRouter {
  private current: PublicProtocolSession | undefined;
  /** Called for each update that arrives while no prompt is active (for example a /cron fire). */
  reportIdle?: (sessionId: string, update: acp.SessionUpdate) => void;
  private loading: { expectedId?: string; updates: acp.SessionNotification[]; bytes: number; overflow: boolean } | undefined;

  begin(expectedId?: string): void {
    if (this.current || this.loading) { throw new Error('An ACP conversation is already attached.'); }
    this.loading = { expectedId, updates: [], bytes: 0, overflow: false };
  }

  receive(notification: acp.SessionNotification): void {
    if (this.current?.sessionId === notification.sessionId) {
      this.current.receive(notification.update);
      return;
    }
    const loading = this.loading;
    if (!loading || (loading.expectedId && loading.expectedId !== notification.sessionId) || loading.overflow) { return; }
    loading.bytes += Buffer.byteLength(JSON.stringify(notification), 'utf8');
    if (loading.updates.length >= MAX_REPLAY_UPDATES || loading.bytes > MAX_REPLAY_BYTES) {
      loading.overflow = true;
      loading.updates = [];
      return;
    }
    loading.updates.push(notification);
  }

  finish(client: acp.ClientContext, sessionId: string,
    configOptions: readonly acp.SessionConfigOption[] = [],
    modes?: acp.SessionModeState | null): PublicProtocolSession {
    const loading = this.loading;
    this.loading = undefined;
    if (!loading || loading.overflow || (loading.expectedId && loading.expectedId !== sessionId)) {
      throw new Error('The saved ACP conversation exceeded its bounded replay or identity contract.');
    }
    const session = new PublicProtocolSession(client, sessionId, this,
      loading.updates.filter(update => update.sessionId === sessionId).map(update => update.update), configOptions, modes ?? undefined);
    this.current = session;
    return session;
  }

  detach(session: PublicProtocolSession): void {
    if (this.current === session) { this.current = undefined; }
  }

  reset(): void {
    this.current = undefined;
    this.loading = undefined;
  }
}

/** Uses public ACP RPCs only; no private SDK ActiveSession construction. */
export class PublicProtocolSession implements ConversationSession {
  private subscriber: ((update: acp.SessionUpdate) => void) | undefined;
  private activePrompt: Promise<acp.PromptResponse> | undefined;
  private disposed = false;
  private idleUpdates = 0;
  private idleBytes = 0;
  private idleDropped = 0;
  private idleOverflow = false;

  constructor(private readonly client: acp.ClientContext, readonly sessionId: string,
    private readonly router: SessionUpdateRouter, private replay: acp.SessionUpdate[],
    private options: readonly acp.SessionConfigOption[] = [],
    private modeState: acp.SessionModeState | undefined = undefined) {}

  private availableCommands: readonly acp.AvailableCommand[] = [];
  private commandsDropped = 0;

  receive(update: acp.SessionUpdate): void {
    // Routing is exact-ID scoped. Late updates cannot be adopted by a new owner.
    if (this.disposed) { return; }
    if (update.sessionUpdate === 'config_option_update') { this.options = update.configOptions; }
    if (update.sessionUpdate === 'current_mode_update' && this.modeState) {
      this.modeState = { ...this.modeState, currentModeId: update.currentModeId };
    }
    if (update.sessionUpdate === 'available_commands_update') {
      this.availableCommands = update.availableCommands.slice(0, MAX_COMMANDS);
      this.commandsDropped = Math.max(0, update.availableCommands.length - MAX_COMMANDS);
    }
    if (this.subscriber) { this.subscriber(update); return; }
    this.bufferIdleUpdate(update);
    this.router.reportIdle?.(this.sessionId, update);
  }

  /**
   * Server-initiated turns (for example a /cron fire) stream content while no
   * prompt() is active. Keep that content so the next prompt shows it, within
   * the same bounds as load-time replay; anything beyond them is counted and
   * announced instead of vanishing.
   */
  private bufferIdleUpdate(update: acp.SessionUpdate): void {
    switch (update.sessionUpdate) {
      case 'agent_message_chunk': case 'agent_thought_chunk': case 'tool_call':
      case 'tool_call_update': case 'plan': break;
      default: return;
    }
    const bytes = Buffer.byteLength(JSON.stringify(update), 'utf8');
    if (this.idleOverflow || this.idleUpdates >= MAX_REPLAY_UPDATES || this.idleBytes + bytes > MAX_REPLAY_BYTES) {
      this.idleOverflow = true;
      this.idleDropped += 1;
      return;
    }
    this.idleUpdates += 1;
    this.idleBytes += bytes;
    this.replay.push(update);
  }

  private takeIdleNotice(): acp.SessionUpdate | undefined {
    const dropped = this.idleDropped;
    this.idleUpdates = 0; this.idleBytes = 0; this.idleDropped = 0; this.idleOverflow = false;
    if (dropped === 0) { return undefined; }
    return { sessionUpdate: 'agent_message_chunk',
      content: { type: 'text', text: `\n\n_${dropped} update${dropped === 1 ? '' : 's'} from a server-started turn were too large to keep and were dropped._\n\n` } };
  }

  modes(): acp.SessionModeState | undefined { return this.modeState; }

  commands(): readonly acp.AvailableCommand[] { return this.availableCommands; }

  /** Advertised commands beyond the stored limit (not runnable through `/run`). */
  droppedCommands(): number { return this.commandsDropped; }

  /** Switch the permission mode; the agent confirms it with a current_mode_update. */
  async setMode(modeId: string): Promise<void> {
    if (this.disposed || this.activePrompt) { throw new Error('ACP conversation is unavailable or busy.'); }
    if (!this.modeState?.availableModes.some(mode => mode.id === modeId)) {
      throw new Error('The agent does not offer that permission mode.');
    }
    await this.client.request(acp.methods.agent.session.setMode,
      { sessionId: this.sessionId, modeId }, { cancellationSignal: AbortSignal.timeout(10_000) });
    if (!this.disposed && this.modeState) { this.modeState = { ...this.modeState, currentModeId: modeId }; }
  }

  configOptions(): readonly acp.SessionConfigOption[] { return this.options; }

  /** Select a config option value; the agent applies it from the next prompt. */
  async setConfigOption(configId: string, value: string): Promise<void> {
    if (this.disposed || this.activePrompt) { throw new Error('ACP conversation is unavailable or busy.'); }
    const response = await this.client.request(acp.methods.agent.session.setConfigOption,
      { sessionId: this.sessionId, configId, value }, { cancellationSignal: AbortSignal.timeout(10_000) });
    if (!this.disposed) { this.options = response.configOptions; }
  }

  async prompt(content: string | readonly acp.ContentBlock[], onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal): Promise<acp.StopReason> {
    if (signal.aborted) { return 'cancelled'; }
    if (this.disposed || this.activePrompt) { throw new Error('ACP conversation is unavailable or busy.'); }
    // Buffered server-started output is delivered only once the prompt is
    // accepted (first live update or settlement). A prompt the agent refuses
    // keeps it buffered instead of replaying it into a failed response.
    let flushed = false;
    const flush = (): void => {
      if (flushed) { return; }
      flushed = true;
      const notice = this.takeIdleNotice();
      if (notice) { this.replay.push(notice); }
      const pendingReplay = this.replay;
      this.replay = [];
      for (const update of pendingReplay) { onUpdate(update); }
    };
    this.subscriber = update => { flush(); onUpdate(update); };
    let cancellationSent = false;
    const cancel = (): void => {
      if (cancellationSent) { return; }
      cancellationSent = true;
      void this.client.notify(acp.methods.agent.session.cancel, { sessionId: this.sessionId }).catch(() => undefined);
    };
    signal.addEventListener('abort', cancel, { once: true });
    // Cancellation requests the server's owned retirement. Do not locally
    // abandon this response and mistake that for model/effect settlement.
    const pending = this.client.request(acp.methods.agent.session.prompt, {
      sessionId: this.sessionId,
      prompt: typeof content === 'string' ? [{ type: 'text', text: content }] : [...content],
    });
    this.activePrompt = pending;
    let refused = false;
    try { const stopReason = (await pending).stopReason; flush(); return stopReason; }
    catch (error) {
      if (error instanceof acp.RequestError && error.code === -32602 && error.message.includes('unknown ACP session')) { throw new SessionUnavailableError(); }
      if (error instanceof Error && error.message.includes('already has an active prompt')) {
        refused = true;
        throw new Error('Mini Agent is still running a turn it started itself (for example a /cron fire). Its output will appear in the next reply; try again when it finishes.');
      }
      throw error;
    }
    finally {
      signal.removeEventListener('abort', cancel);
      this.activePrompt = undefined;
      this.subscriber = undefined;
      if (!refused) { try { flush(); } catch { /* the consumer already failed */ } }
    }
  }

  async close(): Promise<void> {
    if (this.disposed) { return; }
    try {
      const pending = this.activePrompt;
      if (pending) {
        await this.client.notify(acp.methods.agent.session.cancel, { sessionId: this.sessionId });
        // The outer process owner retains its existing forced-stop policy if a
        // peer never acknowledges cancellation; a switch never proceeds then.
        await waitForRetirement(pending);
      }
      await this.client.request(acp.methods.agent.session.close, { sessionId: this.sessionId },
        { cancellationSignal: AbortSignal.timeout(5000) });
    } finally {
      this.disposed = true;
      this.router.detach(this);
      this.subscriber = undefined;
      this.replay = [];
    }
  }
}

async function waitForRetirement(pending: Promise<unknown>): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([pending.catch(() => undefined), new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('ACP prompt retirement timed out; conversation was not switched.')), 5000);
    })]);
  } finally { if (timer) { clearTimeout(timer); } }
}
