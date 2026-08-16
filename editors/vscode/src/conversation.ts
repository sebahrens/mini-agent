import type * as acp from '@agentclientprotocol/sdk';

export class SessionUnavailableError extends Error {
  constructor(message = 'The ACP session is no longer available') {
    super(message);
    this.name = 'SessionUnavailableError';
  }
}

export interface ConversationSession {
  prompt(
    text: string,
    onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal,
  ): Promise<acp.StopReason>;
  close(): Promise<void>;
}

export type ConversationSessionFactory = () => Promise<ConversationSession>;

/** Runs lifecycle transitions in call order and keeps the queue usable after failures. */
export class SerialTransitionQueue {
  private tail: Promise<void> = Promise.resolve();

  run(operation: () => Promise<void>): Promise<void> {
    const result = this.tail.then(operation);
    this.tail = result.catch(() => undefined);
    return result;
  }
}

/** Keeps one ACP session across chat turns and recreates it once if the agent closed it. */
export class Conversation {
  private session: ConversationSession | undefined;
  private prompting = false;

  constructor(private readonly createSession: ConversationSessionFactory) {}

  async prompt(
    text: string,
    onUpdate: (update: acp.SessionUpdate) => void,
    signal: AbortSignal,
  ): Promise<acp.StopReason> {
    if (signal.aborted) { return 'cancelled'; }
    if (this.prompting) { throw new Error('Mini Agent is already handling a prompt in this workspace.'); }

    this.prompting = true;
    try {
      for (let attempt = 0; attempt < 2; attempt += 1) {
        const session = await this.ensureSession();
        if (signal.aborted) { return 'cancelled'; }
        try {
          return await session.prompt(text, onUpdate, signal);
        } catch (error) {
          if (!(error instanceof SessionUnavailableError) || attempt > 0 || signal.aborted) {
            throw error;
          }
          await this.discardSession(session);
        }
      }
      throw new Error('Mini Agent could not recreate its ACP session.');
    } finally {
      this.prompting = false;
    }
  }

  async close(): Promise<void> {
    const session = this.session;
    this.session = undefined;
    if (session) { await session.close(); }
  }

  /** Drops routing state after a process exit; no protocol close is possible then. */
  invalidate(): void {
    this.session = undefined;
  }

  private async ensureSession(): Promise<ConversationSession> {
    this.session ??= await this.createSession();
    return this.session;
  }

  private async discardSession(session: ConversationSession): Promise<void> {
    if (this.session === session) { this.session = undefined; }
    try {
      await session.close();
    } catch {
      // The agent already declared this session unavailable.
    }
  }
}
