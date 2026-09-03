import { describe, expect, it } from 'vitest';
import type * as acp from '@agentclientprotocol/sdk';
import {
  PERMISSION_LABEL_LIMIT,
  buildPermissionDetail,
  permissionOptionTitle,
  suggestedPattern,
  toolCallInputLines,
  truncate,
} from '../src/permission';

const options: acp.PermissionOption[] = [
  { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  { optionId: 'allow_always', name: 'Allow always', kind: 'allow_always' },
  { optionId: 'deny', name: 'Deny', kind: 'reject_once' },
];

/** Mirrors what src/extras/acp/mod.rs drive_permission_bridge sends. */
function serverRequest(input: string, extra: Partial<acp.ToolCallUpdate> = {}): acp.RequestPermissionRequest {
  return {
    sessionId: 'session-1',
    toolCall: {
      toolCallId: 'a1b2c3d4-0000-4000-8000-000000000000',
      title: 'bash',
      content: [{ type: 'content', content: { type: 'text', text: input } }],
      ...extra,
    },
    options,
  };
}

describe('truncate', () => {
  it('keeps short text and bounds long text with an ellipsis', () => {
    expect(truncate('  ls -la  ', 20)).toBe('ls -la');
    const long = 'x'.repeat(100);
    const bounded = truncate(long, 20);
    expect(bounded).toHaveLength(20);
    expect(bounded.endsWith('…')).toBe(true);
  });
});

describe('toolCallInputLines', () => {
  it('extracts text content blocks, diff paths, terminals, locations and rawInput', () => {
    const lines = toolCallInputLines({
      toolCallId: 'id',
      content: [
        { type: 'content', content: { type: 'text', text: 'rm -rf build' } },
        { type: 'diff', path: '/repo/src/main.rs', oldText: 'a', newText: 'b' },
        { type: 'terminal', terminalId: 'term-7' },
        { type: 'content', content: { type: 'resource_link', uri: 'file:///repo/x', name: 'x' } },
      ],
      locations: [{ path: '/repo/src/lib.rs' }],
      rawInput: { command: 'rm -rf build' },
    });
    expect(lines).toEqual([
      'rm -rf build',
      'edit /repo/src/main.rs',
      'terminal term-7',
      'resource: file:///repo/x',
      'path: /repo/src/lib.rs',
      'input: {"command":"rm -rf build"}',
    ]);
  });

  it('does not repeat rawInput when it equals the text block', () => {
    expect(toolCallInputLines({
      toolCallId: 'id',
      content: [{ type: 'content', content: { type: 'text', text: 'git status' } }],
      rawInput: 'git status',
    })).toEqual(['git status']);
  });

  it('handles a tool call with no content at all', () => {
    expect(toolCallInputLines({ toolCallId: 'id' })).toEqual([]);
  });
});

describe('buildPermissionDetail', () => {
  it('shows the command from the content text block, not only the tool name and id', () => {
    const detail = buildPermissionDetail(serverRequest('cargo test --workspace'));
    expect(detail).toContain('cargo test --workspace');
    expect(detail).toContain('"Allow always" will persist a rule for this exact input.');
    expect(detail).toContain('Tool call a1b2c3d4-0000-4000-8000-000000000000');
  });

  it('bounds the input with an ellipsis', () => {
    const detail = buildPermissionDetail(serverRequest('y'.repeat(5000)), 120);
    const [input] = detail.split('\n\n');
    expect(input).toHaveLength(120);
    expect(input?.endsWith('…')).toBe(true);
  });

  it('names the persisted pattern when the server provides one', () => {
    const request = serverRequest('npm test');
    request.options = options.map(option => option.kind === 'allow_always'
      ? { ...option, _meta: { suggestedPattern: 'npm *' } }
      : option);
    expect(buildPermissionDetail(request)).toContain('"Allow always" will persist this rule: npm *');
  });

  it('falls back to a placeholder when there is no input', () => {
    const request = serverRequest('');
    request.toolCall.content = [];
    expect(buildPermissionDetail(request)).toContain('(no input details provided)');
  });
});

describe('suggestedPattern', () => {
  it('prefers option meta over tool-call meta over request meta', () => {
    const request = serverRequest('x', { _meta: { suggested_pattern: 'from-tool-call' } });
    request._meta = { pattern: 'from-request' };
    const always = request.options[1]!;
    expect(suggestedPattern(request, always)).toBe('from-tool-call');
    expect(suggestedPattern(request, { ...always, _meta: { suggestedPattern: 'from-option' } }))
      .toBe('from-option');
    delete request.toolCall._meta;
    expect(suggestedPattern(request, always)).toBe('from-request');
    delete request._meta;
    expect(suggestedPattern(request, always)).toBeUndefined();
  });

  it('ignores non-string or blank meta values', () => {
    const request = serverRequest('x');
    const always = { ...request.options[1]!, _meta: { suggestedPattern: '   ', pattern: 42 } };
    expect(suggestedPattern(request, always)).toBeUndefined();
  });
});

describe('permissionOptionTitle', () => {
  it('keeps short labels and avoids repeating the kind when it matches the name', () => {
    const request = serverRequest('x');
    expect(permissionOptionTitle(request, options[0]!)).toBe('Allow once');
    expect(permissionOptionTitle(request, options[1]!)).toBe('Allow always');
    expect(permissionOptionTitle(request, options[2]!)).toBe('Deny (reject once)');
  });

  it('labels allow-always with the pattern that will be persisted, bounded', () => {
    const request = serverRequest('x');
    const always = { ...options[1]!, _meta: { suggestedPattern: 'cargo *' } };
    expect(permissionOptionTitle(request, always)).toBe('Allow always: cargo *');
    const huge = { ...options[1]!, _meta: { suggestedPattern: 'z'.repeat(500) } };
    const title = permissionOptionTitle(request, huge);
    expect(title.length).toBeLessThanOrEqual('Allow always: '.length + PERMISSION_LABEL_LIMIT);
    expect(title.endsWith('…')).toBe(true);
  });
});
