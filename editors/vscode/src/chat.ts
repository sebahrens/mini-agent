import type * as acp from '@agentclientprotocol/sdk';
import { inlineCode, toolContentMarkdown } from './toolcontent';

export const MAX_RENDER_CHARS = 32 * 1024;

export type RenderEvent =
  | { kind: 'markdown'; value: string }
  | { kind: 'progress'; value: string };

export interface ChatStopResult {
  readonly metadata: { readonly stopReason: acp.StopReason };
  readonly errorDetails?: { readonly message: string };
}

/** Splits oversized text into several chunks (never mid surrogate pair) instead of truncating it. */
function chunked(value: string): string[] {
  const parts: string[] = [];
  let start = 0;
  while (start < value.length) {
    let end = Math.min(value.length, start + MAX_RENDER_CHARS);
    if (end < value.length) {
      const code = value.charCodeAt(end - 1);
      if (code >= 0xd800 && code <= 0xdbff) { end -= 1; }
    }
    parts.push(value.slice(start, end));
    start = end;
  }
  return parts;
}

function bounded(value: string): string {
  if (value.length <= MAX_RENDER_CHARS) { return value; }
  return `${value.slice(0, MAX_RENDER_CHARS)}\n\n…update truncated…`;
}

function contentText(content: acp.ContentBlock): string | undefined {
  return content.type === 'text' ? content.text : undefined;
}

export interface RendererOptions {
  /** Keep agent thought text in the transcript (quoted) instead of a vanishing progress line. */
  readonly keepThoughts?: boolean;
}

/** Converts typed ACP updates into the stable VS Code 1.90 chat stream surface. */
export class ChatUpdateRenderer {
  private readonly toolTitles = new Map<string, string>();

  constructor(private readonly options: RendererOptions = {}) {}

  render(update: acp.SessionUpdate): RenderEvent[] {
    switch (update.sessionUpdate) {
      case 'agent_message_chunk': {
        const text = contentText(update.content);
        return text ? chunked(text).map(value => ({ kind: 'markdown' as const, value })) : [];
      }
      case 'agent_thought_chunk': {
        const text = contentText(update.content);
        if (!text) { return []; }
        if (this.options.keepThoughts) {
          const quoted = bounded(text).split('\n').map(line => `> ${line}`).join('\n');
          return [{ kind: 'markdown', value: `\n${quoted}\n\n` }];
        }
        return [{ kind: 'progress', value: bounded(text) }];
      }
      case 'tool_call': {
        this.toolTitles.set(update.toolCallId, update.title);
        return [{
          kind: 'progress',
          value: bounded(`${update.title} (${update.status ?? 'pending'})`),
        }];
      }
      case 'tool_call_update': {
        const title = update.title ?? this.toolTitles.get(update.toolCallId) ?? 'tool call';
        this.toolTitles.set(update.toolCallId, title);
        const events: RenderEvent[] = [{
          kind: 'progress',
          value: bounded(`${title} (${update.status ?? 'updated'})`),
        }];
        const finished = update.status === 'completed' || update.status === 'failed';
        const body = finished ? toolContentMarkdown(update.content) : '';
        if (body) {
          events.push({
            kind: 'markdown',
            value: bounded(`\n\n**${inlineCode(title)}** ${update.status}\n\n${body}\n\n`),
          });
        }
        return events;
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
