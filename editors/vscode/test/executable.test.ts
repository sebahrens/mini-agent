import { describe, expect, it } from 'vitest';
import * as path from 'node:path';
import { resolveConfiguredExecutable, type PathLookup } from '../src/executable';

describe('resolveConfiguredExecutable', () => {
  it('expands a leading tilde against the home directory', () => {
    expect(resolveConfiguredExecutable('~/.cargo/bin/mini-agent', '/Users/alice', 'darwin'))
      .toEqual({ ok: true, path: '/Users/alice/.cargo/bin/mini-agent' });
    expect(resolveConfiguredExecutable('~', '/home/alice', 'linux'))
      .toEqual({ ok: true, path: '/home/alice' });
    expect(resolveConfiguredExecutable('~\\bin\\mini-agent.exe', 'C:\\Users\\Alice', 'win32'))
      .toEqual({ ok: true, path: 'C:\\Users\\Alice\\bin\\mini-agent.exe' });
  });

  it('normalizes absolute paths and trims whitespace', () => {
    expect(resolveConfiguredExecutable('  /usr/local/bin/../bin/mini-agent ', '/home/alice', 'linux'))
      .toEqual({ ok: true, path: '/usr/local/bin/mini-agent' });
    expect(resolveConfiguredExecutable('D:\\tools\\mini-agent.exe', 'C:\\Users\\Alice', 'win32'))
      .toEqual({ ok: true, path: 'D:\\tools\\mini-agent.exe' });
  });

  it('resolves a bare command name to an absolute path on PATH', () => {
    const files = new Set(['/usr/local/bin/mini-agent', '/usr/bin/mini-agent']);
    const lookup: PathLookup = {
      env: { PATH: 'bin::.:/usr/local/bin:/usr/bin' },
      isExecutableFile: candidate => files.has(candidate),
    };
    expect(resolveConfiguredExecutable('mini-agent', '/home/alice', 'linux', lookup))
      .toEqual({ ok: true, path: '/usr/local/bin/mini-agent' });
    const missing = resolveConfiguredExecutable('other-agent', '/home/alice', 'linux', lookup);
    expect(missing.ok).toBe(false);
    if (!missing.ok) { expect(missing.reason).toMatch(/not found on PATH/); }
  });

  it('never resolves a bare Windows command against the workspace or another current directory', () => {
    const workspace = 'C:\\workspace';
    // Every spelling the current directory could produce is "present", as
    // is a planted copy in the workspace; only C:\\Tools is a real PATH hit.
    const files = new Set([
      'mini-agent.exe',
      '.\\mini-agent.exe',
      'bin\\mini-agent.exe',
      '\\bin\\mini-agent.exe',
      'C:mini-agent.exe',
      `${workspace}\\mini-agent.exe`,
      'C:\\Tools\\mini-agent.bat',
      'C:\\Tools\\mini-agent.exe',
    ]);
    const probed: string[] = [];
    const lookup: PathLookup = {
      env: {
        Path: ';.;bin;\\bin;C:;"C:\\Tools"',
        PATHEXT: '.BAT;.EXE;.CMD',
      },
      isExecutableFile: candidate => {
        probed.push(candidate);
        return files.has(candidate);
      },
    };

    const resolved = resolveConfiguredExecutable('mini-agent', 'C:\\Users\\Alice', 'win32', lookup);
    expect(resolved).toEqual({ ok: true, path: 'C:\\Tools\\mini-agent.exe' });
    if (resolved.ok) {
      expect(path.win32.isAbsolute(resolved.path)).toBe(true);
      expect(resolved.path.toLowerCase().startsWith(workspace.toLowerCase())).toBe(false);
    }
    // Only the fixed PATH directory was searched, and a .bat (which spawn
    // cannot start without a shell) is never chosen.
    expect(probed).toEqual(['C:\\Tools\\mini-agent.exe']);

    expect(resolveConfiguredExecutable('mini-agent.exe', 'C:\\Users\\Alice', 'win32', lookup))
      .toEqual({ ok: true, path: 'C:\\Tools\\mini-agent.exe' });
    expect(resolveConfiguredExecutable('C:mini-agent.exe', 'C:\\Users\\Alice', 'win32', lookup).ok)
      .toBe(false);
  });

  it('rejects relative paths containing a separator with a clear message', () => {
    const result = resolveConfiguredExecutable('./target/debug/mini-agent', '/home/alice', 'linux');
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.reason).toContain('"./target/debug/mini-agent"');
      expect(result.reason).toMatch(/relative path/);
      expect(result.reason).toMatch(/absolute path/);
    }
    expect(resolveConfiguredExecutable('bin\\mini-agent.exe', 'C:\\Users\\Alice', 'win32').ok).toBe(false);
  });

  it('rejects empty values and ~user forms', () => {
    expect(resolveConfiguredExecutable('   ', '/home/alice', 'linux'))
      .toEqual({ ok: false, reason: 'mini-agent.executablePath is empty.' });
    const other = resolveConfiguredExecutable('~bob/mini-agent', '/home/alice', 'linux');
    expect(other.ok).toBe(false);
    if (!other.ok) { expect(other.reason).toMatch(/~user/); }
  });
});
