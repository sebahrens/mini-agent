import { mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { downloadAndUnzipVSCode, runTests, runVSCodeCommand } from '@vscode/test-electron';
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
const version = '1.91.1';
const extensionRoot = process.cwd();
const vsix = resolve(vsixArgument);
const manifest = JSON.parse(readFileSync(resolve('package.json'), 'utf8'));
const cachePath = join(realpathSync(tmpdir()), 'mini-agent-vscode-cache');
const vscodeExecutablePath = await downloadAndUnzipVSCode({ version, platform, cachePath });
const options = { version, platform, cachePath, reuseMachineInstall: false };
// macOS VS Code embeds this path in a Unix socket with a 103-byte limit.
const profileBase = process.platform === 'darwin' ? '/private/tmp' : realpathSync(tmpdir());
const testRoot = mkdtempSync(join(profileBase, 'ma-vs-'));
const workspace = join(testRoot, 'workspace');
const testExtension = join(testRoot, 'test-extension');
const profileRoot = join(testRoot, 'profile');
const profileArgs = [
  `--extensions-dir=${join(profileRoot, 'extensions')}`,
  `--user-data-dir=${join(profileRoot, 'user-data')}`,
];
mkdirSync(workspace);
mkdirSync(testExtension);
writeFileSync(join(testExtension, 'package.json'), JSON.stringify({
  name: 'mini-agent-integration-runner',
  publisher: 'mini-agent-tests',
  version: '0.0.1',
  engines: { vscode: '^1.91.0' },
}));
try {
  // The VS Code CLI puts its clean profile under the current directory.
  process.chdir(testRoot);
  await runVSCodeCommand([...profileArgs, '--install-extension', vsix, '--force'], options);
  const { stdout } = await runVSCodeCommand([...profileArgs, '--list-extensions', '--show-versions'], options);
  if (!stdout.split(/\r?\n/).includes(`mini-agent.mini-agent@${manifest.version}`)) {
    throw new Error(`Installed candidate was not listed by clean VS Code:\n${stdout}`);
  }
  console.log(`Installed and discovered ${vsix} in clean VS Code ${version}`);

  // Install/discovery alone misses activation and ACP startup failures. The
  // host suite sends no model prompt and has only a dummy provider key.
  await runTests({
    vscodeExecutablePath,
    platform: options.platform,
    extensionDevelopmentPath: testExtension,
    extensionTestsPath: resolve(extensionRoot, 'test/integration.cjs'),
    launchArgs: [workspace, '--disable-workspace-trust', ...profileArgs],
    extensionTestsEnv: {
      MINI_AGENT_HOME: join(testRoot, 'config'),
      MINI_AGENT_VSIX_PROFILE_ROOT: join(profileRoot, 'extensions'),
      OPENROUTER_API_KEY: 'unused-vscode-host-smoke-key',
      ...(process.env.ZS_LOCAL_DATA_DIR
        ? { ZS_LOCAL_DATA_DIR: process.env.ZS_LOCAL_DATA_DIR }
        : {}),
    },
  });
} finally {
  process.chdir(extensionRoot);
  rmSync(testRoot, { recursive: true, force: true });
}
