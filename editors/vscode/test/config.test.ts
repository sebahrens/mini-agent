import { promises as fs } from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import {
  SAFE_CONFIG_TEMPLATE,
  ensureConfigFile,
  resolveConfigDirectory,
} from '../src/config';

const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(temporaryDirectories.splice(0).map(directory =>
    fs.rm(directory, { recursive: true, force: true }),
  ));
});

describe('Mini Agent config paths', () => {
  const legacyInstall = (...existing: string[]) => (candidate: string) =>
    existing.includes(candidate);
  const nothingExists = () => false;

  it('keeps the legacy platform roots for an existing zerostack install', () => {
    expect(resolveConfigDirectory('linux', '/home/alice', {},
      legacyInstall('/home/alice/.config/zerostack')))
      .toBe('/home/alice/.config/zerostack');
    expect(resolveConfigDirectory('linux', '/home/alice', {},
      legacyInstall('/home/alice/.local/share/zerostack')))
      .toBe('/home/alice/.config/zerostack');
    expect(resolveConfigDirectory('darwin', '/Users/alice', {},
      legacyInstall('/Users/alice/Library/Application Support/zerostack')))
      .toBe('/Users/alice/Library/Application Support/zerostack');
    expect(resolveConfigDirectory('win32', 'C:\\Users\\Alice', { APPDATA: 'D:\\Roaming' },
      legacyInstall('D:\\Roaming\\zerostack')))
      .toBe('D:\\Roaming\\zerostack');
  });

  it('uses ~/.mini-agent for fresh installs or once it exists', () => {
    expect(resolveConfigDirectory('linux', '/home/alice', {}, nothingExists))
      .toBe('/home/alice/.mini-agent');
    expect(resolveConfigDirectory('darwin', '/Users/alice', {}, nothingExists))
      .toBe('/Users/alice/.mini-agent');
    expect(resolveConfigDirectory('win32', 'C:\\Users\\Alice', {}, nothingExists))
      .toBe('C:\\Users\\Alice\\.mini-agent');
    expect(resolveConfigDirectory('linux', '/home/alice', {}, legacyInstall(
      '/home/alice/.config/zerostack', '/home/alice/.mini-agent',
    ))).toBe('/home/alice/.mini-agent');
  });

  it('honors MINI_AGENT_HOME below ZS_CONFIG_DIR', () => {
    const legacy = legacyInstall('/home/alice/.config/zerostack');
    expect(resolveConfigDirectory('linux', '/home/alice',
      { MINI_AGENT_HOME: '~/agent-home' }, legacy))
      .toBe('/home/alice/agent-home');
    expect(resolveConfigDirectory('linux', '/home/alice',
      { MINI_AGENT_HOME: '~/agent-home', ZS_CONFIG_DIR: '/zs/config' }, legacy))
      .toBe('/zs/config');
    expect(() => resolveConfigDirectory('linux', '/home/alice', { MINI_AGENT_HOME: '' }))
      .toThrow(/empty/);
    expect(() => resolveConfigDirectory('linux', '/home/alice', { MINI_AGENT_HOME: 'rel' }))
      .toThrow(/absolute/);
  });

  it('honors an absolute or home-relative ZS_CONFIG_DIR override', () => {
    expect(resolveConfigDirectory('linux', '/home/alice', { ZS_CONFIG_DIR: '~/agent-config' }))
      .toBe('/home/alice/agent-config');
    expect(() => resolveConfigDirectory('linux', '/home/alice', { ZS_CONFIG_DIR: 'relative' }))
      .toThrow(/absolute/);
  });
});

describe('ensureConfigFile', () => {
  it('creates a private inert TOML template without replacing an existing config', async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mini-agent-vscode-config-'));
    temporaryDirectories.push(root);

    const created = await ensureConfigFile(root);
    expect(created).toBe(path.join(root, 'config.toml'));
    expect(await fs.readFile(created, 'utf8')).toBe(SAFE_CONFIG_TEMPLATE);
    expect(SAFE_CONFIG_TEMPLATE.split('\n').filter(Boolean).every(line => line.startsWith('#')))
      .toBe(true);

    const yaml = path.join(root, 'config.yaml');
    await fs.rm(created);
    await fs.writeFile(yaml, 'model: custom\n');
    expect(await ensureConfigFile(root)).toBe(yaml);
    expect(await fs.readFile(yaml, 'utf8')).toBe('model: custom\n');
  });

  it('rejects a config directory reached through a symbolic link', async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), 'mini-agent-vscode-config-link-'));
    temporaryDirectories.push(root);
    const realDirectory = path.join(root, 'real');
    const linkedDirectory = path.join(root, 'linked');
    await fs.mkdir(realDirectory);
    await fs.writeFile(path.join(realDirectory, 'config.toml'), '# existing\n');
    await fs.symlink(realDirectory, linkedDirectory, 'dir');

    await expect(ensureConfigFile(linkedDirectory)).rejects.toThrow(/symbolic link/);
    expect(await fs.readFile(path.join(realDirectory, 'config.toml'), 'utf8')).toBe('# existing\n');
  });
});
