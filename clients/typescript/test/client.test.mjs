import test from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { Session, buildArgs, isRecord } from '../src/index.js';

const fake = fileURLToPath(new URL('./fake-agent.mjs', import.meta.url));
const fakeSession = (extra = {}) => new Session({ binary: process.execPath, argv: [fake], ...extra });

test('buildArgs requests the control channel and claude schema', () => {
  const args = buildArgs({ prompt: 'hi', askTimeoutSecs: 5, model: 'fast' });
  assert.deepEqual(args.slice(0, 2), ['-p', 'hi']);
  for (const flag of ['--input-format', '--stream-json-control', '--stream-json-schema', '--stream-json-ask-timeout', '--model']) {
    assert.ok(args.includes(flag), flag);
  }
});

test('one turn with a permission ask answered by callback', async () => {
  const seen = [];
  const session = fakeSession({
    canUseTool: (name, input) => { seen.push([name, input]); return { behavior: 'allow' }; },
  });
  session.send('run ls');
  const records = [];
  for await (const record of session.turn()) records.push(record);
  await session.close();
  assert.deepEqual(seen, [['shell', { command: 'ls' }]]);
  assert.ok(records.every(isRecord));
  assert.equal(records.at(-1).type, 'result');
  assert.equal(records.at(-1).result, 'allowed');
  assert.ok(!records.some((r) => r.type === 'control_request'));
});

test('no callback denies; interrupt ends a waiting turn; error acks reject', async () => {
  const session = fakeSession();
  session.send('run ls');
  const first = [];
  for await (const record of session.turn()) first.push(record);
  assert.equal(first.at(-1).result, 'denied');
  session.send('wait');
  assert.deepEqual(await session.interrupt(), {});
  const second = [];
  for await (const record of session.turn()) second.push(record);
  assert.equal(second.at(-1).result, 'interrupted');
  assert.deepEqual(await session.initialize(), { protocol: 'mini-agent-control/1' });
  await assert.rejects(session.setModel('x'), /unknown/);
  await session.close();
});

test('requests fail once the process has exited', async () => {
  const session = fakeSession();
  await session.close();
  await assert.rejects(session.interrupt());
  assert.equal(await session.next(), undefined);
});
