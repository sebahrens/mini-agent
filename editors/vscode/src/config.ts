import { promises as fs } from 'node:fs';
import * as path from 'node:path';

const CONFIG_FILENAMES = ['config.toml', 'config.yaml', 'config.yml', 'config.json'] as const;

export const SAFE_CONFIG_TEMPLATE = `# Mini Agent configuration
#
# Add settings here when needed. Configuration options are documented at:
# https://github.com/sebahrens/mini-agent/blob/main/docs/agent/CONFIG.md
`;

type ConfigEnvironment = Readonly<Record<string, string | undefined>>;

function pathApi(platform: NodeJS.Platform): path.PlatformPath {
  return platform === 'win32' ? path.win32 : path.posix;
}

function expandHome(value: string, homeDirectory: string, paths: path.PlatformPath): string {
  if (value === '~') { return homeDirectory; }
  if (value.startsWith('~/') || value.startsWith('~\\')) {
    return paths.join(homeDirectory, value.slice(2));
  }
  return value;
}

/** Resolve the same legacy `zerostack` configuration root as Rust `AppPaths`. */
export function resolveConfigDirectory(
  platform: NodeJS.Platform,
  homeDirectory: string,
  environment: ConfigEnvironment,
): string {
  const paths = pathApi(platform);
  const override = environment.ZS_CONFIG_DIR;
  if (override !== undefined) {
    if (override.length === 0) { throw new Error('ZS_CONFIG_DIR must not be empty.'); }
    const expanded = expandHome(override, homeDirectory, paths);
    if (!paths.isAbsolute(expanded)) {
      throw new Error('ZS_CONFIG_DIR must resolve to an absolute path.');
    }
    return paths.normalize(expanded);
  }

  if (platform === 'linux') {
    const xdg = environment.XDG_CONFIG_HOME;
    const base = xdg && paths.isAbsolute(xdg) ? xdg : paths.join(homeDirectory, '.config');
    return paths.join(base, 'zerostack');
  }
  if (platform === 'darwin') {
    return paths.join(homeDirectory, 'Library', 'Application Support', 'zerostack');
  }
  if (platform === 'win32') {
    const roaming = environment.APPDATA;
    const base = roaming && paths.isAbsolute(roaming)
      ? roaming
      : paths.join(homeDirectory, 'AppData', 'Roaming');
    return paths.join(base, 'zerostack');
  }
  throw new Error(`Mini Agent does not support configuration paths on ${platform}.`);
}

async function existingConfig(configDirectory: string): Promise<string | undefined> {
  for (const filename of CONFIG_FILENAMES) {
    const candidate = path.join(configDirectory, filename);
    try {
      const metadata = await fs.lstat(candidate);
      if (metadata.isSymbolicLink() || !metadata.isFile()) {
        throw new Error(`Mini Agent config is not a regular file: ${candidate}`);
      }
      return candidate;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') { throw error; }
    }
  }
  return undefined;
}

async function rejectUnsafeConfigRoot(configDirectory: string): Promise<void> {
  try {
    const metadata = await fs.lstat(configDirectory);
    if (metadata.isSymbolicLink()) {
      throw new Error(`Mini Agent config root is a symbolic link: ${configDirectory}`);
    }
    if (!metadata.isDirectory()) {
      throw new Error(`Mini Agent config root is not a regular directory: ${configDirectory}`);
    }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') { throw error; }
  }
}

/** Select the active config format or create an inert, owner-private TOML file. */
export async function ensureConfigFile(configDirectory: string): Promise<string> {
  await rejectUnsafeConfigRoot(configDirectory);
  const selected = await existingConfig(configDirectory);
  if (selected) { return selected; }

  await fs.mkdir(configDirectory, { recursive: true, mode: 0o700 });
  await rejectUnsafeConfigRoot(configDirectory);

  const configPath = path.join(configDirectory, CONFIG_FILENAMES[0]);
  let handle: Awaited<ReturnType<typeof fs.open>>;
  try {
    handle = await fs.open(configPath, 'wx', 0o600);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'EEXIST') {
      const raced = await existingConfig(configDirectory);
      if (raced) { return raced; }
    }
    throw error;
  }

  let complete = false;
  try {
    await handle.writeFile(SAFE_CONFIG_TEMPLATE, 'utf8');
    await handle.sync();
    complete = true;
  } finally {
    await handle.close();
    if (!complete) {
      await fs.unlink(configPath).catch(() => undefined);
    }
  }
  return configPath;
}
