import * as fs from 'node:fs';
import * as path from 'node:path';

export type ExecutableResolution =
  | { readonly ok: true; readonly path: string }
  | { readonly ok: false; readonly reason: string };

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

/** How a bare command name is found: the environment and a file probe. */
export interface PathLookup {
  readonly env: Readonly<Record<string, string | undefined>>;
  /** True when `candidate` (an absolute path) is a file this platform can execute. */
  readonly isExecutableFile: (candidate: string) => boolean;
}

/** The extension host's own PATH and filesystem. */
export function hostPathLookup(platform: NodeJS.Platform = process.platform): PathLookup {
  return {
    env: process.env,
    isExecutableFile: candidate => {
      try {
        if (!fs.statSync(candidate).isFile()) { return false; }
        if (platform !== 'win32') { fs.accessSync(candidate, fs.constants.X_OK); }
        return true;
      } catch {
        return false;
      }
    },
  };
}

function envValue(
  env: Readonly<Record<string, string | undefined>>,
  name: string,
  platform: NodeJS.Platform,
): string | undefined {
  if (platform !== 'win32') { return env[name]; }
  // Windows environment names are case-insensitive (`Path`, `PATH`).
  const key = Object.keys(env).find(candidate => candidate.toUpperCase() === name);
  return key === undefined ? undefined : env[key];
}

/**
 * A PATH entry that names one fixed directory. Relative entries (including
 * the empty entry, which means the current directory) and, on Windows,
 * drive-relative (`C:bin`) or root-relative (`\bin`) entries depend on the
 * current directory or drive, which for the spawned agent is the workspace,
 * so they are never searched.
 */
function isFixedDirectory(entry: string, platform: NodeJS.Platform): boolean {
  if (platform === 'win32') {
    return /^[a-zA-Z]:[\\/]/.test(entry) || /^[\\/]{2}[^\\/]/.test(entry);
  }
  return path.posix.isAbsolute(entry);
}

/**
 * Extensions tried for a bare Windows command: the PATHEXT entries that
 * `child_process.spawn` can start without a shell (`.exe`, `.com`), in
 * PATHEXT order. A name that already carries a PATHEXT extension is tried as
 * written.
 */
function windowsExtensions(command: string, pathext: string | undefined): string[] {
  const listed = (pathext ?? '.COM;.EXE;.BAT;.CMD')
    .split(';')
    .map(extension => extension.trim().toLowerCase())
    .filter(extension => extension.startsWith('.'));
  const lower = command.toLowerCase();
  if (listed.some(extension => lower.endsWith(extension))) { return ['']; }
  const spawnable = listed.filter(extension => extension === '.exe' || extension === '.com');
  return spawnable.length > 0 ? spawnable : ['.exe'];
}

/**
 * Resolve a bare command name against PATH only, without ever consulting the
 * current directory: each fixed PATH directory in order and, on Windows, each
 * spawnable PATHEXT extension. Returns the absolute path of the first
 * executable file, or undefined.
 */
export function findOnPath(
  command: string,
  platform: NodeJS.Platform,
  lookup: PathLookup,
): string | undefined {
  const paths = pathApi(platform);
  const delimiter = platform === 'win32' ? ';' : ':';
  const extensions = platform === 'win32'
    ? windowsExtensions(command, envValue(lookup.env, 'PATHEXT', platform))
    : [''];
  for (const raw of (envValue(lookup.env, 'PATH', platform) ?? '').split(delimiter)) {
    const entry = platform === 'win32' ? raw.trim().replace(/^"(.*)"$/, '$1') : raw;
    if (!isFixedDirectory(entry, platform)) { continue; }
    for (const extension of extensions) {
      const candidate = paths.normalize(paths.join(entry, command + extension));
      if (lookup.isExecutableFile(candidate)) { return candidate; }
    }
  }
  return undefined;
}

/**
 * Normalize the user's `mini-agent.executablePath` setting into an absolute
 * path that `child_process.spawn` runs the same way for the `--version`
 * probe and the `--acp` launch, whatever their working directory:
 *  - `~` / `~/...` is expanded against the home directory,
 *  - absolute paths are normalized,
 *  - a bare command name (no separator) is resolved once against PATH only,
 *    never against the workspace or any other current directory (on Windows
 *    libuv would otherwise search the child's cwd, the workspace, first),
 *  - a relative path containing a separator is rejected, because it would be
 *    resolved against whatever cwd the extension host happens to have.
 */
export function resolveConfiguredExecutable(
  configured: string,
  homeDirectory: string,
  platform: NodeJS.Platform = process.platform,
  lookup: PathLookup = hostPathLookup(platform),
): ExecutableResolution {
  const paths = pathApi(platform);
  const trimmed = configured.trim();
  if (trimmed.length === 0) {
    return { ok: false, reason: 'mini-agent.executablePath is empty.' };
  }

  const expanded = expandHome(trimmed, homeDirectory, paths);
  if (expanded.startsWith('~')) {
    return {
      ok: false,
      reason: `mini-agent.executablePath "${configured}" uses an unsupported "~user" form; use an absolute path.`,
    };
  }
  if (paths.isAbsolute(expanded)) {
    return { ok: true, path: paths.normalize(expanded) };
  }

  // On Windows a drive prefix (`C:mini-agent.exe`) is relative to that
  // drive's current directory, so it counts as a relative path too.
  const hasSeparator = expanded.includes('/') || (platform === 'win32' && /[\\:]/.test(expanded));
  if (hasSeparator) {
    return {
      ok: false,
      reason: `mini-agent.executablePath "${configured}" is a relative path; use an absolute path (or "~/...") or a bare command name on PATH.`,
    };
  }
  const found = findOnPath(expanded, platform, lookup);
  if (found === undefined) {
    return {
      ok: false,
      reason: `mini-agent.executablePath "${configured}" was not found on PATH; use an absolute path.`,
    };
  }
  return { ok: true, path: found };
}
