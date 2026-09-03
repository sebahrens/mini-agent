import { describe, expect, it } from 'vitest';
import {
  LineRingBuffer,
  STDERR_RING_CAPACITY,
  formatStderrBlock,
  probeFailureMessage,
  unexpectedExitMessage,
  withRecentStderr,
} from '../src/diagnostics';

describe('LineRingBuffer', () => {
  it('reassembles lines across chunk boundaries and reports completed lines', () => {
    const ring = new LineRingBuffer(10);
    expect(ring.push('first li')).toEqual([]);
    expect(ring.push('ne\r\nsecond\nthi')).toEqual(['first line', 'second']);
    expect(ring.lines()).toEqual(['first line', 'second']);
    expect(ring.flush()).toEqual(['thi']);
    expect(ring.lines()).toEqual(['first line', 'second', 'thi']);
    expect(ring.flush()).toEqual([]);
  });

  it('accepts binary chunks and drops blank lines', () => {
    const ring = new LineRingBuffer(10);
    expect(ring.push(Buffer.from('a\n\n  \nb\n', 'utf8'))).toEqual(['a', 'b']);
  });

  it('keeps only the most recent lines', () => {
    const ring = new LineRingBuffer(3);
    ring.push('1\n2\n3\n4\n5\n');
    expect(ring.lines()).toEqual(['3', '4', '5']);
    expect(ring.size).toBe(3);
    expect(ring.tail(2)).toEqual(['4', '5']);
    expect(ring.tail(0)).toEqual([]);
  });

  it('defaults to a bounded capacity and rejects invalid ones', () => {
    const ring = new LineRingBuffer();
    ring.push(Array.from({ length: STDERR_RING_CAPACITY + 25 }, (_, i) => `line ${i}`).join('\n') + '\n');
    expect(ring.size).toBe(STDERR_RING_CAPACITY);
    expect(ring.lines()[0]).toBe('line 25');
    expect(() => new LineRingBuffer(0)).toThrow(/positive/);
  });

  it('returns a copy so callers cannot mutate the buffer', () => {
    const ring = new LineRingBuffer(2);
    ring.push('x\n');
    (ring.lines() as string[]).push('injected');
    expect(ring.lines()).toEqual(['x']);
  });
});

describe('unexpectedExitMessage', () => {
  it('includes the last stderr lines and points to the output channel', () => {
    const message = unexpectedExitMessage(1, null, [
      'old noise',
      'Error: no provider API key configured',
      'hint: set ANTHROPIC_API_KEY',
    ], 2);
    expect(message).toBe(
      'Mini Agent exited unexpectedly (code 1). Last stderr: '
      + 'Error: no provider API key configured | hint: set ANTHROPIC_API_KEY'
      + ' — see the Mini Agent output for details. The next chat request will restart it.',
    );
  });

  it('describes signals and empty stderr', () => {
    expect(unexpectedExitMessage(null, 'SIGKILL', []))
      .toBe('Mini Agent exited unexpectedly (signal SIGKILL). The next chat request will restart it.');
    expect(unexpectedExitMessage(null, null, []))
      .toBe('Mini Agent exited unexpectedly (unknown reason). The next chat request will restart it.');
  });
});

describe('probeFailureMessage', () => {
  it('names the executable, exit status, and captured stderr', () => {
    expect(probeFailureMessage('/opt/mini-agent', 1, null, ['Startup::init failed: missing key']))
      .toBe('The executable at "/opt/mini-agent" failed the Mini Agent version check (exit code 1). '
        + 'stderr: Startup::init failed: missing key');
    expect(probeFailureMessage('/opt/mini-agent', null, 'SIGSEGV', []))
      .toBe('The executable at "/opt/mini-agent" failed the Mini Agent version check (signal SIGSEGV).');
  });
});

describe('withRecentStderr / formatStderrBlock', () => {
  it('appends recent stderr only when present', () => {
    expect(withRecentStderr('ACP initialize failed: timeout.', [])).toBe('ACP initialize failed: timeout.');
    expect(withRecentStderr('ACP initialize failed: timeout.', ['a', 'b', 'c', 'd'], 3))
      .toBe('ACP initialize failed: timeout. Recent stderr: b | c | d');
  });

  it('renders a block for the output channel', () => {
    expect(formatStderrBlock([])).toBe('(no stderr captured)');
    expect(formatStderrBlock(['one', 'two'])).toBe('  [stderr] one\n  [stderr] two');
  });
});
