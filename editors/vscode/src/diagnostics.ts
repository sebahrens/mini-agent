/** Default number of child-process stderr lines retained for error reports. */
export const STDERR_RING_CAPACITY = 50;
/** Lines shown inline in a toast; the full ring goes to the output channel. */
export const STDERR_TOAST_LINES = 3;

/** Retains the last N complete lines from a chunked text stream. */
export class LineRingBuffer {
  private readonly buffer: string[] = [];
  private partial = '';

  constructor(private readonly capacity: number = STDERR_RING_CAPACITY) {
    if (!Number.isInteger(capacity) || capacity < 1) {
      throw new Error('LineRingBuffer capacity must be a positive integer.');
    }
  }

  /** Feed a chunk; returns the complete lines it finished so callers can log them. */
  push(chunk: string | Uint8Array): string[] {
    const text = typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString('utf8');
    const parts = (this.partial + text).split(/\r?\n/);
    this.partial = parts.pop() ?? '';
    const completed = parts.map(line => line.trimEnd()).filter(line => line.length > 0);
    for (const line of completed) { this.append(line); }
    return completed;
  }

  /** Flush a trailing partial line (e.g. after the stream ends). */
  flush(): string[] {
    const line = this.partial.trimEnd();
    this.partial = '';
    if (line.length === 0) { return []; }
    this.append(line);
    return [line];
  }

  lines(): readonly string[] { return [...this.buffer]; }

  tail(count: number): readonly string[] {
    return count <= 0 ? [] : this.buffer.slice(-count);
  }

  get size(): number { return this.buffer.length; }

  private append(line: string): void {
    this.buffer.push(line);
    if (this.buffer.length > this.capacity) {
      this.buffer.splice(0, this.buffer.length - this.capacity);
    }
  }
}

function joinTail(lines: readonly string[], count: number): string | undefined {
  const tail = lines.slice(-count);
  return tail.length > 0 ? tail.join(' | ') : undefined;
}

/** Message for the "exited unexpectedly" toast, ending with recent stderr if any. */
export function unexpectedExitMessage(
  code: number | null,
  signal: NodeJS.Signals | null,
  stderr: readonly string[],
  toastLines: number = STDERR_TOAST_LINES,
): string {
  const reason = code !== null ? `code ${code}` : signal !== null ? `signal ${signal}` : 'unknown reason';
  const tail = joinTail(stderr, toastLines);
  const head = `Mini Agent exited unexpectedly (${reason}).`;
  return tail
    ? `${head} Last stderr: ${tail} — see the Mini Agent output for details. The next chat request will restart it.`
    : `${head} The next chat request will restart it.`;
}

/** Message when the `--version` probe exits non-zero. */
export function probeFailureMessage(
  executablePath: string,
  code: number | null,
  signal: NodeJS.Signals | null,
  stderr: readonly string[],
  toastLines: number = STDERR_TOAST_LINES,
): string {
  const reason = code !== null ? `exit code ${code}` : signal !== null ? `signal ${signal}` : 'no exit code';
  const tail = joinTail(stderr, toastLines);
  const head = `The executable at "${executablePath}" failed the Mini Agent version check (${reason}).`;
  return tail ? `${head} stderr: ${tail}` : head;
}

/** Append recent stderr lines to an initialize/handshake failure message. */
export function withRecentStderr(
  message: string,
  stderr: readonly string[],
  toastLines: number = STDERR_TOAST_LINES,
): string {
  const tail = joinTail(stderr, toastLines);
  return tail ? `${message} Recent stderr: ${tail}` : message;
}

/** Multi-line block for the output channel. */
export function formatStderrBlock(stderr: readonly string[]): string {
  if (stderr.length === 0) { return '(no stderr captured)'; }
  return stderr.map(line => `  [stderr] ${line}`).join('\n');
}
