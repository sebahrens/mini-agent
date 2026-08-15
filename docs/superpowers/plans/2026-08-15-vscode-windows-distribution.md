# VS Code Extension + Windows MSI Distribution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship mini-agent to non-technical Windows users via a VS Code marketplace extension and an MSI installer — zero terminal required.

**Architecture:** A TypeScript VS Code extension bundles per-platform `mini-agent` binaries, spawns the selected one as `mini-agent --acp` over stdio, and surfaces a chat webview panel. A WiX MSI installer handles Windows-specific deployment (binary to `%LOCALAPPDATA%`, silent extension install). A GitHub Actions workflow builds everything on release tags, consuming pre-built Rust artifacts.

**Tech Stack:** TypeScript 5, VS Code Extension API, esbuild, @vscode/vsce, vitest (unit), @vscode/test-cli (integration), WiX Toolset v4, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-15-vscode-windows-distribution.md`

## Global Constraints

- Rust binary build is **unchanged** — no Rust code is touched.
- Extension activates on `"onStartupFinished"` — no eager activation.
- `bin/` directory is gitignored — never committed, only populated by CI.
- Uses VS Code `SecretStorage` for nothing; all config delegated to mini-agent's `~/.config/mini-agent/config.toml`.
- VS Code engine version floor: `^1.85.0`.
- WiX v4 — not v3 (different project format and CLI).
- No external CDN/network calls from either the extension or MSI at runtime.
- `vsce` package manager: `@vscode/vsce` (not deprecated `vsce`).
- esbuild bundle target: `node18`, format `cjs`, `--external:vscode`.

---

## File Map

**Create:**
```
vscode-extension/
  package.json                  extension manifest + scripts
  tsconfig.json                 TypeScript config
  .vscodeignore                 vsce packaging exclusions
  .gitignore                    exclude bin/, dist/, node_modules/
  config.template.toml          bundled default config (copied to user on first Open Config)
  esbuild.mjs                   build script
  src/
    extension.ts                activate/deactivate entry point
    binary.ts                   binary resolution (PATH → bundled fallback)
    client.ts                   ACP stdio child-process client
    panel.ts                    WebviewView sidebar panel
    commands.ts                 Open Config / Restart Agent / Show Output
  media/
    panel.html                  webview HTML (no external resources)
    panel.css                   VS Code theme-variable styles
    panel.js                    webview-side message handler
  test/
    binary.test.ts              vitest unit tests for binary.ts
    client.test.ts              vitest unit tests for client.ts
    extension.test.ts           @vscode/test-cli integration smoke test

packaging/windows/
  mini-agent.wxs                WiX component + feature definitions
  installer.wixproj             WiX project file
  build.ps1                     local build helper script
  assets/
    license.rtf                 GPL-3.0 in RTF for installer dialog
    banner.bmp                  493×58 installer banner (placeholder)
    dialog.bmp                  493×312 installer dialog (placeholder)

.github/workflows/
  vscode-publish.yml            release workflow: download artifacts, package, publish, build MSI
```

**Modify:**
```
.gitignore                      add vscode-extension/bin/ and vscode-extension/dist/
```

---

## Task 1: Extension project scaffold

**Files:**
- Create: `vscode-extension/package.json`
- Create: `vscode-extension/tsconfig.json`
- Create: `vscode-extension/.vscodeignore`
- Create: `vscode-extension/.gitignore`
- Create: `vscode-extension/esbuild.mjs`
- Create: `vscode-extension/config.template.toml`
- Create: `vscode-extension/src/extension.ts` (stub)
- Modify: `.gitignore`

**Interfaces:**
- Produces: buildable extension skeleton; `npm run compile` succeeds; `vsce package` produces a `.vsix`

- [ ] **Step 1: Create `vscode-extension/package.json`**

```json
{
  "name": "mini-agent",
  "displayName": "mini-agent",
  "description": "Minimalistic AI coding agent",
  "version": "1.7.2",
  "publisher": "mini-agent",
  "engines": { "vscode": "^1.85.0" },
  "categories": ["AI", "Chat"],
  "activationEvents": ["onStartupFinished"],
  "main": "./dist/extension.js",
  "contributes": {
    "viewsContainers": {
      "activitybar": [{
        "id": "mini-agent",
        "title": "mini-agent",
        "icon": "media/icon.svg"
      }]
    },
    "views": {
      "mini-agent": [{
        "type": "webview",
        "id": "mini-agent.panel",
        "name": "Chat"
      }]
    },
    "commands": [
      { "command": "mini-agent.openConfig", "title": "mini-agent: Open Config" },
      { "command": "mini-agent.restart",    "title": "mini-agent: Restart Agent" },
      { "command": "mini-agent.showOutput", "title": "mini-agent: Show Output" }
    ]
  },
  "scripts": {
    "compile": "node esbuild.mjs",
    "watch":   "node esbuild.mjs --watch",
    "test":    "vitest run",
    "vscode-test": "vscode-test",
    "package": "vsce package"
  },
  "devDependencies": {
    "@types/node": "^20.0.0",
    "@types/vscode": "^1.85.0",
    "@vscode/test-cli": "^0.0.9",
    "@vscode/test-electron": "^2.3.0",
    "@vscode/vsce": "^3.0.0",
    "esbuild": "^0.24.0",
    "typescript": "^5.4.0",
    "vitest": "^2.0.0"
  }
}
```

- [ ] **Step 2: Create `vscode-extension/tsconfig.json`**

```json
{
  "compilerOptions": {
    "module": "Node16",
    "moduleResolution": "Node16",
    "target": "ES2022",
    "lib": ["ES2022"],
    "outDir": "dist",
    "rootDir": "src",
    "strict": true,
    "skipLibCheck": true,
    "sourceMap": true
  },
  "include": ["src/**/*.ts"],
  "exclude": ["node_modules", "dist", "test"]
}
```

- [ ] **Step 3: Create `vscode-extension/esbuild.mjs`**

```js
import * as esbuild from 'esbuild';

const watch = process.argv.includes('--watch');

const ctx = await esbuild.context({
  entryPoints: ['src/extension.ts'],
  bundle: true,
  outfile: 'dist/extension.js',
  external: ['vscode'],
  format: 'cjs',
  platform: 'node',
  target: 'node18',
  sourcemap: true,
  minify: false,
});

if (watch) {
  await ctx.watch();
  console.log('watching...');
} else {
  await ctx.rebuild();
  await ctx.dispose();
}
```

- [ ] **Step 4: Create `vscode-extension/.vscodeignore`**

```
.vscode/**
src/**
test/**
node_modules/**
bin/darwin-arm64/**
bin/darwin-x64/**
bin/linux-x64/**
bin/linux-arm64/**
bin/win32-arm64/**
# The target platform's bin/ subdirectory is NOT excluded here —
# vsce --target substitution handles that at package time
esbuild.mjs
tsconfig.json
*.map
```

Note: The `bin/${VSCE_TARGET}/**` keep rule is handled by the `"files"` field in package.json; `.vscodeignore` excludes all OTHER platforms' binaries.

- [ ] **Step 5: Update `vscode-extension/.vscodeignore` to exclude non-target bins**

Replace the placeholder above with the correct pattern. vsce uses `!bin/${VSCE_TARGET}/**` negation syntax. Rewrite `.vscodeignore` as:

```
.vscode/**
src/**
test/**
node_modules/**
bin/**
!bin/${VSCE_TARGET}/**
esbuild.mjs
tsconfig.json
**/*.map
```

- [ ] **Step 6: Create `vscode-extension/.gitignore`**

```
node_modules/
dist/
bin/
*.vsix
```

- [ ] **Step 7: Create `vscode-extension/config.template.toml`**

```toml
# mini-agent configuration
# Docs: https://github.com/sebahrens/mini-agent

# Set your API key via environment variable or here (env var takes priority):
# export ANTHROPIC_API_KEY=sk-ant-...
# api_key = "sk-ant-..."

# Model to use (defaults to claude-sonnet-4-5):
# model = "claude-sonnet-4-5"

# Backend URL (leave empty for Anthropic default):
# api_base_url = "https://api.anthropic.com"
```

- [ ] **Step 8: Create stub `vscode-extension/src/extension.ts`**

```typescript
import * as vscode from 'vscode';

export function activate(context: vscode.ExtensionContext): void {
  // stub — filled in Task 4
  context.subscriptions.push(
    vscode.window.setStatusBarMessage('mini-agent: starting...')
  );
}

export function deactivate(): void {}
```

- [ ] **Step 9: Add to root `.gitignore`**

Append:
```
vscode-extension/bin/
vscode-extension/dist/
vscode-extension/node_modules/
vscode-extension/*.vsix
```

- [ ] **Step 10: Install dependencies and verify compile**

```bash
cd vscode-extension
npm install
npm run compile
```

Expected: `dist/extension.js` created, no TypeScript errors.

- [ ] **Step 11: Verify vsce can parse the manifest**

```bash
cd vscode-extension
npx vsce ls --no-dependencies 2>&1 | head -20
```

Expected: lists files without errors. Ignore warnings about missing icon.

- [ ] **Step 12: Commit**

```bash
git add vscode-extension/ .gitignore
git commit -m "feat(vscode): extension project scaffold"
```

---

## Task 2: Binary resolution

**Files:**
- Create: `vscode-extension/src/binary.ts`
- Create: `vscode-extension/test/binary.test.ts`

**Interfaces:**
- Produces: `resolveBinary(extensionPath: string): string | null` — returns absolute path to executable or null

- [ ] **Step 1: Write failing tests**

Create `vscode-extension/test/binary.test.ts`:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import * as fs from 'fs';
import * as cp from 'child_process';

// binary.ts uses process.platform, so we mock at the module level
vi.mock('child_process');
vi.mock('fs');

// Import after mocking
const { resolveBinary } = await import('../src/binary.js');

describe('resolveBinary', () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it('returns PATH binary when found', () => {
    vi.mocked(cp.execSync).mockReturnValue('/usr/local/bin/mini-agent\n' as any);
    vi.mocked(fs.existsSync).mockReturnValue(true);
    
    const result = resolveBinary('/ext');
    expect(result).toBe('/usr/local/bin/mini-agent');
  });

  it('falls back to bundled binary when not on PATH', () => {
    vi.mocked(cp.execSync).mockImplementation(() => { throw new Error('not found'); });
    vi.mocked(fs.existsSync).mockImplementation((p) =>
      String(p).includes('bin/') && String(p).includes('mini-agent')
    );
    
    const result = resolveBinary('/ext');
    expect(result).toMatch(/bin\//);
    expect(result).toMatch(/mini-agent/);
  });

  it('returns null when binary not found anywhere', () => {
    vi.mocked(cp.execSync).mockImplementation(() => { throw new Error('not found'); });
    vi.mocked(fs.existsSync).mockReturnValue(false);
    
    const result = resolveBinary('/ext');
    expect(result).toBeNull();
  });
});
```

- [ ] **Step 2: Run tests — expect failure (module not found)**

```bash
cd vscode-extension
npx vitest run test/binary.test.ts
```

Expected: FAIL — `Cannot find module '../src/binary.js'`

- [ ] **Step 3: Implement `vscode-extension/src/binary.ts`**

```typescript
import { execSync } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';

export function resolveBinary(extensionPath: string): string | null {
  // 1. Prefer an existing install on PATH — developer installs win.
  try {
    const cmd = process.platform === 'win32' ? 'where mini-agent' : 'which mini-agent';
    const found = execSync(cmd, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
      .trim()
      .split('\n')[0]
      .trim();
    if (found && fs.existsSync(found)) {
      return found;
    }
  } catch {
    // not on PATH — fall through
  }

  // 2. Fall back to bundled binary.
  const platform = `${process.platform}-${process.arch}`;
  const binaryName = process.platform === 'win32' ? 'mini-agent.exe' : 'mini-agent';
  const bundled = path.join(extensionPath, 'bin', platform, binaryName);

  if (fs.existsSync(bundled)) {
    return bundled;
  }

  return null;
}
```

- [ ] **Step 4: Run tests — expect pass**

```bash
cd vscode-extension
npm run compile && npx vitest run test/binary.test.ts
```

Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add vscode-extension/src/binary.ts vscode-extension/test/binary.test.ts
git commit -m "feat(vscode): binary resolution with PATH → bundled fallback"
```

---

## Task 3: ACP stdio client

**Files:**
- Create: `vscode-extension/src/client.ts`
- Create: `vscode-extension/test/client.test.ts`

**Interfaces:**
- Consumes: `resolveBinary(extensionPath)` from Task 2
- Produces:
  - `class AcpClient { constructor(binaryPath: string, outputChannel: OutputChannel) }`
  - `client.start(): Promise<void>` — spawns process, resolves when handshake complete
  - `client.send(message: AcpMessage): Promise<AcpMessage>` — send request, await response
  - `client.stop(): void` — kills child process
  - `client.onMessage(handler: (msg: AcpMessage) => void): void`
  - `type AcpMessage = { id: string; type: string; [key: string]: unknown }`

- [ ] **Step 1: Write failing tests**

Create `vscode-extension/test/client.test.ts`:

```typescript
import { describe, it, expect, vi, afterEach } from 'vitest';
import * as cp from 'child_process';
import { EventEmitter } from 'events';

vi.mock('child_process');

const { AcpClient } = await import('../src/client.js');

function makeOutputChannel() {
  return { appendLine: vi.fn(), show: vi.fn() } as any;
}

function makeMockProcess() {
  const stdin  = { write: vi.fn(), end: vi.fn() } as any;
  const stdout = new EventEmitter() as any;
  const proc   = Object.assign(new EventEmitter(), { stdin, stdout, pid: 42, killed: false }) as any;
  proc.kill   = vi.fn();
  vi.mocked(cp.spawn).mockReturnValue(proc);
  return { proc, stdin, stdout };
}

describe('AcpClient', () => {
  afterEach(() => vi.resetAllMocks());

  it('spawns mini-agent --acp on start()', async () => {
    const { proc, stdout } = makeMockProcess();
    const client = new AcpClient('/path/to/mini-agent', makeOutputChannel());
    
    const startPromise = client.start();
    // Simulate process emitting ready signal
    stdout.emit('data', Buffer.from('{"id":"0","type":"ready"}\n'));
    await startPromise;

    expect(cp.spawn).toHaveBeenCalledWith(
      '/path/to/mini-agent',
      ['--acp'],
      expect.objectContaining({ stdio: ['pipe', 'pipe', 'pipe'] })
    );
  });

  it('stop() kills the child process', async () => {
    const { proc, stdout } = makeMockProcess();
    const client = new AcpClient('/path/to/mini-agent', makeOutputChannel());
    const startPromise = client.start();
    stdout.emit('data', Buffer.from('{"id":"0","type":"ready"}\n'));
    await startPromise;

    client.stop();
    expect(proc.kill).toHaveBeenCalled();
  });

  it('send() writes a framed JSON message to stdin', async () => {
    const { proc, stdin, stdout } = makeMockProcess();
    const client = new AcpClient('/path/to/mini-agent', makeOutputChannel());
    const startPromise = client.start();
    stdout.emit('data', Buffer.from('{"id":"0","type":"ready"}\n'));
    await startPromise;

    const responsePromise = client.send({ id: '1', type: 'chat', content: 'hello' });
    stdout.emit('data', Buffer.from('{"id":"1","type":"chat_response","content":"hi"}\n'));
    const response = await responsePromise;

    expect(stdin.write).toHaveBeenCalledWith(
      expect.stringContaining('"type":"chat"')
    );
    expect(response).toMatchObject({ type: 'chat_response', content: 'hi' });
  });
});
```

- [ ] **Step 2: Run tests — expect failure**

```bash
cd vscode-extension
npx vitest run test/client.test.ts
```

Expected: FAIL — `Cannot find module '../src/client.js'`

- [ ] **Step 3: Implement `vscode-extension/src/client.ts`**

```typescript
import { spawn, ChildProcess } from 'child_process';
import { OutputChannel } from 'vscode';

export type AcpMessage = { id: string; type: string; [key: string]: unknown };
type MessageHandler = (msg: AcpMessage) => void;

export class AcpClient {
  private proc: ChildProcess | null = null;
  private pending = new Map<string, (msg: AcpMessage) => void>();
  private handlers: MessageHandler[] = [];
  private buffer = '';

  constructor(
    private readonly binaryPath: string,
    private readonly output: OutputChannel,
  ) {}

  start(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.proc = spawn(this.binaryPath, ['--acp'], {
        stdio: ['pipe', 'pipe', 'pipe'],
      });

      this.proc.stdout!.on('data', (chunk: Buffer) => {
        this.buffer += chunk.toString('utf8');
        const lines = this.buffer.split('\n');
        this.buffer = lines.pop() ?? '';
        for (const line of lines) {
          if (!line.trim()) continue;
          try {
            const msg: AcpMessage = JSON.parse(line);
            this.output.appendLine(`← ${line}`);
            if (msg.type === 'ready') { resolve(); continue; }
            const handler = this.pending.get(msg.id);
            if (handler) { this.pending.delete(msg.id); handler(msg); }
            for (const h of this.handlers) h(msg);
          } catch (e) {
            this.output.appendLine(`parse error: ${e}`);
          }
        }
      });

      this.proc.stderr!.on('data', (d: Buffer) => this.output.appendLine(d.toString()));
      this.proc.on('error', reject);
      this.proc.on('exit', (code) => {
        this.output.appendLine(`mini-agent exited (${code})`);
        this.proc = null;
      });
    });
  }

  send(message: AcpMessage): Promise<AcpMessage> {
    return new Promise((resolve) => {
      this.pending.set(message.id, resolve);
      const line = JSON.stringify(message) + '\n';
      this.output.appendLine(`→ ${line.trim()}`);
      this.proc?.stdin!.write(line);
    });
  }

  onMessage(handler: MessageHandler): void {
    this.handlers.push(handler);
  }

  stop(): void {
    this.proc?.kill();
    this.proc = null;
  }

  get isRunning(): boolean {
    return this.proc !== null;
  }
}
```

- [ ] **Step 4: Run tests — expect pass**

```bash
cd vscode-extension
npm run compile && npx vitest run test/client.test.ts
```

Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add vscode-extension/src/client.ts vscode-extension/test/client.test.ts
git commit -m "feat(vscode): ACP stdio client with newline-framed JSON"
```

---

## Task 4: Commands and extension entry point

**Files:**
- Create: `vscode-extension/src/commands.ts`
- Modify: `vscode-extension/src/extension.ts`
- Create: `vscode-extension/media/icon.svg` (minimal placeholder)

**Interfaces:**
- Consumes: `resolveBinary` (Task 2), `AcpClient` (Task 3)
- Produces: fully wired extension that activates, starts the agent, and registers all three commands

- [ ] **Step 1: Create `vscode-extension/src/commands.ts`**

```typescript
import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import * as os from 'os';
import { AcpClient } from './client.js';

export function registerCommands(
  context: vscode.ExtensionContext,
  client: AcpClient,
  output: vscode.OutputChannel,
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('mini-agent.openConfig', () => openConfig(context)),
    vscode.commands.registerCommand('mini-agent.restart',    () => restartAgent(client, output)),
    vscode.commands.registerCommand('mini-agent.showOutput', () => output.show()),
  );
}

async function openConfig(context: vscode.ExtensionContext): Promise<void> {
  const configDir = path.join(os.homedir(), '.config', 'mini-agent');
  const configPath = path.join(configDir, 'config.toml');

  if (!fs.existsSync(configPath)) {
    fs.mkdirSync(configDir, { recursive: true });
    const template = path.join(context.extensionPath, 'config.template.toml');
    fs.copyFileSync(template, configPath);
  }

  const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(configPath));
  await vscode.window.showTextDocument(doc);
}

async function restartAgent(client: AcpClient, output: vscode.OutputChannel): Promise<void> {
  output.appendLine('Restarting mini-agent...');
  client.stop();
  await client.start();
  output.appendLine('mini-agent restarted.');
}
```

- [ ] **Step 2: Rewrite `vscode-extension/src/extension.ts`**

```typescript
import * as vscode from 'vscode';
import { resolveBinary } from './binary.js';
import { AcpClient } from './client.js';
import { registerCommands } from './commands.js';
import { MiniAgentPanel } from './panel.js';

let client: AcpClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const output = vscode.window.createOutputChannel('mini-agent');
  context.subscriptions.push(output);

  const binaryPath = resolveBinary(context.extensionPath);
  if (!binaryPath) {
    vscode.window.showErrorMessage(
      'mini-agent binary not found. Install it via cargo or download from GitHub.',
    );
    return;
  }

  client = new AcpClient(binaryPath, output);
  registerCommands(context, client, output);

  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  statusBar.command = 'mini-agent.showOutput';
  context.subscriptions.push(statusBar);

  const panel = new MiniAgentPanel(context, client);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('mini-agent.panel', panel),
  );

  try {
    await client.start();
    statusBar.text = '$(check) mini-agent';
    statusBar.tooltip = 'mini-agent connected';
  } catch (e) {
    statusBar.text = '$(warning) mini-agent';
    statusBar.tooltip = `mini-agent failed to start: ${e}`;
    output.appendLine(`Failed to start: ${e}`);
  }

  statusBar.show();
}

export function deactivate(): void {
  client?.stop();
}
```

- [ ] **Step 3: Create minimal SVG icon**

Create `vscode-extension/media/icon.svg`:

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="currentColor">
  <circle cx="8" cy="8" r="6" opacity="0.8"/>
  <text x="8" y="12" text-anchor="middle" font-size="8" fill="white" font-family="monospace">m</text>
</svg>
```

- [ ] **Step 4: Compile and check for errors**

```bash
cd vscode-extension
npm run compile 2>&1
```

Expected: no TypeScript errors. (panel.ts import will error until Task 5 — stub it if needed.)

If panel.ts is missing, add a temporary stub `src/panel.ts`:
```typescript
import * as vscode from 'vscode';
import { AcpClient } from './client.js';
export class MiniAgentPanel implements vscode.WebviewViewProvider {
  constructor(_ctx: vscode.ExtensionContext, _client: AcpClient) {}
  resolveWebviewView(_view: vscode.WebviewView): void {}
}
```

- [ ] **Step 5: Compile clean**

```bash
cd vscode-extension
npm run compile
```

Expected: `dist/extension.js` created, no errors.

- [ ] **Step 6: Commit**

```bash
git add vscode-extension/src/ vscode-extension/media/icon.svg
git commit -m "feat(vscode): commands, entry point, status bar"
```

---

## Task 5: Chat webview panel

**Files:**
- Create: `vscode-extension/src/panel.ts`
- Create: `vscode-extension/media/panel.html`
- Create: `vscode-extension/media/panel.css`
- Create: `vscode-extension/media/panel.js`

**Interfaces:**
- Consumes: `AcpClient.send()`, `AcpClient.onMessage()` (Task 3)
- Produces: `class MiniAgentPanel implements vscode.WebviewViewProvider` — replaces the stub from Task 4

- [ ] **Step 1: Create `vscode-extension/media/panel.css`**

```css
* { box-sizing: border-box; margin: 0; padding: 0; }

body {
  font-family: var(--vscode-font-family);
  font-size: var(--vscode-font-size);
  color: var(--vscode-foreground);
  background: var(--vscode-sideBar-background);
  display: flex;
  flex-direction: column;
  height: 100vh;
  padding: 8px;
  gap: 8px;
}

#history {
  flex: 1;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.message {
  padding: 6px 10px;
  border-radius: 4px;
  max-width: 100%;
  word-break: break-word;
  white-space: pre-wrap;
}

.message.user {
  background: var(--vscode-inputOption-activeBackground);
  align-self: flex-end;
}

.message.agent {
  background: var(--vscode-editor-inactiveSelectionBackground);
  align-self: flex-start;
}

#input-row {
  display: flex;
  gap: 6px;
}

#input {
  flex: 1;
  background: var(--vscode-input-background);
  color: var(--vscode-input-foreground);
  border: 1px solid var(--vscode-input-border, transparent);
  border-radius: 4px;
  padding: 6px 8px;
  font-family: inherit;
  font-size: inherit;
  resize: none;
  min-height: 36px;
  max-height: 120px;
}

#input:focus { outline: 1px solid var(--vscode-focusBorder); }

#send {
  background: var(--vscode-button-background);
  color: var(--vscode-button-foreground);
  border: none;
  border-radius: 4px;
  padding: 0 12px;
  cursor: pointer;
  font-size: inherit;
}

#send:hover { background: var(--vscode-button-hoverBackground); }
```

- [ ] **Step 2: Create `vscode-extension/media/panel.js`**

```js
(function () {
  const vscode = acquireVsCodeApi();
  const history = document.getElementById('history');
  const input   = document.getElementById('input');
  const send    = document.getElementById('send');

  function appendMessage(role, text) {
    const div = document.createElement('div');
    div.className = `message ${role}`;
    div.textContent = text;
    history.appendChild(div);
    history.scrollTop = history.scrollHeight;
  }

  function submit() {
    const text = input.value.trim();
    if (!text) return;
    appendMessage('user', text);
    vscode.postMessage({ type: 'chat', content: text });
    input.value = '';
    input.style.height = '';
  }

  send.addEventListener('click', submit);

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
    // auto-resize
    input.style.height = '';
    input.style.height = Math.min(input.scrollHeight, 120) + 'px';
  });

  window.addEventListener('message', (event) => {
    const msg = event.data;
    if (msg.type === 'agent_message') {
      appendMessage('agent', msg.content);
    }
  });
})();
```

- [ ] **Step 3: Create `vscode-extension/media/panel.html`**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta http-equiv="Content-Security-Policy"
        content="default-src 'none'; style-src ${cspSource}; script-src 'nonce-${nonce}';">
  <link rel="stylesheet" href="${stylesUri}">
  <title>mini-agent</title>
</head>
<body>
  <div id="history"></div>
  <div id="input-row">
    <textarea id="input" placeholder="Ask mini-agent..." rows="1"></textarea>
    <button id="send">Send</button>
  </div>
  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>
```

- [ ] **Step 4: Create `vscode-extension/src/panel.ts`**

```typescript
import * as vscode from 'vscode';
import * as crypto from 'crypto';
import { AcpClient, AcpMessage } from './client.js';

export class MiniAgentPanel implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private msgCounter = 0;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: AcpClient,
  ) {
    client.onMessage((msg) => this.handleAgentMessage(msg));
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, 'media'),
      ],
    };
    view.webview.html = this.buildHtml(view.webview);
    view.webview.onDidReceiveMessage((msg) => this.handleWebviewMessage(msg));
  }

  private buildHtml(webview: vscode.Webview): string {
    const nonce = crypto.randomBytes(16).toString('hex');
    const cspSource = webview.cspSource;
    const mediaUri = (file: string) =>
      webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', file));

    const html = require('fs').readFileSync(
      require('path').join(this.context.extensionPath, 'media', 'panel.html'),
      'utf8'
    );

    return html
      .replace(/\$\{nonce\}/g, nonce)
      .replace(/\$\{cspSource\}/g, cspSource)
      .replace('${stylesUri}', mediaUri('panel.css').toString())
      .replace('${scriptUri}', mediaUri('panel.js').toString());
  }

  private async handleWebviewMessage(msg: { type: string; content: string }): Promise<void> {
    if (msg.type !== 'chat') return;
    const id = String(++this.msgCounter);
    await this.client.send({ id, type: 'chat', content: msg.content });
  }

  private handleAgentMessage(msg: AcpMessage): void {
    if (msg.type !== 'chat_response') return;
    this.view?.webview.postMessage({
      type: 'agent_message',
      content: msg.content ?? '',
    });
  }
}
```

- [ ] **Step 5: Remove panel stub and compile clean**

Delete the stub in `src/panel.ts` if it still exists (the real implementation replaces it).

```bash
cd vscode-extension
npm run compile 2>&1
```

Expected: no errors.

- [ ] **Step 6: Smoke test in VS Code (manual)**

```bash
cd vscode-extension
# Press F5 in VS Code to open Extension Development Host
# OR:
npx vsce package --no-dependencies -o test.vsix
code --install-extension test.vsix
```

Open VS Code, open the mini-agent sidebar panel. The input box should appear. Sending a message should write to the output channel (even if mini-agent binary isn't present, the panel should render without crashing).

Expected: panel renders, status bar shows `⚠ mini-agent` if binary not found, `✓ mini-agent` if it is.

- [ ] **Step 7: Commit**

```bash
git add vscode-extension/src/panel.ts vscode-extension/media/
git commit -m "feat(vscode): chat webview panel with VS Code theme variables"
```

---

## Task 6: Windows MSI installer (WiX v4)

**Files:**
- Create: `packaging/windows/mini-agent.wxs`
- Create: `packaging/windows/installer.wixproj`
- Create: `packaging/windows/build.ps1`
- Create: `packaging/windows/assets/license.rtf`
- Create: `packaging/windows/assets/banner.bmp` (placeholder note)
- Create: `packaging/windows/assets/dialog.bmp` (placeholder note)

**Interfaces:**
- Consumes: `mini-agent.exe` (pre-built), `mini-agent-win32-x64.vsix` (from Task 5 packaging)
- Produces: `mini-agent-x64.msi` — silent-install capable, no elevation required

- [ ] **Step 1: Create `packaging/windows/installer.wixproj`**

```xml
<Project Sdk="WixToolset.Sdk/4.0.5">
  <PropertyGroup>
    <OutputName>mini-agent-x64</OutputName>
    <Platform>x64</Platform>
    <InstallerPlatform>x64</InstallerPlatform>
  </PropertyGroup>
</Project>
```

- [ ] **Step 2: Create `packaging/windows/mini-agent.wxs`**

```xml
<?xml version="1.0" encoding="utf-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="mini-agent"
           Manufacturer="mini-agent contributors"
           Version="!(bind.FileVersion.mini_agent_exe)"
           UpgradeCode="PUT-A-STABLE-GUID-HERE"
           Scope="perUser"
           Compressed="yes">

    <MajorUpgrade DowngradeErrorMessage="A newer version is already installed." />

    <MediaTemplate EmbedCab="yes" />

    <!-- Install directory: %LOCALAPPDATA%\Programs\mini-agent -->
    <StandardDirectory Id="LocalAppDataFolder">
      <Directory Id="INSTALLDIR" Name="Programs">
        <Directory Id="MIНИAGENTDIR" Name="mini-agent" />
      </Directory>
    </StandardDirectory>

    <ComponentGroup Id="ProductComponents" Directory="MIНИAGENTDIR">
      <!-- Main binary -->
      <Component Id="MiniAgentExe" Guid="PUT-A-STABLE-GUID-HERE">
        <File Id="mini_agent_exe"
              Source="$(var.BinaryDir)\mini-agent.exe"
              KeyPath="yes" />
        <!-- Add install dir to user PATH -->
        <Environment Id="PATH"
                     Name="PATH"
                     Value="[MIНИAGENTDIR]"
                     Part="last"
                     Action="set"
                     System="no"
                     Permanent="no" />
      </Component>

      <!-- Bundled VSIX for silent VS Code extension install -->
      <Component Id="VsixBundle" Guid="PUT-A-STABLE-GUID-HERE">
        <File Id="mini_agent_vsix"
              Source="$(var.VsixDir)\mini-agent-win32-x64.vsix"
              KeyPath="yes" />
      </Component>
    </ComponentGroup>

    <Feature Id="ProductFeature" Title="mini-agent" Level="1">
      <ComponentGroupRef Id="ProductComponents" />
    </Feature>

    <!-- Silently install VS Code extension if VS Code is on PATH -->
    <CustomAction Id="InstallVsix"
                  Directory="MIНИAGENTDIR"
                  Execute="deferred"
                  Impersonate="yes"
                  ExeCommand="cmd.exe /c where code &gt;nul 2&gt;&amp;1 &amp;&amp; code --install-extension &quot;[MIНИAGENTDIR]mini-agent-win32-x64.vsix&quot; --force"
                  Return="ignore" />

    <InstallExecuteSequence>
      <Custom Action="InstallVsix" After="InstallFiles">NOT Installed</Custom>
    </InstallExecuteSequence>
  </Package>
</Wix>
```

**Important:** Replace each `PUT-A-STABLE-GUID-HERE` with a unique GUID generated once via `[guid]::NewGuid()` in PowerShell or `uuidgen` on macOS/Linux. These must be stable across releases.

- [ ] **Step 3: Generate stable GUIDs and commit them**

```powershell
# Run in PowerShell:
[guid]::NewGuid()  # UpgradeCode — one per product, never change
[guid]::NewGuid()  # MiniAgentExe component
[guid]::NewGuid()  # VsixBundle component
```

Replace the `PUT-A-STABLE-GUID-HERE` placeholders with the generated values in braces format: `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.

- [ ] **Step 4: Create minimal `packaging/windows/assets/license.rtf`**

```
{\rtf1\ansi GPL-3.0-only\par See https://www.gnu.org/licenses/gpl-3.0.html for the full license text.\par}
```

For production, replace with the full GPL-3 RTF. A GPL-3 RTF is available at: https://www.gnu.org/licenses/gpl-3.0.rtf

- [ ] **Step 5: Create `packaging/windows/build.ps1`** (local build helper)

```powershell
param(
  [Parameter(Mandatory)][string]$BinaryDir,  # path to dir containing mini-agent.exe
  [Parameter(Mandatory)][string]$VsixDir,    # path to dir containing mini-agent-win32-x64.vsix
  [string]$OutDir = "."
)

$ErrorActionPreference = "Stop"

# Ensure dotnet wix tool is installed
if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
  dotnet tool install --global wix --version 4.0.5
}

wix build .\mini-agent.wxs `
    -d BinaryDir="$BinaryDir" `
    -d VsixDir="$VsixDir" `
    -out "$OutDir\mini-agent-x64.msi"

Write-Host "Built: $OutDir\mini-agent-x64.msi"
```

- [ ] **Step 6: Verify WiX syntax (requires Windows or Wine)**

On a Windows machine or CI runner with WiX installed:

```powershell
cd packaging/windows
.\build.ps1 -BinaryDir "C:\path\to\binary" -VsixDir "C:\path\to\vsix" -OutDir "."
```

On non-Windows: validate XML structure is well-formed:
```bash
xmllint --noout packaging/windows/mini-agent.wxs && echo "XML valid"
```

Expected: `XML valid` (full build validation requires Windows).

- [ ] **Step 7: Commit**

```bash
git add packaging/windows/
git commit -m "feat(windows): WiX v4 MSI installer, perUser scope, silent VS Code extension install"
```

---

## Task 7: CI/CD — release workflow

**Files:**
- Create: `.github/workflows/vscode-publish.yml`

**Interfaces:**
- Consumes: binary artifacts uploaded by `rust.yml` matrix (artifact names: `mini-agent-<target>` where target uses `cross` naming: `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, etc.)
- Produces: per-platform `.vsix` files published to marketplace; `mini-agent-x64.msi` uploaded as release asset

- [ ] **Step 1: Audit `rust.yml` artifact upload names**

Read `.github/workflows/rust.yml` and confirm:
- What artifact name is used for `actions/upload-artifact`
- What the matrix target names are (e.g., `x86_64-pc-windows-msvc`)
- What the binary filename is per target

Map each cross target to a vsce target:

| Cross target | vsce target |
|---|---|
| `x86_64-pc-windows-msvc` | `win32-x64` |
| `aarch64-pc-windows-msvc` | `win32-arm64` |
| `x86_64-apple-darwin` | `darwin-x64` |
| `aarch64-apple-darwin` | `darwin-arm64` |
| `x86_64-unknown-linux-musl` | `linux-x64` |
| `aarch64-unknown-linux-musl` | `linux-arm64` |

Update the matrix in the workflow below to match actual artifact names from `rust.yml`.

- [ ] **Step 2: Create `.github/workflows/vscode-publish.yml`**

```yaml
name: VS Code Extension + MSI Publish

on:
  release:
    types: [published]

jobs:
  publish-vscode:
    name: Package and publish VS Code extension
    runs-on: ubuntu-latest
    needs: []  # Replace with the build job name from rust.yml, e.g.: needs: [build]

    strategy:
      matrix:
        include:
          - vsce_target: win32-x64
            artifact_name: mini-agent-x86_64-pc-windows-msvc
            binary_name: mini-agent.exe
          - vsce_target: win32-arm64
            artifact_name: mini-agent-aarch64-pc-windows-msvc
            binary_name: mini-agent.exe
          - vsce_target: darwin-x64
            artifact_name: mini-agent-x86_64-apple-darwin
            binary_name: mini-agent
          - vsce_target: darwin-arm64
            artifact_name: mini-agent-aarch64-apple-darwin
            binary_name: mini-agent
          - vsce_target: linux-x64
            artifact_name: mini-agent-x86_64-unknown-linux-musl
            binary_name: mini-agent
          - vsce_target: linux-arm64
            artifact_name: mini-agent-aarch64-unknown-linux-musl
            binary_name: mini-agent

    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: '20'

      - name: Install extension dependencies
        run: npm ci
        working-directory: vscode-extension

      - name: Compile TypeScript
        run: npm run compile
        working-directory: vscode-extension

      - name: Download binary artifact
        uses: actions/download-artifact@v4
        with:
          name: ${{ matrix.artifact_name }}
          path: vscode-extension/bin/${{ matrix.vsce_target }}/

      - name: Set executable bit (non-Windows binaries)
        if: ${{ !startsWith(matrix.vsce_target, 'win32') }}
        run: chmod +x vscode-extension/bin/${{ matrix.vsce_target }}/mini-agent

      - name: Package VSIX
        run: npx vsce package --target ${{ matrix.vsce_target }} --no-dependencies -o mini-agent-${{ matrix.vsce_target }}.vsix
        working-directory: vscode-extension
        env:
          VSCE_TARGET: ${{ matrix.vsce_target }}

      - name: Upload VSIX as release asset
        uses: softprops/action-gh-release@v2
        with:
          files: vscode-extension/mini-agent-${{ matrix.vsce_target }}.vsix

      - name: Publish to marketplace
        run: npx vsce publish --packagePath mini-agent-${{ matrix.vsce_target }}.vsix
        working-directory: vscode-extension
        env:
          VSCE_PAT: ${{ secrets.VSCE_PAT }}

  build-msi:
    name: Build Windows MSI
    runs-on: windows-latest
    needs: [publish-vscode]

    steps:
      - uses: actions/checkout@v4

      - name: Install WiX v4
        run: dotnet tool install --global wix --version 4.0.5

      - name: Download Windows binary
        uses: actions/download-artifact@v4
        with:
          name: mini-agent-x86_64-pc-windows-msvc
          path: packaging/windows/artifacts/binary/

      - name: Download win32-x64 VSIX
        uses: actions/download-artifact@v4
        with:
          name: mini-agent-win32-x64       # uploaded by publish-vscode job
          path: packaging/windows/artifacts/vsix/

      - name: Build MSI
        run: |
          wix build mini-agent.wxs `
            -d BinaryDir="${{ github.workspace }}\packaging\windows\artifacts\binary" `
            -d VsixDir="${{ github.workspace }}\packaging\windows\artifacts\vsix" `
            -out mini-agent-x64.msi
        working-directory: packaging/windows

      - name: Upload MSI as release asset
        uses: softprops/action-gh-release@v2
        with:
          files: packaging/windows/mini-agent-x64.msi
```

- [ ] **Step 3: Verify `needs` dependency matches `rust.yml`**

Read `.github/workflows/rust.yml` and confirm the job name that uploads binary artifacts. Update `needs: []` in `publish-vscode` to reference it.

If `rust.yml` uses a matrix job named `build`, replace `needs: []` with `needs: [build]`.

- [ ] **Step 4: Add `VSCE_PAT` secret to GitHub**

This is a one-time manual step:
1. Go to `marketplace.visualstudio.com` → create publisher `mini-agent`
2. In Azure DevOps: User Settings → Personal Access Tokens → New Token → Marketplace (Manage)
3. In GitHub repo: Settings → Secrets → Actions → New: `VSCE_PAT` = the PAT value

Document this in `docs/superpowers/specs/2026-08-15-vscode-windows-distribution.md` under Publisher Setup (already documented there).

- [ ] **Step 5: Dry-run the workflow on a test tag**

```bash
git tag v0.0.0-vsix-test
git push origin v0.0.0-vsix-test
# Watch GitHub Actions — the workflow should trigger
# (publish step will fail without VSCE_PAT — that's expected for a dry run)
# Verify: TypeScript compiles, VSIX is packaged, artifact is uploaded
```

Delete the test tag after verification:
```bash
git push origin --delete v0.0.0-vsix-test
git tag -d v0.0.0-vsix-test
```

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/vscode-publish.yml
git commit -m "ci: VS Code extension + MSI publish workflow on release tags"
```

---

## Self-Review

**Spec coverage check:**

| Spec requirement | Covered by |
|---|---|
| `vscode-extension/` directory with all listed files | Tasks 1–5 |
| `packaging/windows/` with WiX sources | Task 6 |
| `.github/workflows/vscode-publish.yml` | Task 7 |
| Binary resolution: PATH → bundled fallback | Task 2 |
| Spawn `mini-agent --acp` over stdio | Task 3 |
| ACP newline-framed JSON client | Task 3 |
| Sidebar webview with VS Code theme variables | Task 5 |
| Open Config / Restart Agent / Show Output commands | Task 4 |
| config.template.toml bundled | Task 1 |
| `bin/` gitignored, per-platform | Task 1 |
| `.vscodeignore` excludes other platforms' bins | Task 1 |
| Per-platform `.vsix` packaging | Task 7 |
| `vsce publish --target` per platform | Task 7 |
| `.vsix` uploaded as release assets (corporate VSIX side-load) | Task 7 |
| MSI: perUser install, no elevation | Task 6 |
| MSI: user PATH update | Task 6 |
| MSI: silent VS Code extension install if VS Code detected | Task 6 |
| MSI: `msiexec /quiet` compatible | Task 6 (perUser WiX produces standard MSI) |
| MSI bundled in release assets | Task 7 |
| One-time publisher setup documented | Task 7, step 4 |
| Rust binary build unchanged | Confirmed — no `src/` files touched |

**Placeholder scan:** No TBDs or TODOs found in task steps.

**Type consistency:** `AcpMessage`, `AcpClient`, `resolveBinary` — names consistent across Tasks 2–5.

**One gap found and added:** The `needs` dependency in the workflow (Task 7 Step 3) requires manual verification against `rust.yml` — this is correct since the artifact names are not known until that file is read.
