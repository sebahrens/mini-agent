import type * as acp from '@agentclientprotocol/sdk';

const MAX_DIFF_LINES = 200;
const MAX_TEXT_CHARS = 8 * 1024;
const CONTEXT_LINES = 3;
const MAX_ROW_CHARS = 200;
const MAX_DIFF_CHARS = 16 * 1024;

/** A fence longer than any backtick run inside, so tool output cannot close it early. */
export function fenced(text: string, language = ''): string {
  let longest = 0;
  for (const run of text.match(/`+/gu) ?? []) { longest = Math.max(longest, run.length); }
  const fence = '`'.repeat(Math.max(3, longest + 1));
  return `${fence}${language}\n${text}\n${fence}`;
}

export function inlineCode(text: string): string {
  const flat = text.replace(/\s+/gu, ' ').trim();
  let longest = 0;
  for (const run of flat.match(/`+/gu) ?? []) { longest = Math.max(longest, run.length); }
  const tick = '`'.repeat(longest + 1);
  return `${tick} ${flat} ${tick}`;
}

/** Line diff trimmed to the changed region plus a little context; bounded and never silent about it. */
export function diffText(diff: acp.Diff, full = false): string {
  const oldLines = diff.oldText == null ? [] : diff.oldText.split('\n');
  const newLines = diff.newText.split('\n');
  let prefix = 0;
  while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) { prefix += 1; }
  let suffix = 0;
  while (suffix < oldLines.length - prefix && suffix < newLines.length - prefix
    && oldLines[oldLines.length - 1 - suffix] === newLines[newLines.length - 1 - suffix]) { suffix += 1; }
  const rows: string[] = [];
  const head = Math.max(0, prefix - CONTEXT_LINES);
  for (let index = head; index < prefix; index += 1) { rows.push(` ${oldLines[index]}`); }
  for (const line of oldLines.slice(prefix, oldLines.length - suffix)) { rows.push(`-${line}`); }
  for (const line of newLines.slice(prefix, newLines.length - suffix)) { rows.push(`+${line}`); }
  for (const line of oldLines.slice(oldLines.length - suffix, oldLines.length - suffix + CONTEXT_LINES)) { rows.push(` ${line}`); }
  const header = diff.oldText == null ? `new file ${diff.path}` : diff.path;
  const meta = diff._meta ?? {};
  const notes: string[] = [];
  if (meta['mini-agent-partial']) { notes.push('changed region only'); }
  if (meta['mini-agent-truncated']) { notes.push('preview was truncated by the agent'); }
  const omittedBlocks = Number(meta['mini-agent-omitted-blocks']);
  if (Number.isFinite(omittedBlocks) && omittedBlocks > 0) {
    notes.push(`${Math.floor(omittedBlocks)} more edit block${omittedBlocks === 1 ? '' : 's'} not shown`);
  }
  // Bound lines, line length and total size so a fenced block always closes.
  // The full (approval) view applies no cap: every row is shown unclipped.
  if (!full) {
    let budget = MAX_DIFF_CHARS;
    let kept = 0;
    for (const row of rows) {
      if (kept >= MAX_DIFF_LINES || budget <= 0) { break; }
      const clipped = row.length > MAX_ROW_CHARS ? `${row.slice(0, MAX_ROW_CHARS)}…` : row;
      rows[kept] = clipped;
      budget -= clipped.length + 1;
      kept += 1;
    }
    if (kept < rows.length) {
      notes.push(`${rows.length - kept} more diff lines not shown`);
      rows.length = kept;
    }
  }
  return [full ? header : header.slice(0, MAX_ROW_CHARS), ...rows, ...(notes.length ? [`(${notes.join('; ')})`] : [])].join('\n');
}

/** Markdown for the result content of a finished tool call; empty when there is none. */
export function toolContentMarkdown(content: readonly acp.ToolCallContent[] | null | undefined): string {
  const parts: string[] = [];
  for (const entry of content ?? []) {
    if (entry.type === 'diff') {
      parts.push(fenced(diffText(entry), 'diff'));
    } else if (entry.type === 'content' && entry.content.type === 'text' && entry.content.text.trim()) {
      const text = entry.content.text;
      const shown = text.length > MAX_TEXT_CHARS
        ? `${text.slice(0, MAX_TEXT_CHARS)}\n… ${text.length - MAX_TEXT_CHARS} more characters not shown`
        : text;
      parts.push(fenced(shown));
    }
  }
  return parts.join('\n\n');
}

/** Plain-text edit summary for the approval prompt (no markdown there). */
export function diffSummaryLines(diff: acp.Diff, maxRows = 12): string[] {
  const rows = diffText(diff).split('\n');
  return rows.length > maxRows + 1 ? [...rows.slice(0, maxRows + 1), `… ${rows.length - maxRows - 1} more lines`] : rows;
}
