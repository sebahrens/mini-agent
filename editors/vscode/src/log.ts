import * as vscode from 'vscode';

const channel = vscode.window.createOutputChannel('Mini Agent');

const LEVELS = ['error', 'warn', 'info', 'debug', 'trace'] as const;
type Level = typeof LEVELS[number];

let currentLevel: Level = 'info';

export function setLogLevel(level: string): void {
  if (LEVELS.includes(level as Level)) {
    currentLevel = level as Level;
  }
}

function shouldLog(level: Level): boolean {
  return LEVELS.indexOf(level) <= LEVELS.indexOf(currentLevel);
}

function write(level: Level, message: string): void {
  if (!shouldLog(level)) { return; }
  const ts = new Date().toISOString();
  channel.appendLine(`[${ts}] [${level.toUpperCase()}] ${message}`);
}

export const log = {
  error: (msg: string) => write('error', msg),
  warn:  (msg: string) => write('warn', msg),
  info:  (msg: string) => write('info', msg),
  debug: (msg: string) => write('debug', msg),
  trace: (msg: string) => write('trace', msg),
  channel,
};
