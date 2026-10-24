import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

const host = vi.hoisted(() => ({ download: vi.fn(), command: vi.fn(), tests: vi.fn() }));
vi.mock('@vscode/test-electron', () => ({
  downloadAndUnzipVSCode: host.download, runVSCodeCommand: host.command, runTests: host.tests,
}));
vi.mock('node:http', async () => {
  const { EventEmitter } = await import('node:events');
  class OfflineServer extends EventEmitter {
    listen(_port: number, _address: string, callback: () => void) { callback(); }
    address() { return { port: 54321 }; }
    closeAllConnections() {}
    close(callback: () => void) { callback(); }
  }
  return { createServer: () => new OfflineServer() };
});
const originalArguments = process.argv;
let errors: ReturnType<typeof vi.spyOn>;
const runner: string = '../scripts/run-integration-tests.mjs';

beforeEach(() => {
  vi.resetModules();
  host.download.mockReset().mockResolvedValue('/offline/Code');
  host.command.mockReset();
  host.tests.mockReset();
  process.argv = ['node', 'runner.mjs', 'offline.vsix', 'win32-x64', 'resume'];
  errors = vi.spyOn(console, 'error').mockImplementation(() => {});
});
afterEach(() => { process.argv = originalArguments; vi.restoreAllMocks(); });

it('reports invalid startup before any fixture or VS Code launch', async () => {
  process.argv = ['node', 'runner.mjs'];
  await expect(import(runner)).rejects.toThrow('Usage:');
  expect(errors).toHaveBeenCalledWith('IDE_HOST_RUNNER_FAILED stage=startup category=error exit=unknown signal=none');
  expect(host.download).not.toHaveBeenCalled();
});

it('reports a download failure without copying its private message', async () => {
  const failure = Object.assign(new Error('PRIVATE_PATH_AND_PROVIDER_CANARY'), { code: 'EACCES' });
  host.download.mockRejectedValue(failure);
  await expect(import(runner)).rejects.toBe(failure);
  expect(errors).toHaveBeenCalledWith('IDE_HOST_RUNNER_FAILED stage=vscode-download category=permission exit=unknown signal=none');
  expect(JSON.stringify(errors.mock.calls)).not.toContain('PRIVATE_PATH_AND_PROVIDER_CANARY');
});

it('retains the install phase and numeric launcher exit before cleanup runs', async () => {
  class VSCodeCommandError extends Error { exitCode = 7; }
  const failure = Object.assign(new VSCodeCommandError('PRIVATE_COMMAND_OUTPUT'), { signal: 'SIGTERM' });
  host.command.mockRejectedValue(failure);
  await expect(import(runner)).rejects.toBe(failure);
  expect(errors).toHaveBeenCalledWith('IDE_HOST_RUNNER_FAILED stage=vsix-install category=vscode-command exit=7 signal=SIGTERM');
  expect(errors).toHaveBeenCalledWith('IDE_HOST_RUNNER_STAGE=cleanup');
  expect(host.tests).not.toHaveBeenCalled();
  expect(JSON.stringify(errors.mock.calls)).not.toContain('PRIVATE_COMMAND_OUTPUT');
});

it('passes distinct data/state roots to the actual host runner and leaves private data creation to Rust', async () => {
  const version = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version;
  host.command.mockResolvedValueOnce({ stdout: '' }).mockResolvedValueOnce({ stdout: `mini-agent.mini-agent@${version}` });
  const stopped = new Error('offline host boundary reached');
  host.tests.mockImplementation(async (options: { extensionTestsEnv: Record<string, string> }) => {
    const environment = options.extensionTestsEnv;
    expect(environment.ZS_STATE_DIR).toBe(environment.ZS_CONFIG_DIR);
    expect(environment.ZS_DATA_DIR).toBe(join(dirname(environment.ZS_CONFIG_DIR), 'data'));
    expect(environment.ZS_DATA_DIR).not.toBe(environment.ZS_STATE_DIR);
    expect(existsSync(environment.ZS_DATA_DIR)).toBe(false);
    throw stopped;
  });
  await expect(import(runner)).rejects.toBe(stopped);
  expect(host.tests).toHaveBeenCalledOnce();
});
