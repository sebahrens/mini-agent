// Client for `mini-agent -p --input-format stream-json --output stream-json --stream-json-control`.
// Wire contract: docs/specs/stream-json-control.md. Unknown record fields are ignored.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

export const PROTOCOL = 'mini-agent-control/1';

/** Builds the argv for the headless stream-json session. */
export function buildArgs(options = {}) {
  const args = ['-p'];
  if (options.prompt) args.push(options.prompt);
  args.push('--input-format', 'stream-json', '--output', 'stream-json', '--stream-json-control');
  args.push('--stream-json-schema', options.schema ?? 'claude');
  if (options.includePartialMessages) args.push('--include-partial-messages');
  if (options.askTimeoutSecs !== undefined) args.push('--stream-json-ask-timeout', String(options.askTimeoutSecs));
  if (options.previewBytes !== undefined) args.push('--stream-json-preview-bytes', String(options.previewBytes));
  if (options.model) args.push('--model', options.model);
  if (options.permissionMode) args.push('--permission-mode', options.permissionMode);
  if (options.continueSession) args.push('--continue');
  return [...args, ...(options.extraArgs ?? [])];
}

/**
 * One headless session. `messages()` yields every record the agent prints (including
 * `result`); `can_use_tool` asks are answered through `options.canUseTool` and are not yielded.
 */
export class Session {
  constructor(options = {}) {
    this.options = options;
    this.nextId = 1;
    this.pending = new Map();
    this.queue = [];
    this.waiters = [];
    this.closed = false;
    const bin = options.binary ?? 'mini-agent';
    this.child = spawn(bin, options.argv ?? buildArgs(options), {
      cwd: options.cwd,
      env: options.env ? { ...process.env, ...options.env } : process.env,
      stdio: ['pipe', 'pipe', options.stderr ?? 'ignore'],
    });
    this.exit = new Promise((resolve) => this.child.once('close', (code, signal) => {
      this.#finish();
      resolve({ code, signal });
    }));
    this.child.on('error', (error) => { this.error = error; this.#finish(); });
    this.child.stdin.on('error', () => {});
    createInterface({ input: this.child.stdout }).on('line', (line) => this.#line(line));
  }

  #finish() {
    this.closed = true;
    for (const reject of this.pending.values()) reject.reject(new Error('mini-agent exited'));
    this.pending.clear();
    for (const waiter of this.waiters.splice(0)) waiter(undefined);
  }

  #push(record) {
    const waiter = this.waiters.shift();
    if (waiter) waiter(record); else this.queue.push(record);
  }

  #line(line) {
    let record;
    try { record = JSON.parse(line); } catch { return; }
    if (record.type === 'control_response') {
      const body = record.response ?? {};
      const entry = this.pending.get(body.request_id);
      if (entry) {
        this.pending.delete(body.request_id);
        if (body.subtype === 'success') entry.resolve(body.response ?? {});
        else entry.reject(new Error(body.error ?? 'control request failed'));
      }
      return;
    }
    if (record.type === 'control_request' && record.request?.subtype === 'can_use_tool') {
      void this.#answerPermission(record);
      return;
    }
    this.#push(record);
  }

  async #answerPermission(record) {
    let reply = { behavior: 'deny', message: 'no canUseTool callback' };
    const callback = this.options.canUseTool;
    if (callback) {
      try {
        const ask = record.request;
        reply = await callback(ask.tool_name, ask.tool_input ?? ask.input, ask);
      } catch (error) {
        reply = { behavior: 'deny', message: String(error?.message ?? error) };
      }
    }
    this.#write({ type: 'control_response', response: { subtype: 'success', request_id: record.request_id, response: reply } });
  }

  #write(value) {
    if (this.closed || this.child.stdin.destroyed) return false;
    this.child.stdin.write(`${JSON.stringify(value)}\n`);
    return true;
  }

  /** Sends one user message; its turn ends with a `result` record. */
  send(text) {
    return this.#write({ type: 'user', message: { role: 'user', content: text } });
  }

  /** Sends a control request and resolves with its `response.response` payload. */
  control(subtype, fields = {}) {
    const requestId = `c${this.nextId++}`;
    const promise = new Promise((resolve, reject) => this.pending.set(requestId, { resolve, reject }));
    if (!this.#write({ type: 'control_request', request_id: requestId, request: { subtype, ...fields } })) {
      this.pending.delete(requestId);
      return Promise.reject(new Error('mini-agent is not running'));
    }
    return promise;
  }

  initialize() { return this.control('initialize'); }
  interrupt() { return this.control('interrupt'); }
  setModel(model) { return this.control('set_model', { model }); }
  setPermissionMode(mode) { return this.control('set_permission_mode', { mode }); }

  /** Next record, or `undefined` once the process has exited and the queue is empty. */
  next() {
    if (this.queue.length) return Promise.resolve(this.queue.shift());
    if (this.closed) return Promise.resolve(undefined);
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  async *messages() {
    for (;;) {
      const record = await this.next();
      if (record === undefined) return;
      yield record;
    }
  }

  /** Yields records up to and including the next `result`. */
  async *turn() {
    for await (const record of this.messages()) {
      yield record;
      if (record.type === 'result') return;
    }
  }

  /** Closes stdin (the run ends at EOF) and waits for exit. */
  async close() {
    if (!this.child.stdin.destroyed) this.child.stdin.end();
    return this.exit;
  }
}

/** Runs one prompt to completion: yields its records, then ends the session. */
export async function* query({ prompt, options = {} }) {
  const session = new Session({ ...options, prompt });
  try {
    for await (const record of session.turn()) yield record;
  } finally {
    await session.close();
  }
}

/** Validates a record against the minimal shape every stream-json record shares. */
export function isRecord(value) {
  return value !== null && typeof value === 'object' && typeof value.type === 'string';
}
