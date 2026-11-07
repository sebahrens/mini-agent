import type * as acp from '@agentclientprotocol/sdk';
import { diffSummaryLines, diffText } from './toolcontent';

/** Upper bound for the input text rendered in the permission modal `detail`. */
export const PERMISSION_DETAIL_LIMIT = 800;
/** Upper bound for the pattern echoed in the allow-always button label. */
export const PERMISSION_LABEL_LIMIT = 60;

const META_PATTERN_KEYS = ['suggestedPattern', 'suggested_pattern', 'pattern'] as const;

/** Button that opens the complete, unclipped input before anything is approved. */
export const SHOW_FULL_COMMAND_LABEL = 'Show full command';

export function truncate(value: string, limit: number): string {
  const text = value.trim();
  if (text.length <= limit) { return text; }
  return `${text.slice(0, Math.max(0, limit - 1)).trimEnd()}…`;
}

function metaString(meta: unknown): string | undefined {
  if (!meta || typeof meta !== 'object') { return undefined; }
  const record = meta as Record<string, unknown>;
  for (const key of META_PATTERN_KEYS) {
    const value = record[key];
    if (typeof value === 'string' && value.trim().length > 0) { return value.trim(); }
  }
  return undefined;
}

/**
 * The session rule the server will install when the user picks an `allow_always`
 * option, if the server advertised it (option `_meta` wins over the tool call
 * and request `_meta`). Returns undefined when nothing was provided.
 */
export function suggestedPattern(
  request: acp.RequestPermissionRequest,
  option: acp.PermissionOption,
): string | undefined {
  return metaString(option._meta) ?? metaString(request.toolCall._meta) ?? metaString(request._meta);
}

function stringifyRawInput(rawInput: unknown): string | undefined {
  if (rawInput === undefined || rawInput === null) { return undefined; }
  if (typeof rawInput === 'string') { return rawInput; }
  try {
    return JSON.stringify(rawInput);
  } catch {
    return typeof rawInput === 'bigint'
      ? rawInput.toString()
      : '[input could not be serialized]';
  }
}

/** Human-readable lines describing what the tool call is about to do. */
export function toolCallInputLines(toolCall: acp.ToolCallUpdate, full = false): string[] {
  const lines: string[] = [];
  // Full view keeps diff rows verbatim (indentation, blank context rows); others are trimmed.
  const verbatim = new Set<number>();
  for (const entry of toolCall.content ?? []) {
    switch (entry.type) {
      case 'content': {
        const block = entry.content;
        if (block.type === 'text') { lines.push(block.text); }
        else if (block.type === 'resource_link') { lines.push(`resource: ${block.uri}`); }
        else { lines.push(`[${block.type} content]`); }
        break;
      }
      case 'diff':
        // The modal detail gets the 12-row summary; the full view gets every row the server sent.
        lines.push(`edit ${entry.path}`);
        for (const row of (full ? diffText(entry, true).split('\n') : diffSummaryLines(entry)).slice(1)) {
          if (full) { verbatim.add(lines.length); }
          lines.push(row);
        }
        break;
      case 'terminal':
        lines.push(`terminal ${entry.terminalId}`);
        break;
    }
  }
  for (const location of toolCall.locations ?? []) {
    lines.push(`path: ${location.path}`);
  }
  const raw = stringifyRawInput(toolCall.rawInput);
  if (raw && !lines.some(line => line.trim() === raw.trim())) {
    lines.push(`input: ${raw}`);
  }
  return lines.map((line, index) => (verbatim.has(index) ? line : line.trim()))
    .filter((line, index) => verbatim.has(index) || line.length > 0);
}

/** The complete input text of the tool call (what an allow authorises). */
export function permissionInputText(request: acp.RequestPermissionRequest): string {
  return toolCallInputLines(request.toolCall, true).join('\n');
}

/** True when the agent itself marked a diff preview as truncated (the full view cannot show more). */
function agentTruncatedDiff(toolCall: acp.ToolCallUpdate): boolean {
  return (toolCall.content ?? []).some(entry => entry.type === 'diff' && Boolean(entry._meta?.['mini-agent-truncated']));
}

/** The allow-always session rule, if the server advertised one. */
function allowAlwaysPattern(request: acp.RequestPermissionRequest): string | undefined {
  const always = request.options.find(option => option.kind === 'allow_always');
  return always ? suggestedPattern(request, always) : undefined;
}

/** Characters of the input (and allow-always pattern) the modal detail cannot show. */
export function permissionOmittedChars(
  request: acp.RequestPermissionRequest,
  limit: number = PERMISSION_DETAIL_LIMIT,
): number {
  // Indentation kept in the full view is not "omitted" text: compare with whitespace trimmed per row,
  // so only characters lost to row/line clipping count.
  const input = toolCallInputLines(request.toolCall, true).map(line => line.trim()).join('\n').trim();
  // The modal shows the clipped summary (diffs are cut to a few rows), so measure against that.
  const shown = Math.min(toolCallInputLines(request.toolCall).join('\n').trim().length, limit);
  const pattern = allowAlwaysPattern(request)?.trim() ?? '';
  const inputOmitted = Math.max(0, input.length - shown);
  return Math.max(inputOmitted, agentTruncatedDiff(request.toolCall) ? 1 : 0) + Math.max(0, pattern.length - limit);
}

/** Text for the read-only full view: the whole input and the whole allow-always rule. */
export function permissionFullText(request: acp.RequestPermissionRequest): string {
  const pattern = allowAlwaysPattern(request);
  return [
    permissionInputText(request) || '(no input details provided)',
    ...(pattern ? [`--- allow-always session rule ---\n${pattern}`] : []),
  ].join('\n\n');
}

/**
 * Compose the modal `detail` text: the command/input the tool wants to run,
 * bounded to a sane length, what the allow-always option grants for this session, and the id.
 */
export function buildPermissionDetail(
  request: acp.RequestPermissionRequest,
  limit: number = PERMISSION_DETAIL_LIMIT,
): string {
  const input = toolCallInputLines(request.toolCall).join('\n');
  const sections: string[] = [];
  sections.push(input.length > 0 ? truncate(input, limit) : '(no input details provided)');
  const omitted = permissionOmittedChars(request, limit);
  if (omitted > 0) {
    sections.push(`WARNING: ${omitted} more characters are not shown above. Choose "${SHOW_FULL_COMMAND_LABEL}" and read all of it before approving.`);
  }

  const always = request.options.find(option => option.kind === 'allow_always');
  if (always) {
    const pattern = suggestedPattern(request, always);
    // mini-agent installs allow-always grants for the current session only.
    sections.push(pattern
      ? `"${always.name}" will allow this for this session: ${truncate(pattern, limit)}`
      : `"${always.name}" will allow this exact input for this session.`);
  }

  sections.push(`Tool call ${request.toolCall.toolCallId}`);
  return sections.join('\n\n');
}

/** Button label for an option; the allow-always option shows the session rule it installs. */
export function permissionOptionTitle(
  request: acp.RequestPermissionRequest,
  option: acp.PermissionOption,
): string {
  const kind = option.kind.replaceAll('_', ' ');
  const base = option.name.toLowerCase() === kind ? option.name : `${option.name} (${kind})`;
  if (option.kind !== 'allow_always') { return base; }
  const pattern = suggestedPattern(request, option);
  return pattern ? `${base}: ${truncate(pattern, PERMISSION_LABEL_LIMIT)}` : base;
}
