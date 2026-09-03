import type * as acp from '@agentclientprotocol/sdk';

/** Upper bound for the input text rendered in the permission modal. */
export const PERMISSION_DETAIL_LIMIT = 800;
/** Upper bound for the pattern echoed in the "Allow always" button label. */
export const PERMISSION_LABEL_LIMIT = 60;

const META_PATTERN_KEYS = ['suggestedPattern', 'suggested_pattern', 'pattern'] as const;

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
 * The rule the server will persist when the user picks an `allow_always`
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
    return String(rawInput);
  }
}

/** Human-readable lines describing what the tool call is about to do. */
export function toolCallInputLines(toolCall: acp.ToolCallUpdate): string[] {
  const lines: string[] = [];
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
        lines.push(`edit ${entry.path}`);
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
  return lines.map(line => line.trim()).filter(Boolean);
}

/**
 * Compose the modal `detail` text: the command/input the tool wants to run,
 * bounded to a sane length, what "Allow always" would persist, and the id.
 */
export function buildPermissionDetail(
  request: acp.RequestPermissionRequest,
  limit: number = PERMISSION_DETAIL_LIMIT,
): string {
  const input = toolCallInputLines(request.toolCall).join('\n');
  const sections: string[] = [];
  sections.push(input.length > 0 ? truncate(input, limit) : '(no input details provided)');

  const always = request.options.find(option => option.kind === 'allow_always');
  if (always) {
    const pattern = suggestedPattern(request, always);
    sections.push(pattern
      ? `"${always.name}" will persist this rule: ${truncate(pattern, limit)}`
      : `"${always.name}" will persist a rule for this exact input.`);
  }

  sections.push(`Tool call ${request.toolCall.toolCallId}`);
  return sections.join('\n\n');
}

/** Button label for an option; the allow-always option shows what it persists. */
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
