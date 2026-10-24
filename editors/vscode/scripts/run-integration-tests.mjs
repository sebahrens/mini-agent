import { appendFileSync, mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { downloadAndUnzipVSCode, runTests, runVSCodeCommand } from '@vscode/test-electron';
import { TARGETS } from './platform.mjs';
import { createIntegrationFixture } from './integration-fixture.mjs';

let runnerStage = 'startup';
let reportedFailure;
function stage(value) {
  runnerStage = value;
  console.error(`IDE_HOST_RUNNER_STAGE=${value}`);
}
function reportFailure(error) {
  const code = error?.code;
  const category = code === 'ERR_ASSERTION' || error?.name === 'AssertionError' ? 'assertion'
    : code === 'ETIMEDOUT' || error?.name === 'TimeoutError' || /timed? out/i.test(error?.message ?? '') ? 'timeout'
    : code === 'ENOENT' ? 'missing'
    : code === 'EACCES' || code === 'EPERM' ? 'permission'
    : code === 'EPIPE' ? 'pipe'
    : code === 'ECONNRESET' ? 'connection-reset'
    : error?.name === 'SyntaxError' ? 'json'
    : error?.name === 'TypeError' ? 'type'
    : error?.constructor?.name === 'TestRunFailedError' ? 'host-test'
    : error?.constructor?.name === 'VSCodeCommandError' ? 'vscode-command'
    : typeof code === 'number' || typeof error?.exitCode === 'number' || typeof error?.status === 'number' ? 'process'
    : 'error';
  const value = typeof code === 'number' ? code : typeof error?.exitCode === 'number' ? error.exitCode : error?.status;
  const exit = Number.isInteger(value) && value >= -2147483648 && value <= 2147483647 ? value : 'unknown';
  const signal = ['SIGTERM', 'SIGKILL', 'SIGABRT', 'SIGSEGV', 'SIGINT', 'SIGBREAK'].includes(error?.signal) ? error.signal : 'none';
  console.error(`IDE_HOST_RUNNER_FAILED stage=${runnerStage} category=${category} exit=${exit} signal=${signal}`);
  reportedFailure = error;
}

try {
  stage('startup');
  const [, , vsixArgument, target, testScenario = 'smoke'] = process.argv;
  if (!vsixArgument || !TARGETS[target] || !['smoke', 'resume'].includes(testScenario)) {
    throw new Error('Usage: node scripts/run-integration-tests.mjs <candidate.vsix> <target> [smoke|resume]');
  }

  stage('configuration');
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
  stage('vscode-download');
  const vscodeExecutablePath = await downloadAndUnzipVSCode({ version, platform, cachePath });
  const options = { version, platform, cachePath, reuseMachineInstall: false };
  // macOS VS Code embeds this path in a Unix socket with a 103-byte limit.
  const profileBase = process.platform === 'darwin' ? '/private/tmp' : realpathSync(tmpdir());
  stage('fixture-create');
  const fixture = createIntegrationFixture(profileBase);
  const testRoot = fixture.root;
  const workspace = join(testRoot, 'workspace');
  const testExtension = join(testRoot, 'test-extension');
  const profileRoot = join(testRoot, 'profile');
  const configRoot = fixture.configRoot;
  const providerEvents = join(testRoot, 'provider-events.jsonl');
  const fixtureFile = join(workspace, 'fixture.txt');
  const profileArgs = [
    `--extensions-dir=${join(profileRoot, 'extensions')}`,
    `--user-data-dir=${join(profileRoot, 'user-data')}`,
  ];
  stage('workspace-setup');
  mkdirSync(workspace);
  writeFileSync(fixtureFile, 'OFFLINE_IDE_FIXTURE\n');
  mkdirSync(testExtension);
  const provider = createServer(async (request, response) => {
    if (request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end('{"data":[]}');
      return;
    }
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
      ...(testScenario === 'resume' ? { messageHistory: payload.messages } : {}),
    }) + '\n');
    const chunk = (delta, finishReason) => JSON.stringify({
      id: 'offline-ide-turn', object: 'chat.completion.chunk', created: 0,
      model: 'offline-ide',
      choices: [{ index: 0, delta, finish_reason: finishReason }],
      ...(testScenario === 'resume' && finishReason === 'stop'
        ? { usage: { prompt_tokens: 20, completion_tokens: 5, total_tokens: 25 } } : {}),
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
  try {
    stage('provider-listen');
    await new Promise((resolveListen, rejectListen) => {
      provider.once('error', rejectListen);
      provider.listen(0, '127.0.0.1', resolveListen);
    });
    const port = provider.address().port;
    stage('config-private-init');
    const privateInitialization = fixture.writeConfig([
      'provider = "offline-ide"',
      'model = "offline-ide"',
      'max_tokens = 256',
      'session_title_model = "off"',
      'default_permission_mode = "restrictive"',
      '[custom_providers.offline-ide]',
      'provider_type = "openai"',
      `base_url = "http://127.0.0.1:${port}/v1"`,
      'api_key_env = "MINI_AGENT_IDE_FIXTURE_KEY"',
      'api_style = "completions"',
      '',
    ].join('\n'));
    if (privateInitialization) {
      console.log(`Windows owned integration fixture ACL/readback: ${JSON.stringify(privateInitialization)}`);
    }
    stage('test-extension-write');
    writeFileSync(join(testExtension, 'package.json'), JSON.stringify({
      name: 'mini-agent-integration-runner',
      publisher: 'mini-agent-tests',
      version: '0.0.1',
      engines: { vscode: '^1.91.0' },
    }));
    // The VS Code CLI puts its clean profile under the current directory.
    process.chdir(testRoot);
    stage('vsix-install');
    await runVSCodeCommand([...profileArgs, '--install-extension', vsix, '--force'], options);
    stage('vsix-discovery');
    const { stdout } = await runVSCodeCommand([...profileArgs, '--list-extensions', '--show-versions'], options);
    if (!stdout.split(/\r?\n/).includes(`mini-agent.mini-agent@${manifest.version}`)) {
      throw new Error(`Installed candidate was not listed by clean VS Code:\n${stdout}`);
    }
    console.log(`Installed and discovered ${vsix} in clean VS Code ${version}`);

    // Install/discovery alone misses activation and ACP startup failures. The
    // host suite sends prompts only to the loopback scripted provider.
    stage('host-tests');
    await runTests({
      vscodeExecutablePath,
      platform: options.platform,
      extensionDevelopmentPath: testExtension,
      extensionTestsPath: resolve(extensionRoot, testScenario === 'resume' ? 'test/resume.integration.cjs' : 'test/integration.cjs'),
      launchArgs: [workspace, '--disable-workspace-trust', ...profileArgs],
      extensionTestsEnv: {
        MINI_AGENT_HOME: configRoot,
        ZS_CONFIG_DIR: configRoot,
        ZS_DATA_DIR: configRoot,
        ZS_LOCAL_DATA_DIR: configRoot,
        ZS_STATE_DIR: configRoot,
        ZS_CACHE_DIR: configRoot,
        ZS_CREDENTIALS_DIR: configRoot,
        MINI_AGENT_VSIX_PROFILE_ROOT: join(profileRoot, 'extensions'),
        MINI_AGENT_IDE_PROVIDER_EVENTS: providerEvents,
        MINI_AGENT_IDE_FIXTURE_KEY: 'unused-local-fixture-key',
        OPENROUTER_API_KEY: 'unused-vscode-host-smoke-key',
        ...(process.env.ZS_LOCAL_DATA_DIR
          ? { ZS_LOCAL_DATA_DIR: process.env.ZS_LOCAL_DATA_DIR }
          : {}),
      },
    });
  } catch (error) {
    reportFailure(error);
    throw error;
  } finally {
    stage('cleanup');
    process.chdir(extensionRoot);
    provider.closeAllConnections();
    await new Promise(resolveClose => provider.close(resolveClose));
    rmSync(testRoot, { recursive: true, force: true });
  }
} catch (error) {
  if (error !== reportedFailure) { reportFailure(error); }
  throw error;
}
