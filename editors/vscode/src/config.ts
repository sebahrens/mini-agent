import { lstatSync, promises as fs } from 'node:fs';
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

/** Reports whether a path exists without following a final symbolic link. */
export type PathProbe = (candidate: string) => boolean;

function pathExists(candidate: string): boolean {
  try {
    lstatSync(candidate);
    return true;
  } catch {
    return false;
  }
}

function overridePath(
  variable: string,
  value: string | undefined,
  homeDirectory: string,
  paths: path.PlatformPath,
): string | undefined {
  if (value === undefined) { return undefined; }
  if (value.length === 0) { throw new Error(`${variable} must not be empty.`); }
  const expanded = expandHome(value, homeDirectory, paths);
  if (!paths.isAbsolute(expanded)) {
    throw new Error(`${variable} must resolve to an absolute path.`);
  }
  return paths.normalize(expanded);
}

/** Legacy per-OS `zerostack` roots whose presence marks an existing install. */
function legacyRoots(
  platform: NodeJS.Platform,
  homeDirectory: string,
  environment: ConfigEnvironment,
  paths: path.PlatformPath,
): { config: string; markers: string[] } {
  if (platform === 'linux') {
    const xdgConfig = environment.XDG_CONFIG_HOME;
    const configBase = xdgConfig && paths.isAbsolute(xdgConfig)
      ? xdgConfig
      : paths.join(homeDirectory, '.config');
    const xdgData = environment.XDG_DATA_HOME;
    const dataBase = xdgData && paths.isAbsolute(xdgData)
      ? xdgData
      : paths.join(homeDirectory, '.local', 'share');
    const config = paths.join(configBase, 'zerostack');
    return { config, markers: [config, paths.join(dataBase, 'zerostack')] };
  }
  if (platform === 'darwin') {
    const config = paths.join(homeDirectory, 'Library', 'Application Support', 'zerostack');
    return { config, markers: [config] };
  }
  if (platform === 'win32') {
    const roaming = environment.APPDATA;
    const roamingBase = roaming && paths.isAbsolute(roaming)
      ? roaming
      : paths.join(homeDirectory, 'AppData', 'Roaming');
    const local = environment.LOCALAPPDATA;
    const localBase = local && paths.isAbsolute(local)
      ? local
      : paths.join(homeDirectory, 'AppData', 'Local');
    const config = paths.join(roamingBase, 'zerostack');
    return { config, markers: [config, paths.join(localBase, 'zerostack')] };
  }
  throw new Error(`Mini Agent does not support configuration paths on ${platform}.`);
}

/**
 * Resolve the same configuration root as Rust `AppPaths`:
 * `ZS_CONFIG_DIR`, then `MINI_AGENT_HOME`, then `~/.mini-agent` when it exists
 * or no legacy `zerostack` root exists, otherwise the legacy per-OS root.
 */
export function resolveConfigDirectory(
  platform: NodeJS.Platform,
  homeDirectory: string,
  environment: ConfigEnvironment,
  exists: PathProbe = pathExists,
): string {
  const paths = pathApi(platform);
  const configOverride = overridePath(
    'ZS_CONFIG_DIR', environment.ZS_CONFIG_DIR, homeDirectory, paths,
  );
  if (configOverride !== undefined) { return configOverride; }
  const homeOverride = overridePath(
    'MINI_AGENT_HOME', environment.MINI_AGENT_HOME, homeDirectory, paths,
  );
  if (homeOverride !== undefined) { return homeOverride; }

  const legacy = legacyRoots(platform, homeDirectory, environment, paths);
  const globalHome = paths.join(homeDirectory, '.mini-agent');
  if (exists(globalHome) || !legacy.markers.some(marker => exists(marker))) {
    return globalHome;
  }
  return legacy.config;
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
