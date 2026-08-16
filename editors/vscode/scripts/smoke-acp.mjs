import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { Readable, Writable } from 'node:stream';
import * as acp from '@agentclientprotocol/sdk';

const binary = process.argv[2];
if (!binary) { throw new Error('Usage: node scripts/smoke-acp.mjs <mini-agent-binary>'); }
const executable = resolve(binary);
const manifest = JSON.parse(readFileSync(resolve('package.json'), 'utf8'));
const proc = spawn(executable, ['--acp'], {
  cwd: resolve('../..'),
  shell: false,
  stdio: ['pipe', 'pipe', 'pipe'],
  env: {
    ...process.env,
    // The prompt is cancelled before provider work; this only satisfies startup validation.
    OPENROUTER_API_KEY: process.env.OPENROUTER_API_KEY ?? 'unused-acp-artifact-smoke-key',
  },
});
let stderr = '';
proc.stderr.on('data', chunk => {
  if (stderr.length < 64 * 1024) { stderr += chunk.toString(); }
});

const output = Writable.toWeb(proc.stdin);
const input = Readable.toWeb(proc.stdout);
const app = acp.client({ name: 'mini-agent-vscode-artifact-smoke' })
  .onRequest(acp.methods.client.session.requestPermission, () => ({
    outcome: { outcome: 'cancelled' },
  }));
const connection = app.connect(acp.ndJsonStream(output, input));
let session;

function within(promise, milliseconds, label) {
  return new Promise((resolvePromise, rejectPromise) => {
    const timer = setTimeout(() => rejectPromise(new Error(`${label} timed out`)), milliseconds);
    promise.then(
      value => { clearTimeout(timer); resolvePromise(value); },
      error => { clearTimeout(timer); rejectPromise(error); },
    );
  });
}

async function readStop(activeSession) {
  for (;;) {
    const message = await activeSession.nextUpdate();
    if (message.kind === 'stop') { return message.stopReason; }
  }
}

async function terminate() {
  if (proc.exitCode !== null) { return; }
  const exited = new Promise(resolveExit => proc.once('exit', resolveExit));
  proc.kill('SIGTERM');
  try {
    await within(exited, 5000, 'ACP process termination');
  } catch {
    proc.kill('SIGKILL');
    await within(exited, 2000, 'forced ACP process termination');
  }
}

try {
  await within(connection.agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: {},
    clientInfo: { name: 'mini-agent-vscode-artifact-smoke', version: manifest.version },
  }), 10_000, 'initialize');
  session = await within(connection.agent.buildSession(process.cwd()).start(), 10_000, 'session/new');

  const cancellation = new AbortController();
  void session.prompt(
    'This artifact smoke prompt must be cancelled before provider work.',
    { cancellationSignal: cancellation.signal },
  );
  // Exercise the same two cancellation paths as the extension. Request-level
  // cancellation is ordered with the prompt by the SDK; session/cancel covers
  // ACP clients and agents that use the explicit lifecycle notification.
  cancellation.abort();
  await connection.agent.notify(acp.methods.agent.session.cancel, { sessionId: session.sessionId });
  const reason = await within(readStop(session), 20_000, 'cancelled prompt');
  if (reason !== 'cancelled') { throw new Error(`Expected cancelled stop reason, received ${reason}`); }

  await within(connection.agent.request(acp.methods.agent.session.close, {
    sessionId: session.sessionId,
  }), 10_000, 'session/close');
  console.log(`Verified ACP initialize/new/prompt/cancel/close for ${executable}`);
} catch (error) {
  if (stderr) { console.error(stderr); }
  throw error;
} finally {
  session?.dispose();
  connection.close();
  await terminate();
}
