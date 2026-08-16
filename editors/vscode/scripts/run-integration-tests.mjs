import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { runVSCodeCommand } from '@vscode/test-electron';
import { TARGETS } from './platform.mjs';

const [, , vsixArgument, target] = process.argv;
if (!vsixArgument || !TARGETS[target]) {
  throw new Error('Usage: node scripts/run-integration-tests.mjs <candidate.vsix> <target>');
}

const platform = target === 'win32-x64'
  ? 'win32-x64-archive'
  : target === 'darwin-x64'
    ? 'darwin'
    : target;
const options = {
  version: '1.90.2',
  platform,
  reuseMachineInstall: false,
};
const vsix = resolve(vsixArgument);
const manifest = JSON.parse(readFileSync(resolve('package.json'), 'utf8'));

await runVSCodeCommand(['--install-extension', vsix, '--force'], options);
const { stdout } = await runVSCodeCommand(['--list-extensions', '--show-versions'], options);
if (!stdout.split(/\r?\n/).includes(`mini-agent.mini-agent@${manifest.version}`)) {
  throw new Error(`Installed candidate was not listed by clean VS Code:\n${stdout}`);
}
console.log(`Installed and discovered ${vsix} in clean VS Code 1.90.2`);
