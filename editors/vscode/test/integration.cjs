// Runs inside a real VS Code extension host. The caller supplies a private
// config root and a local scripted provider; no paid model is contacted.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vscode = require('vscode');

async function within(promise, milliseconds, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label} timed out`)), milliseconds);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function drivePermissionPicker(deny) {
  // The ACP tool-call notification precedes the real VS Code Quick Pick.
  await new Promise(resolve => setTimeout(resolve, 750));
  if (deny) {
    await vscode.commands.executeCommand('workbench.action.quickOpenSelectNext');
    await vscode.commands.executeCommand('workbench.action.quickOpenSelectNext');
  }
  await vscode.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
}

exports.run = async () => {
  const extension = vscode.extensions.getExtension('mini-agent.mini-agent');
  assert.ok(extension, 'Mini Agent extension is discoverable');
  const profileRoot = process.env.MINI_AGENT_VSIX_PROFILE_ROOT;
  assert.ok(profileRoot, 'host suite receives the isolated VSIX profile');
  const relativePath = path.relative(profileRoot, extension.extensionPath);
  assert.ok(relativePath && !relativePath.startsWith('..') && !path.isAbsolute(relativePath),
    'the extension under test is installed from the VSIX');
  await extension.activate();
  assert.ok(extension.isActive, 'Mini Agent extension activates');
  assert.ok(vscode.workspace.isTrusted, 'the fixture workspace is trusted');

  const config = vscode.workspace.getConfiguration('mini-agent');
  const original = config.inspect('executablePath')?.globalValue;
  try {
    // The binary in this VSIX is the same compiled candidate that passed the
    // ACP artifact smoke in package-target.mjs.
    const bundled = path.join(extension.extensionPath, 'bin',
      `${process.platform}-${process.arch}`,
      process.platform === 'win32' ? 'mini-agent.exe' : 'mini-agent');
    assert.ok(fs.existsSync(bundled), 'compiled ACP binary is bundled');
    await config.update('executablePath', bundled, vscode.ConfigurationTarget.Global);

    await vscode.commands.executeCommand('mini-agent.start');
    await vscode.commands.executeCommand('mini-agent.restart');
    await vscode.commands.executeCommand('mini-agent.stop');
    await vscode.commands.executeCommand('mini-agent.openConfig');

    const configPath = path.join(process.env.MINI_AGENT_HOME, 'config.toml');
    assert.ok(fs.existsSync(configPath), 'Open Config selects or creates the private config');
    assert.equal(vscode.window.activeTextEditor?.document.uri.fsPath, configPath);

    // The clean offline host cannot fetch VS Code's signed-in Chat registry.
    // Invoke the installed extension's production session path directly while
    // keeping the real extension host and compiled ACP binary in the loop.
    const installed = require(path.join(extension.extensionPath, 'dist', 'extension.js'));
    assert.equal(typeof installed.ensureSession, 'function');
    const active = await installed.ensureSession({
      extensionUri: extension.extensionUri,
      extension: { packageJSON: extension.packageJSON },
      subscriptions: [],
    });
    assert.ok(active, 'installed extension creates an ACP session');
    let reply = '';
    const cancellation = new AbortController();
    const stopReason = await within(active.prompt(
      'Reply to this offline IDE fixture.',
      update => {
        if (update.sessionUpdate === 'agent_message_chunk' && update.content.type === 'text') {
          reply += update.content.text;
        }
      },
      cancellation.signal,
    ), 30_000, 'offline IDE prompt').catch(error => {
      cancellation.abort();
      throw error;
    });
    assert.equal(stopReason, 'end_turn');
    assert.match(reply, /OFFLINE_IDE_REPLY/);
    const eventsPath = process.env.MINI_AGENT_IDE_PROVIDER_EVENTS;
    assert.ok(eventsPath, 'local provider events path is available');
    assert.ok(fs.existsSync(eventsPath), 'Chat submitted a real request to the local provider');
    const events = fs.readFileSync(eventsPath, 'utf8').trim().split('\n').map(JSON.parse);
    assert.ok(events.some(event => event.path === '/v1/chat/completions'),
      'the installed extension uses the local Chat Completions fixture');
    console.log('IDE_ACP_TURN_PASS local scripted provider and streamed reply');

    for (const deny of [false, true]) {
      const marker = deny ? 'PERMISSION_DENY' : 'PERMISSION_ALLOW';
      let pickerDriver;
      const updates = [];
      const controller = new AbortController();
      const before = fs.readFileSync(eventsPath, 'utf8').trim().split('\n').map(JSON.parse);
      const priorResults = before.filter(event => event.scenario === 'TOOL_RESULT').length;
      const permissionPrompt = active.prompt(
        `${marker}: read the local fixture file.`,
        update => {
          updates.push(update);
          if (update.sessionUpdate === 'tool_call' && !pickerDriver) {
            pickerDriver = drivePermissionPicker(deny);
          }
        },
        controller.signal,
      );
      const reason = await within(permissionPrompt, 30_000, `${marker} prompt`).catch(error => {
        controller.abort();
        throw error;
      });
      await pickerDriver;
      assert.equal(reason, 'end_turn');
      assert.ok(updates.some(update => update.sessionUpdate === 'tool_call'),
        `${marker} displays a tool call in the IDE session`);
      assert.ok(updates.some(update => update.sessionUpdate === 'agent_message_chunk'
        && update.content.type === 'text' && update.content.text.includes('OFFLINE_AFTER_TOOL')),
      `${marker} streams the final provider reply to the IDE session`);
      const after = fs.readFileSync(eventsPath, 'utf8').trim().split('\n').map(JSON.parse);
      assert.equal(after.filter(event => event.scenario === 'TOOL_RESULT').length,
        priorResults + 1, `${marker} sends exactly one tool result`);
      const result = after.findLast(event => event.scenario === 'TOOL_RESULT');
      assert.ok(result, `${marker} sends a tool result to the provider`);
      if (deny) {
        assert.doesNotMatch(String(result.toolResult), /OFFLINE_IDE_FIXTURE/);
      } else {
        assert.match(String(result.toolResult), /OFFLINE_IDE_FIXTURE/);
      }
      console.log(`IDE_PERMISSION_${deny ? 'DENY' : 'ALLOW'}_PASS real Quick Pick and tool effect`);
    }

    const beforeCancel = fs.readFileSync(eventsPath, 'utf8').trim().split('\n').map(JSON.parse);
    const cancelController = new AbortController();
    let cancelTimer;
    let sawCancelTool = false;
    const cancelled = active.prompt(
      'PERMISSION_CANCEL: read the local fixture file.',
      update => {
        if (update.sessionUpdate === 'tool_call' && !sawCancelTool) {
          sawCancelTool = true;
          cancelTimer = setTimeout(() => cancelController.abort(), 750);
        }
      },
      cancelController.signal,
    );
    let cancelReason;
    try {
      cancelReason = await within(cancelled, 30_000, 'permission cancellation');
    } catch (error) {
      cancelController.abort();
      throw error;
    } finally {
      clearTimeout(cancelTimer);
    }
    assert.ok(sawCancelTool, 'cancellation reaches the ACP permission tool call');
    assert.equal(cancelReason, 'cancelled');
    const afterCancel = fs.readFileSync(eventsPath, 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(afterCancel.filter(event => event.scenario === 'TOOL_RESULT').length,
      beforeCancel.filter(event => event.scenario === 'TOOL_RESULT').length,
      'cancelled permission does not submit a tool result to the provider');
    console.log('IDE_PERMISSION_CANCEL_PASS active ACP turn and picker cancellation');
    console.log('IDE_HOST_SMOKE_PASS activation/start/restart/stop/config');
  } finally {
    try {
      await vscode.commands.executeCommand('mini-agent.stop');
    } finally {
      await config.update('executablePath', original, vscode.ConfigurationTarget.Global);
    }
  }
};
