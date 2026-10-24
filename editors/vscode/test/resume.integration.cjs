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
      timer = setTimeout(() => reject(Object.assign(new Error(`${label} timed out`), { code: 'IDE_ACP53_TIMEOUT' })), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

// This offline harness uses fixed synthetic inputs. Retain only a bounded
// error name/message, scrubbing fixture paths and credential values; never
// serialize the error object, stack, environment or provider/session data.
function reportFailure(stage, error) {
  const categories = {
    ERR_ASSERTION: 'assertion', IDE_ACP53_TIMEOUT: 'timeout', ETIMEDOUT: 'timeout',
    ENOENT: 'missing', EACCES: 'permission', EPERM: 'permission', EPIPE: 'pipe', ECONNRESET: 'connection-reset',
  };
  const category = Object.hasOwn(categories, error?.code) ? categories[error.code]
    : error instanceof SyntaxError ? 'json' : error instanceof TypeError ? 'type' : 'error';
  const location = typeof error?.stack === 'string'
    ? error.stack.split('\n').slice(1).map(line => line.match(/\bresume\.integration\.cjs:(\d{1,7}):(\d{1,5})\b/)).find(Boolean)
    : undefined;
  let detail = `${typeof error?.name === 'string' ? error.name : 'Error'}: ${typeof error?.message === 'string' ? error.message : ''}`;
  for (const key of ['MINI_AGENT_VSIX_PROFILE_ROOT', 'ZS_CONFIG_DIR', 'MINI_AGENT_HOME', 'MINI_AGENT_IDE_PROVIDER_EVENTS']) {
    const value = process.env[key];
    if (value) { detail = detail.replace(new RegExp(value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi'), '<fixture>'); }
  }
  for (const key of ['OPENAI_API_KEY', 'OPENROUTER_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'AZURE_OPENAI_API_KEY', 'GH_TOKEN', 'GITHUB_TOKEN']) {
    if (process.env[key]) { detail = detail.replaceAll(process.env[key], '<redacted>'); }
  }
  console.error(`IDE_ACP53_RESUME_FAILED stage=${stage} category=${category} location=${location ? `resume.integration.cjs:${location[1]}:${location[2]}` : 'unknown'} detail=${JSON.stringify(detail.slice(0, 2000))}`);
}

exports.run = async () => {
  let stage = 'entry';
  const mark = value => { stage = value; console.log(`IDE_ACP53_RESUME_STAGE=${stage}`); };
  let config;
  let original;
  let restoreConfig = false;
  let failed = false;
  mark('entry');
  try {
    mark('extension-discovery');
    const extension = vscode.extensions.getExtension('mini-agent.mini-agent');
    assert.ok(extension, 'installed extension exists');
    mark('installed-profile');
    const relative = path.relative(process.env.MINI_AGENT_VSIX_PROFILE_ROOT, extension.extensionPath);
    assert.ok(relative && !relative.startsWith('..') && !path.isAbsolute(relative), 'extension belongs to the isolated installed profile');
    mark('activate');
    await extension.activate();
    mark('workspace-trust');
    assert.ok(vscode.workspace.isTrusted, 'resume fixture workspace is trusted');
    mark('config-setup');
    config = vscode.workspace.getConfiguration('mini-agent');
    original = config.inspect('executablePath')?.globalValue;
    const bundled = path.join(extension.extensionPath, 'bin', `${process.platform}-${process.arch}`,
      process.platform === 'win32' ? 'mini-agent.exe' : 'mini-agent');
    const installed = require(path.join(extension.extensionPath, 'dist', 'extension.js'));
    const context = { extensionUri: extension.extensionUri, extension: { packageJSON: extension.packageJSON }, subscriptions: [] };
    const events = () => fs.readFileSync(process.env.MINI_AGENT_IDE_PROVIDER_EVENTS, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
    const sessions = path.join(process.env.ZS_STATE_DIR, 'sessions');
    const first = 'ACP53_SAVED_SENTINEL literal $HOME @never-fetch';
    restoreConfig = true;
    await config.update('executablePath', bundled, vscode.ConfigurationTarget.Global);
    mark('start-initial');
    const active = await installed.ensureSession(context);
    assert.ok(active, 'installed extension creates the initial ACP session');
    const initialUpdates = [];
    mark('prompt-initial');
    assert.equal(await within(active.prompt(first, update => initialUpdates.push(update), new AbortController().signal), 30_000, 'first saved prompt'), 'end_turn', 'initial prompt completes normally');
    assert.ok(initialUpdates.some(update => update.sessionUpdate === 'agent_message_chunk' && update.content.text?.includes('OFFLINE_IDE_REPLY')), 'initial prompt streams the offline reply');
    mark('stop-initial');
    await vscode.commands.executeCommand('mini-agent.stop');
    mark('saved-session-read');
    const names = fs.readdirSync(sessions).filter(name => name.endsWith('.json'));
    assert.equal(names.length, 1, 'one canonical durable session');
    const savedPath = path.join(sessions, names[0]);
    const before = JSON.parse(fs.readFileSync(savedPath, 'utf8'));
    mark('saved-session-assertions');
    assert.equal(before.messages[0].content, first, 'saved session retains the first literal user message');
    assert.equal(before.total_input_tokens, 20, 'first turn persists input usage');
    assert.equal(events().length, 1, 'first turn makes exactly one provider request');

    let settled = false;
    mark('resume-command');
    const resume = vscode.commands.executeCommand('mini-agent.resume');
    void resume.then(() => { settled = true; }, () => { settled = true; });
    const deadline = Date.now() + 15_000;
    mark('picker');
    while (!settled && Date.now() < deadline) {
      await delay(100);
      await vscode.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
    }
    mark('resume-settled');
    await within(resume, 1000, 'real saved-session picker');
    assert.equal(events().length, 1, 'list/load/picker do not request a completion');
    mark('start-resumed');
    const resumed = await installed.ensureSession(context);
    assert.ok(resumed, 'installed extension retains the resumed ACP session');
    const replayed = [];
    mark('prompt-resumed');
    assert.equal(await within(resumed.prompt('ACP53_CONTINUE_SAVED_CONTEXT', update => replayed.push(update), new AbortController().signal), 30_000, 'resumed prompt'), 'end_turn', 'resumed prompt completes normally');
    mark('replay-assertions');
    assert.ok(replayed.some(update => update.sessionUpdate === 'user_message_chunk' && update.content.text === first), 'saved literal user data is replayed through exact-ID session/update routing');
    assert.equal(events().length, 2, 'resumed prompt makes exactly one additional provider request');
    assert.ok(events()[1].messageHistory.some(row => row.role === 'user' && row.content === first), 'actual second wire request contains original canonical history');
    mark('stop-resumed');
    await vscode.commands.executeCommand('mini-agent.stop');
    mark('saved-final-read');
    const after = JSON.parse(fs.readFileSync(savedPath, 'utf8'));
    mark('saved-final-assertions');
    assert.equal(after.id, before.id, 'resume preserves the durable session ID');
    assert.equal(after.messages.length, 4, 'both complete turns are persisted');
    assert.equal(after.total_input_tokens, 40, 'input usage accumulates across resume');
    assert.equal(after.total_output_tokens, 10, 'output usage accumulates across resume');
    assert.deepEqual(after.permission_allowlist, [], 'resume does not create permission grants');
    console.log('IDE_ACP53_RESUME_PASS actual installed extension, picker, public load/prompt, same durable ID and 2 loopback requests');
  } catch (error) {
    failed = true;
    reportFailure(stage, error);
    throw error;
  } finally {
    if (restoreConfig) {
      let cleanupFailed = false;
      let cleanupError;
      for (const [cleanupStage, action] of [
        ['cleanup-stop', () => vscode.commands.executeCommand('mini-agent.stop')],
        ['cleanup-config', () => config.update('executablePath', original, vscode.ConfigurationTarget.Global)],
      ]) {
        mark(cleanupStage);
        try { await action(); } catch (error) {
          reportFailure(cleanupStage, error);
          if (!cleanupFailed) { cleanupFailed = true; cleanupError = error; }
        }
      }
      if (cleanupFailed && !failed) { throw cleanupError; }
    }
  }
};
