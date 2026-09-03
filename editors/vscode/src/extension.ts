import * as vscode from 'vscode';
import * as os from 'node:os';
import type * as acp from '@agentclientprotocol/sdk';
import { ChatUpdateRenderer, stopResult } from './chat';
import { ensureConfigFile, resolveConfigDirectory } from './config';
import { resolveConfiguredExecutable } from './executable';
import { buildPermissionDetail, permissionOptionTitle } from './permission';
import { AgentSession } from './session';
import { log, setLogLevel, showOutput } from './log';
import { assertExecutableScope, gatedFolderPick } from './trust';

let session: AgentSession | undefined;
let sessionCreation: Promise<AgentSession | undefined> | undefined;
let sessionGeneration = 0;

export async function discardSession(): Promise<void> {
  sessionGeneration += 1;
  sessionCreation = undefined;
  const current = session;
  session = undefined;
  if (!current) { return; }
  try {
    await current.stop();
  } finally {
    current.dispose();
  }
}

export function activate(context: vscode.ExtensionContext): void {
  const cfg = vscode.workspace.getConfiguration('mini-agent');
  setLogLevel(cfg.get<string>('logLevel', 'info'));

  log.info('Mini Agent extension activating');
  assertExecutableScope();

  context.subscriptions.push(
    log.channel,
    registerChatParticipant(context),
    vscode.commands.registerCommand('mini-agent.start', () => cmdStart(context)),
    vscode.commands.registerCommand('mini-agent.stop', () => cmdStop()),
    vscode.commands.registerCommand('mini-agent.restart', () => cmdRestart(context)),
    vscode.commands.registerCommand('mini-agent.selectFolder', () => cmdSelectFolder(context)),
    vscode.commands.registerCommand('mini-agent.openConfig', () => cmdOpenConfig()),
    vscode.commands.registerCommand('mini-agent.showOutput', () => showOutput()),
    vscode.workspace.onDidChangeConfiguration(e => {
      if (e.affectsConfiguration('mini-agent.logLevel')) {
        setLogLevel(vscode.workspace.getConfiguration('mini-agent').get<string>('logLevel', 'info'));
      }
      if (e.affectsConfiguration('mini-agent.executablePath')) {
        assertExecutableScope();
      }
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(async event => {
      if (session && event.removed.some(folder => folder.uri.toString() === session?.workspaceFolder.uri.toString())) {
        await discardSession();
      } else if (sessionCreation && event.removed.length > 0) {
        await discardSession();
      }
    }),
  );
}

export async function deactivate(): Promise<void> {
  log.info('Mini Agent extension deactivating');
  await discardSession();
}

export async function cmdStart(context: vscode.ExtensionContext): Promise<void> {
  if (session) {
    const confirm = await vscode.window.showWarningMessage(
      'A Mini Agent session is already running. Replace it?',
      { modal: true },
      'Replace',
    );
    if (confirm !== 'Replace') { return; }
    await discardSession();
  }

  const active = await ensureSession(context);
  await active?.start();
}

async function cmdStop(): Promise<void> {
  if (!session && !sessionCreation) {
    void vscode.window.showInformationMessage('No Mini Agent session is running.');
    return;
  }
  await discardSession();
}

async function cmdRestart(context: vscode.ExtensionContext): Promise<void> {
  await cmdStop();
  await cmdStart(context);
}

async function cmdSelectFolder(context: vscode.ExtensionContext): Promise<void> {
  await cmdStart(context);
}

async function cmdOpenConfig(): Promise<void> {
  try {
    const configDirectory = resolveConfigDirectory(process.platform, os.homedir(), process.env);
    const configPath = await ensureConfigFile(configDirectory);
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(configPath));
    await vscode.window.showTextDocument(document);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    log.error(`Unable to open Mini Agent config: ${message}`);
    void vscode.window.showErrorMessage(`Unable to open Mini Agent config: ${message}`);
  }
}

function resolveExecutable(context: vscode.ExtensionContext): string | undefined {
  const cfg = vscode.workspace.getConfiguration('mini-agent');
  // Read from machine scope only; workspace/folder settings cannot override this.
  const configured = cfg.inspect<string>('executablePath');
  const path = configured?.globalValue ?? configured?.defaultValue ?? '';

  if (path.trim()) {
    const resolved = resolveConfiguredExecutable(path, os.homedir(), process.platform);
    if (!resolved.ok) {
      log.error(resolved.reason);
      void vscode.window.showErrorMessage(`Cannot start Mini Agent: ${resolved.reason}`);
      return undefined;
    }
    if (resolved.path !== path) {
      log.info(`Resolved mini-agent.executablePath "${path}" to "${resolved.path}"`);
    }
    return resolved.path;
  }

  // Fall back to the bundled artifact co-located with the extension.
  const platform = `${process.platform}-${process.arch}`;
  const bundled = vscode.Uri.joinPath(
    context.extensionUri,
    'bin',
    platform,
    process.platform === 'win32' ? 'mini-agent.exe' : 'mini-agent',
  );
  return bundled.fsPath;
}

function registerChatParticipant(context: vscode.ExtensionContext): vscode.ChatParticipant {
  const participant = vscode.chat.createChatParticipant(
    'mini-agent.chat',
    async (request, _chatContext, response, token): Promise<vscode.ChatResult> => {
      if (!request.prompt.trim()) {
        return { errorDetails: { message: 'Enter a prompt for Mini Agent.' } };
      }

      const active = await ensureSession(context);
      if (!active) {
        return { errorDetails: { message: 'Mini Agent requires a trusted local workspace folder.' } };
      }

      const cancellation = new AbortController();
      if (token.isCancellationRequested) { cancellation.abort(); }
      const cancellationSubscription = token.onCancellationRequested(() => cancellation.abort());
      const renderer = new ChatUpdateRenderer();
      response.progress('Connecting to Mini Agent…');

      try {
        const reason = await active.prompt(request.prompt, update => {
          for (const event of renderer.render(update)) {
            if (event.kind === 'markdown') { response.markdown(event.value); }
            else { response.progress(event.value); }
          }
        }, cancellation.signal);
        return stopResult(reason);
      } catch (error) {
        if (cancellation.signal.aborted) { return stopResult('cancelled'); }
        const message = error instanceof Error ? error.message : String(error);
        log.error(`Chat request failed: ${message}`);
        return { errorDetails: { message: `Mini Agent request failed: ${message}` } };
      } finally {
        cancellationSubscription.dispose();
      }
    },
  );
  participant.iconPath = new vscode.ThemeIcon('sparkle');
  return participant;
}

export async function ensureSession(context: vscode.ExtensionContext): Promise<AgentSession | undefined> {
  if (!vscode.workspace.isTrusted) {
    await discardSession();
    return undefined;
  }
  if (session) { return session; }
  if (sessionCreation) { return sessionCreation; }
  // Latch chat and command callers onto one creation attempt. The generation
  // invalidates a folder picker that resolves after stop, restart, trust loss,
  // or workspace-folder removal.
  const generation = sessionGeneration;
  const tracked = createSession(context, generation).finally(() => {
    if (sessionCreation === tracked) { sessionCreation = undefined; }
  });
  sessionCreation = tracked;
  return sessionCreation;
}

async function createSession(
  context: vscode.ExtensionContext,
  generation: number,
): Promise<AgentSession | undefined> {
  const folder = await gatedFolderPick();
  if (!folder || generation !== sessionGeneration) { return undefined; }
  const executablePath = resolveExecutable(context);
  if (!executablePath || generation !== sessionGeneration) { return undefined; }
  const created = new AgentSession(executablePath, folder, context, requestPermission);
  session = created;
  return created;
}

async function requestPermission(
  request: acp.RequestPermissionRequest,
  signal: AbortSignal,
): Promise<acp.RequestPermissionResponse> {
  if (signal.aborted || !vscode.workspace.isTrusted) {
    return { outcome: { outcome: 'cancelled' } };
  }

  interface PermissionItem extends vscode.MessageItem { readonly optionIndex: number }
  const items = request.options.map((option, index): PermissionItem => ({
    title: permissionOptionTitle(request, option),
    isCloseAffordance: option.kind.startsWith('reject'),
    optionIndex: index,
  }));
  // The detail shows the actual command/input (content text blocks and rawInput),
  // not just the tool name, so "Allow always" never persists an unseen rule.
  const selected = await vscode.window.showWarningMessage(
    `Mini Agent requests permission: ${request.toolCall.title ?? 'tool call'}`,
    { modal: true, detail: buildPermissionDetail(request) },
    ...items,
  );
  if (!selected || signal.aborted) { return { outcome: { outcome: 'cancelled' } }; }
  const option = request.options[selected.optionIndex];
  if (!option) { return { outcome: { outcome: 'cancelled' } }; }
  return { outcome: { outcome: 'selected', optionId: option.optionId } };
}
