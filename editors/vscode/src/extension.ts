import * as vscode from 'vscode';
import { AgentSession } from './session';
import { log, setLogLevel } from './log';
import { assertExecutableScope, gatedFolderPick, onTrustRevoked } from './trust';

let session: AgentSession | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const cfg = vscode.workspace.getConfiguration('mini-agent');
  setLogLevel(cfg.get<string>('logLevel', 'info'));

  log.info('Mini Agent extension activating');
  assertExecutableScope();

  context.subscriptions.push(
    vscode.commands.registerCommand('mini-agent.start', () => cmdStart(context)),
    vscode.commands.registerCommand('mini-agent.stop', () => cmdStop()),
    vscode.commands.registerCommand('mini-agent.restart', () => cmdRestart(context)),
    vscode.commands.registerCommand('mini-agent.selectFolder', () => cmdSelectFolder(context)),
    vscode.workspace.onDidChangeConfiguration(e => {
      if (e.affectsConfiguration('mini-agent.logLevel')) {
        setLogLevel(vscode.workspace.getConfiguration('mini-agent').get<string>('logLevel', 'info'));
      }
      if (e.affectsConfiguration('mini-agent.executablePath')) {
        assertExecutableScope();
      }
    }),
    onTrustRevoked(async () => {
      await session?.stop();
      session = undefined;
    }),
  );
}

export async function deactivate(): Promise<void> {
  log.info('Mini Agent extension deactivating');
  await session?.stop();
  session = undefined;
}

async function cmdStart(context: vscode.ExtensionContext): Promise<void> {
  const folder = await gatedFolderPick();
  if (!folder) { return; }

  if (session) {
    const confirm = await vscode.window.showWarningMessage(
      'A Mini Agent session is already running. Replace it?',
      { modal: true },
      'Replace',
    );
    if (confirm !== 'Replace') { return; }
    await session.stop();
    session = undefined;
  }

  const executablePath = resolveExecutable(context);
  if (!executablePath) { return; }

  session = new AgentSession(executablePath, folder, context);
  await session.start();
}

async function cmdStop(): Promise<void> {
  if (!session) {
    void vscode.window.showInformationMessage('No Mini Agent session is running.');
    return;
  }
  await session.stop();
  session = undefined;
}

async function cmdRestart(context: vscode.ExtensionContext): Promise<void> {
  await cmdStop();
  await cmdStart(context);
}

async function cmdSelectFolder(context: vscode.ExtensionContext): Promise<void> {
  await cmdStart(context);
}

function resolveExecutable(context: vscode.ExtensionContext): string | undefined {
  const cfg = vscode.workspace.getConfiguration('mini-agent');
  // Read from machine scope only; workspace/folder settings cannot override this.
  const configured = cfg.inspect<string>('executablePath');
  const path = configured?.globalValue ?? configured?.defaultValue ?? '';

  if (path) {
    return path;
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
