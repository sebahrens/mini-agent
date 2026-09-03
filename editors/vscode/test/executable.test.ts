import { describe, expect, it } from 'vitest';
import { resolveConfiguredExecutable } from '../src/executable';

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

  it('keeps a bare command name for PATH lookup', () => {
    expect(resolveConfiguredExecutable('mini-agent', '/home/alice', 'linux'))
      .toEqual({ ok: true, path: 'mini-agent' });
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
