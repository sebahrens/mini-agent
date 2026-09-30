import { appendFileSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
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
const version = process.env.MINI_AGENT_VSCODE_TEST_VERSION ?? '1.91.1';
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
const configRoot = join(testRoot, 'config');
const providerEvents = join(testRoot, 'provider-events.jsonl');
const fixtureFile = join(workspace, 'fixture.txt');
const profileArgs = [
  `--extensions-dir=${join(profileRoot, 'extensions')}`,
  `--user-data-dir=${join(profileRoot, 'user-data')}`,
];
mkdirSync(workspace);
writeFileSync(fixtureFile, 'OFFLINE_IDE_FIXTURE\n');
mkdirSync(testExtension);
mkdirSync(configRoot, { mode: 0o700 });
const provider = createServer(async (request, response) => {
  let body = '';
  for await (const chunk of request) { body += chunk.toString(); }
  const payload = JSON.parse(body);
  const userText = payload.messages?.findLast(message => message.role === 'user')?.content ?? '';
  const scenario = payload.messages?.at(-1)?.role === 'tool'
    ? 'TOOL_RESULT'
    : typeof userText === 'string' && userText.includes('PERMISSION_')
      ? userText.match(/PERMISSION_[A-Z]+/)?.[0]
      : 'REPLY';
  appendFileSync(providerEvents, JSON.stringify({
    path: request.url,
    messages: payload.messages?.length ?? 0,
    scenario,
    toolResult: payload.messages?.findLast(message => message.role === 'tool')?.content,
  }) + '\n');
  const chunk = (delta, finishReason) => JSON.stringify({
    id: 'offline-ide-turn', object: 'chat.completion.chunk', created: 0,
    model: 'offline-ide',
    choices: [{ index: 0, delta, finish_reason: finishReason }],
  });
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (scenario?.startsWith('PERMISSION_')) {
    const call = { index: 0, id: `fixture-${scenario}`, type: 'function', function: {
      name: 'read', arguments: JSON.stringify({ path: fixtureFile }),
    } };
    response.end(`data: ${chunk({ role: 'assistant', tool_calls: [call] }, null)}\n\n`
      + `data: ${chunk({}, 'tool_calls')}\n\ndata: [DONE]\n\n`);
  } else {
    response.end(`data: ${chunk({ role: 'assistant', content: scenario === 'TOOL_RESULT'
      ? 'OFFLINE_AFTER_TOOL' : 'OFFLINE_IDE_REPLY' }, null)}\n\n`
      + `data: ${chunk({}, 'stop')}\n\ndata: [DONE]\n\n`);
  }
});
await new Promise((resolveListen, rejectListen) => {
  provider.once('error', rejectListen);
  provider.listen(0, '127.0.0.1', resolveListen);
});
const port = provider.address().port;
writeFileSync(join(configRoot, 'config.toml'), [
  'provider = "offline-ide"',
  'model = "offline-ide"',
  'max_tokens = 256',
  'default_permission_mode = "restrictive"',
  '[custom_providers.offline-ide]',
  'provider_type = "openai"',
  `base_url = "http://127.0.0.1:${port}/v1"`,
  'api_key_env = "MINI_AGENT_IDE_FIXTURE_KEY"',
  'api_style = "completions"',
  '',
].join('\n'), { mode: 0o600 });
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
  // host suite sends prompts only to the loopback scripted provider.
  await runTests({
    vscodeExecutablePath,
    platform: options.platform,
    extensionDevelopmentPath: testExtension,
    extensionTestsPath: resolve(extensionRoot, 'test/integration.cjs'),
    launchArgs: [workspace, '--disable-workspace-trust', ...profileArgs],
    extensionTestsEnv: {
      MINI_AGENT_HOME: configRoot,
      MINI_AGENT_VSIX_PROFILE_ROOT: join(profileRoot, 'extensions'),
      MINI_AGENT_IDE_PROVIDER_EVENTS: providerEvents,
      MINI_AGENT_IDE_FIXTURE_KEY: 'unused-local-fixture-key',
      OPENROUTER_API_KEY: 'unused-vscode-host-smoke-key',
      ...(process.env.ZS_LOCAL_DATA_DIR
        ? { ZS_LOCAL_DATA_DIR: process.env.ZS_LOCAL_DATA_DIR }
        : {}),
    },
  });
} finally {
  process.chdir(extensionRoot);
  provider.closeAllConnections();
  await new Promise(resolveClose => provider.close(resolveClose));
  rmSync(testRoot, { recursive: true, force: true });
}
