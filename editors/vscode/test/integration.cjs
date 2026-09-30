// Runs inside a real VS Code extension host. The caller supplies a private
// config root and a dummy provider key, and no prompt is sent to a model.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vscode = require('vscode');

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

    console.log('IDE_HOST_SMOKE_PASS activation/start/restart/stop/config');
  } finally {
    try {
      await vscode.commands.executeCommand('mini-agent.stop');
    } finally {
      await config.update('executablePath', original, vscode.ConfigurationTarget.Global);
    }
  }
};
