// Focused real extension-host/public-RPC saved-session proof. The runner owns
// the private profile and loopback provider; no SDK constructor reflection.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vscode = require('vscode');

const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function within(promise, milliseconds, label) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} timed out`)), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

exports.run = async () => {
  const extension = vscode.extensions.getExtension('mini-agent.mini-agent');
  assert.ok(extension, 'installed extension exists');
  const relative = path.relative(process.env.MINI_AGENT_VSIX_PROFILE_ROOT, extension.extensionPath);
  assert.ok(relative && !relative.startsWith('..') && !path.isAbsolute(relative));
  await extension.activate();
  assert.ok(vscode.workspace.isTrusted);
  const config = vscode.workspace.getConfiguration('mini-agent');
  const original = config.inspect('executablePath')?.globalValue;
  const bundled = path.join(extension.extensionPath, 'bin', `${process.platform}-${process.arch}`,
    process.platform === 'win32' ? 'mini-agent.exe' : 'mini-agent');
  const installed = require(path.join(extension.extensionPath, 'dist', 'extension.js'));
  const context = { extensionUri: extension.extensionUri, extension: { packageJSON: extension.packageJSON }, subscriptions: [] };
  const events = () => fs.readFileSync(process.env.MINI_AGENT_IDE_PROVIDER_EVENTS, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
  const sessions = path.join(process.env.ZS_STATE_DIR, 'sessions');
  const first = 'ACP53_SAVED_SENTINEL literal $HOME @never-fetch';
  try {
    await config.update('executablePath', bundled, vscode.ConfigurationTarget.Global);
    const active = await installed.ensureSession(context);
    assert.ok(active);
    const initialUpdates = [];
    assert.equal(await within(active.prompt(first, update => initialUpdates.push(update), new AbortController().signal), 30_000, 'first saved prompt'), 'end_turn');
    assert.ok(initialUpdates.some(update => update.sessionUpdate === 'agent_message_chunk' && update.content.text?.includes('OFFLINE_IDE_REPLY')));
    await vscode.commands.executeCommand('mini-agent.stop');
    const names = fs.readdirSync(sessions).filter(name => name.endsWith('.json'));
    assert.equal(names.length, 1, 'one canonical durable session');
    const savedPath = path.join(sessions, names[0]);
    const before = JSON.parse(fs.readFileSync(savedPath, 'utf8'));
    assert.equal(before.messages[0].content, first);
    assert.equal(before.total_input_tokens, 20);
    assert.equal(events().length, 1);

    let settled = false;
    const resume = vscode.commands.executeCommand('mini-agent.resume');
    void resume.then(() => { settled = true; }, () => { settled = true; });
    const deadline = Date.now() + 15_000;
    while (!settled && Date.now() < deadline) {
      await delay(100);
      await vscode.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
    }
    await within(resume, 1000, 'real saved-session picker');
    assert.equal(events().length, 1, 'list/load/picker do not request a completion');
    const resumed = await installed.ensureSession(context);
    assert.ok(resumed);
    const replayed = [];
    assert.equal(await within(resumed.prompt('ACP53_CONTINUE_SAVED_CONTEXT', update => replayed.push(update), new AbortController().signal), 30_000, 'resumed prompt'), 'end_turn');
    assert.ok(replayed.some(update => update.sessionUpdate === 'user_message_chunk' && update.content.text === first), 'saved literal user data is replayed through exact-ID session/update routing');
    assert.equal(events().length, 2);
    assert.ok(events()[1].messageHistory.some(row => row.role === 'user' && row.content === first), 'actual second wire request contains original canonical history');
    await vscode.commands.executeCommand('mini-agent.stop');
    const after = JSON.parse(fs.readFileSync(savedPath, 'utf8'));
    assert.equal(after.id, before.id);
    assert.equal(after.messages.length, 4);
    assert.equal(after.total_input_tokens, 40);
    assert.equal(after.total_output_tokens, 10);
    assert.deepEqual(after.permission_allowlist, []);
    console.log('IDE_ACP53_RESUME_PASS actual installed extension, picker, public load/prompt, same durable ID and 2 loopback requests');
  } finally {
    try { await vscode.commands.executeCommand('mini-agent.stop'); }
    finally { await config.update('executablePath', original, vscode.ConfigurationTarget.Global); }
  }
};
