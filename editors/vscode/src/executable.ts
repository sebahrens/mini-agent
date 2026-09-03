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

/**
 * Normalize the user's `mini-agent.executablePath` setting into something
 * `child_process.spawn` can use without surprises:
 *  - `~` / `~/...` is expanded against the home directory,
 *  - absolute paths are normalized,
 *  - a bare command name (no separator) is kept for PATH lookup,
 *  - a relative path containing a separator is rejected, because it would be
 *    resolved against whatever cwd the extension host happens to have.
 */
export function resolveConfiguredExecutable(
  configured: string,
  homeDirectory: string,
  platform: NodeJS.Platform = process.platform,
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

  const hasSeparator = expanded.includes('/') || (platform === 'win32' && expanded.includes('\\'));
  if (hasSeparator) {
    return {
      ok: false,
      reason: `mini-agent.executablePath "${configured}" is a relative path; use an absolute path (or "~/...") or a bare command name on PATH.`,
    };
  }
  return { ok: true, path: expanded };
}
