import { ChatViewModel, type ChatSnapshot } from './chatViewModel';

/** Page for the Mini Agent view. Everything model-supplied is inserted with textContent. */
export function renderChatHtml(nonce: string, cspSource: string): string {
  const csp = `default-src 'none'; style-src ${cspSource} 'nonce-${nonce}'; script-src 'nonce-${nonce}';`;
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style nonce="${nonce}">
body{font-family:var(--vscode-font-family);color:var(--vscode-foreground);padding:6px;margin:0}
#log{display:flex;flex-direction:column;gap:6px}
.user{background:var(--vscode-input-background);padding:4px 6px;white-space:pre-wrap}
.agent,.notice{white-space:pre-wrap}.thought{opacity:.7;font-style:italic;white-space:pre-wrap}
details.tool{border-left:3px solid var(--vscode-focusBorder);padding-left:6px}
details.tool pre{white-space:pre-wrap;max-height:240px;overflow:auto}
#plan{margin:6px 0;padding-left:18px}#meter{font-size:11px;opacity:.8}
form{display:flex;gap:4px;margin-top:8px}textarea{flex:1;font:inherit}
</style></head><body>
<div id="meter" role="status"></div><ul id="plan" aria-label="Plan"></ul><div id="log" role="log" aria-live="polite"></div>
<form id="f"><textarea id="t" rows="2" aria-label="Message for Mini Agent"></textarea>
<button id="s" type="submit">Send</button><button id="c" type="button">Stop</button></form>
<script nonce="${nonce}">
const vscode = acquireVsCodeApi();
const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; };
function render(s) {
  const log = document.getElementById('log'); log.replaceChildren();
  for (const e of s.entries) {
    if (e.kind === 'tool') {
      const d = el('details', 'tool'); d.append(el('summary', '', e.title + ' (' + e.status + ')'));
      for (const p of e.paths) { const a = el('a', '', p); a.href = '#'; a.onclick = ev => { ev.preventDefault(); vscode.postMessage({ type: 'open', path: p }); }; d.append(a, el('br')); }
      if (e.body) d.append(el('pre', '', e.body)); log.append(d);
    } else log.append(el('div', e.kind, e.text));
  }
  const plan = document.getElementById('plan'); plan.replaceChildren();
  for (const p of s.plan) plan.append(el('li', '', (p.status === 'completed' ? '[x] ' : p.status === 'in_progress' ? '[>] ' : '[ ] ') + p.content));
  document.getElementById('meter').textContent = s.usage ? 'Context: ' + s.usage.used + ' / ' + s.usage.size + ' tokens' : '';
  document.getElementById('s').disabled = s.busy;
  window.scrollTo(0, document.body.scrollHeight);
}
window.addEventListener('message', ev => { if (ev.data && ev.data.type === 'state') render(ev.data.state); });
document.getElementById('f').addEventListener('submit', ev => { ev.preventDefault(); const t = document.getElementById('t'); if (t.value.trim()) { vscode.postMessage({ type: 'send', text: t.value }); t.value = ''; } });
document.getElementById('c').addEventListener('click', () => vscode.postMessage({ type: 'cancel' }));
vscode.postMessage({ type: 'ready' });
</script></body></html>`;
}

export type ViewMessage =
  | { readonly type: 'send'; readonly text: string }
  | { readonly type: 'cancel' }
  | { readonly type: 'ready' }
  | { readonly type: 'open'; readonly path: string };

/** Validates an untrusted message from the webview. */
export function parseViewMessage(value: unknown): ViewMessage | undefined {
  if (typeof value !== 'object' || value === null) { return undefined; }
  const message = value as Record<string, unknown>;
  switch (message['type']) {
    case 'send':
      return typeof message['text'] === 'string' && message['text'].trim() && message['text'].length <= 100_000
        ? { type: 'send', text: message['text'] } : undefined;
    case 'open':
      return typeof message['path'] === 'string' && message['path'].length <= 4096
        ? { type: 'open', path: message['path'] } : undefined;
    case 'cancel': return { type: 'cancel' };
    case 'ready': return { type: 'ready' };
    default: return undefined;
  }
}

export interface ChatViewHost {
  /** Runs one prompt over the shared Conversation; resolves with the stop reason. */
  runPrompt(text: string, onUpdate: (update: import('@agentclientprotocol/sdk').SessionUpdate) => void, signal: AbortSignal): Promise<string>;
  openPath(path: string): void;
  post(state: ChatSnapshot): void;
}

/** Drives one view: webview messages in, snapshots out. No vscode API, so it is unit-testable. */
export class ChatViewController {
  readonly model = new ChatViewModel();
  private abort: AbortController | undefined;

  constructor(private readonly host: ChatViewHost) {}

  handle(raw: unknown): void {
    const message = parseViewMessage(raw);
    if (!message) { return; }
    switch (message.type) {
      case 'ready': this.host.post(this.model.snapshot()); return;
      case 'cancel': this.abort?.abort(); return;
      case 'open': this.host.openPath(message.path); return;
      case 'send': void this.send(message.text); return;
    }
  }

  private async send(text: string): Promise<void> {
    if (this.abort) { this.model.notice('Mini Agent is still working on the previous prompt.'); this.host.post(this.model.snapshot()); return; }
    this.abort = new AbortController();
    this.model.addUser(text);
    this.host.post(this.model.snapshot());
    try {
      const reason = await this.host.runPrompt(text, update => {
        this.model.apply(update);
        this.host.post(this.model.snapshot());
      }, this.abort.signal);
      this.model.finish(reason as never);
    } catch (error) {
      this.model.notice(`Request failed: ${error instanceof Error ? error.message : String(error)}`);
      this.model.finish('error');
    } finally {
      this.abort = undefined;
      this.host.post(this.model.snapshot());
    }
  }
}
