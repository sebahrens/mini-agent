export declare const PROTOCOL: 'mini-agent-control/1';

export interface Options {
  /** Path of the mini-agent binary (default `mini-agent` on PATH). */
  binary?: string;
  cwd?: string;
  env?: Record<string, string>;
  /** Initial prompt, run before any `send()`. */
  prompt?: string;
  /** `--stream-json-schema`; default `claude`. */
  schema?: 'native' | 'claude';
  includePartialMessages?: boolean;
  /** `--stream-json-ask-timeout` seconds. */
  askTimeoutSecs?: number;
  previewBytes?: number;
  model?: string;
  permissionMode?: string;
  continueSession?: boolean;
  extraArgs?: string[];
  /** Replaces the generated argv entirely (wrappers, tests). */
  argv?: string[];
  stderr?: 'ignore' | 'inherit' | 'pipe';
  /** Answers `can_use_tool` asks. Absent callback denies. */
  canUseTool?: (toolName: string, input: unknown, ask: PermissionAsk) => PermissionReply | Promise<PermissionReply>;
}

export interface PermissionAsk {
  subtype: 'can_use_tool';
  tool_name: string;
  input: string;
  input_bytes?: number;
  truncated?: boolean;
  tool_input?: Record<string, unknown>;
}

export type PermissionReply =
  | { behavior: 'allow'; scope?: 'run'; updatedInput?: Record<string, unknown> }
  | { behavior: 'deny'; message?: string };

export interface StreamRecord { type: string; [key: string]: unknown }

export declare function buildArgs(options?: Options): string[];
export declare function isRecord(value: unknown): value is StreamRecord;

export declare class Session {
  constructor(options?: Options);
  send(text: string): boolean;
  control(subtype: string, fields?: Record<string, unknown>): Promise<Record<string, unknown>>;
  initialize(): Promise<Record<string, unknown>>;
  interrupt(): Promise<Record<string, unknown>>;
  setModel(model: string): Promise<Record<string, unknown>>;
  setPermissionMode(mode: string): Promise<Record<string, unknown>>;
  next(): Promise<StreamRecord | undefined>;
  messages(): AsyncGenerator<StreamRecord>;
  turn(): AsyncGenerator<StreamRecord>;
  close(): Promise<{ code: number | null; signal: string | null }>;
}

export declare function query(args: { prompt: string; options?: Options }): AsyncGenerator<StreamRecord>;
