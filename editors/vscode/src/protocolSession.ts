import * as acp from '@agentclientprotocol/sdk';
import { SessionUnavailableError, type ConversationSession } from './conversation';

const MAX_REPLAY_UPDATES = 100_000;
const MAX_REPLAY_BYTES = 48 * 1024 * 1024;
export const MAX_COMMANDS = 256;

/** One connection's current conversation, with bounded load-time update routing. */
export class SessionUpdateRouter {
  private current: PublicProtocolSession | undefined;
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
    this.subscriber?.(update);
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
    this.subscriber = onUpdate;
    try { for (const update of this.replay) { onUpdate(update); } }
    catch (error) { this.subscriber = undefined; this.replay = []; throw error; }
    this.replay = [];
    if (signal.aborted) { this.subscriber = undefined; return 'cancelled'; }
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
    try { return (await pending).stopReason; }
    catch (error) {
      if (error instanceof acp.RequestError && error.code === -32602 && error.message.includes('unknown ACP session')) { throw new SessionUnavailableError(); }
      throw error;
    }
    finally {
      signal.removeEventListener('abort', cancel);
      this.activePrompt = undefined;
      this.subscriber = undefined;
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
