import type * as acp from '@agentclientprotocol/sdk';
import { diffText } from './toolcontent';

/** Caps keeping the webview state bounded however long the session runs. */
export const MAX_ENTRIES = 400;
export const MAX_ENTRY_CHARS = 64 * 1024;
export const MAX_PLAN_ENTRIES = 50;

export interface ToolEntry {
  readonly kind: 'tool';
  readonly id: string;
  title: string;
  status: string;
  /** Result text and bounded diff rows, shown when the call is expanded. */
  body: string;
  /** File anchors (absolute paths) the call touched. */
  paths: string[];
}
export type TextEntry = { readonly kind: 'user' | 'agent' | 'thought' | 'notice'; text: string };
export type ChatEntry = TextEntry | ToolEntry;

export interface PlanItem { readonly content: string; readonly status: string }
export interface UsageMeter { readonly used: number; readonly size: number }

export interface ChatSnapshot {
  readonly entries: readonly ChatEntry[];
  readonly plan: readonly PlanItem[];
  readonly usage: UsageMeter | undefined;
  readonly busy: boolean;
}

function clip(text: string): string {
  return text.length <= MAX_ENTRY_CHARS ? text : `${text.slice(0, MAX_ENTRY_CHARS)}\n… truncated`;
}

/**
 * Reduces typed ACP updates into the transcript a webview renders: messages, a
 * persistent tool list with diff bodies, the plan as a checklist (kept after the
 * turn ends) and the context-usage meter. Contains no vscode API.
 */
export class ChatViewModel {
  private entries: ChatEntry[] = [];
  private plan: PlanItem[] = [];
  private usage: UsageMeter | undefined;
  private busy = false;
  private readonly tools = new Map<string, ToolEntry>();

  addUser(text: string): void {
    this.busy = true;
    this.push({ kind: 'user', text: clip(text) });
  }

  notice(text: string): void {
    this.push({ kind: 'notice', text: clip(text) });
  }

  finish(reason: acp.StopReason | 'error'): void {
    this.busy = false;
    if (reason !== 'end_turn') { this.notice(`Turn ended: ${reason}`); }
  }

  apply(update: acp.SessionUpdate): void {
    switch (update.sessionUpdate) {
      case 'agent_message_chunk':
      case 'agent_thought_chunk': {
        if (update.content.type !== 'text') { return; }
        const kind = update.sessionUpdate === 'agent_message_chunk' ? 'agent' : 'thought';
        const last = this.entries[this.entries.length - 1];
        if (last && last.kind === kind) { last.text = clip(last.text + update.content.text); }
        else { this.push({ kind, text: clip(update.content.text) }); }
        return;
      }
      case 'tool_call':
      case 'tool_call_update': {
        let tool = this.tools.get(update.toolCallId);
        if (!tool) {
          tool = { kind: 'tool', id: update.toolCallId, title: 'tool call', status: 'pending', body: '', paths: [] };
          this.tools.set(update.toolCallId, tool);
          this.push(tool);
        }
        if (update.title) { tool.title = update.title.slice(0, 300); }
        if (update.status) { tool.status = update.status; }
        const body = (update.content ?? []).map(entry => {
          if (entry.type === 'diff') { return diffText(entry); }
          if (entry.type === 'content' && entry.content.type === 'text') { return entry.content.text; }
          return '';
        }).filter(Boolean).join('\n\n');
        if (body) { tool.body = clip(body); }
        for (const location of update.locations ?? []) {
          if (!tool.paths.includes(location.path) && tool.paths.length < 20) { tool.paths.push(location.path); }
        }
        return;
      }
      case 'plan':
        this.plan = update.entries.slice(0, MAX_PLAN_ENTRIES)
          .map(entry => ({ content: entry.content.slice(0, 500), status: entry.status }));
        return;
      case 'usage_update':
        this.usage = { used: update.used, size: update.size };
        return;
      default:
        return;
    }
  }

  snapshot(): ChatSnapshot {
    return {
      entries: this.entries.map(entry => ({ ...entry })),
      plan: [...this.plan],
      usage: this.usage,
      busy: this.busy,
    };
  }

  private push(entry: ChatEntry): void {
    this.entries.push(entry);
    if (this.entries.length > MAX_ENTRIES) {
      const dropped = this.entries.splice(0, this.entries.length - MAX_ENTRIES);
      for (const old of dropped) { if (old.kind === 'tool') { this.tools.delete(old.id); } }
    }
  }
}
