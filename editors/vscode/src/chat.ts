import type * as acp from '@agentclientprotocol/sdk';

export const MAX_RENDER_CHARS = 32 * 1024;

export type RenderEvent =
  | { kind: 'markdown'; value: string }
  | { kind: 'progress'; value: string };

export interface ChatStopResult {
  readonly metadata: { readonly stopReason: acp.StopReason };
  readonly errorDetails?: { readonly message: string };
}

function bounded(value: string): string {
  if (value.length <= MAX_RENDER_CHARS) { return value; }
  return `${value.slice(0, MAX_RENDER_CHARS)}\n\n…update truncated…`;
}

function contentText(content: acp.ContentBlock): string | undefined {
  return content.type === 'text' ? content.text : undefined;
}

/** Converts typed ACP updates into the stable VS Code 1.90 chat stream surface. */
export class ChatUpdateRenderer {
  private readonly toolTitles = new Map<string, string>();

  render(update: acp.SessionUpdate): RenderEvent[] {
    switch (update.sessionUpdate) {
      case 'agent_message_chunk': {
        const text = contentText(update.content);
        return text ? [{ kind: 'markdown', value: bounded(text) }] : [];
      }
      case 'agent_thought_chunk': {
        const text = contentText(update.content);
        return text ? [{ kind: 'progress', value: bounded(text) }] : [];
      }
      case 'tool_call': {
        this.toolTitles.set(update.toolCallId, update.title);
        return [{
          kind: 'progress',
          value: bounded(`Tool ${update.toolCallId}: ${update.title} (${update.status ?? 'pending'})`),
        }];
      }
      case 'tool_call_update': {
        const title = update.title ?? this.toolTitles.get(update.toolCallId) ?? 'tool call';
        this.toolTitles.set(update.toolCallId, title);
        return [{
          kind: 'progress',
          value: bounded(`Tool ${update.toolCallId}: ${title} (${update.status ?? 'updated'})`),
        }];
      }
      case 'plan': {
        const summary = update.entries
          .map(entry => `${entry.status === 'completed' ? '✓' : entry.status === 'in_progress' ? '→' : '○'} ${entry.content}`)
          .join('\n');
        return summary ? [{ kind: 'progress', value: bounded(summary) }] : [];
      }
      default:
        return [];
    }
  }
}

export function stopResult(stopReason: acp.StopReason): ChatStopResult {
  const metadata = { stopReason } as const;
  switch (stopReason) {
    case 'end_turn':
    case 'cancelled':
      return { metadata };
    case 'refusal':
      return { metadata, errorDetails: { message: 'Mini Agent refused this request.' } };
    case 'max_tokens':
      return { metadata, errorDetails: { message: 'Mini Agent reached the response token limit.' } };
    case 'max_turn_requests':
      return { metadata, errorDetails: { message: 'Mini Agent reached the turn request limit.' } };
  }
}
