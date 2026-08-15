import * as vscode from 'vscode';
import { log } from './log';

/**
 * Validates workspace trust and folder scheme before any agent operation.
 * Returns the selected folder if all gates pass, or undefined if blocked.
 *
 * Gates (in order):
 * 1. Workspace must be trusted (VS Code Restricted Mode check)
 * 2. Selected folder must use the file: scheme (no virtual workspaces)
 * 3. Executable config must come from user/machine scope only
 */
export async function gatedFolderPick(): Promise<vscode.WorkspaceFolder | undefined> {
  if (!assertTrusted()) { return undefined; }

  const folder = await pickRealFolder();
  if (!folder) { return undefined; }

  return folder;
}

/**
 * Checks that the executable configuration is not being overridden from
 * workspace or folder settings (only user/machine scope is authoritative).
 * Logs a warning if a workspace-scope override is detected.
 */
export function assertExecutableScope(): void {
  const inspect = vscode.workspace.getConfiguration('mini-agent').inspect<string>('executablePath');
  if (inspect?.workspaceValue || inspect?.workspaceFolderValue) {
    log.warn(
      'mini-agent.executablePath is set in workspace or folder settings, which is ignored for security. ' +
      'Move the setting to User Settings or Remote Settings.',
    );
  }
}

/**
 * Revokes the current session when workspace trust is revoked.
 * Call this from activate() to subscribe to trust change events.
 */
export function onTrustRevoked(callback: () => Promise<void>): vscode.Disposable {
  return vscode.workspace.onDidGrantWorkspaceTrust(async () => {
    // onDidGrantWorkspaceTrust fires when trust is granted or revoked.
    // If trust is now false, invoke the revocation callback.
    if (!vscode.workspace.isTrusted) {
      log.warn('Workspace trust revoked — stopping Mini Agent session');
      await callback();
    }
  });
}

function assertTrusted(): boolean {
  if (vscode.workspace.isTrusted) { return true; }

  log.warn('Mini Agent blocked: workspace is not trusted');
  void vscode.window.showErrorMessage(
    'Mini Agent requires a trusted workspace. Open the Trust dialog to grant trust, then try again.',
    'Manage Trust',
  ).then(choice => {
    if (choice === 'Manage Trust') {
      void vscode.commands.executeCommand('workbench.action.manageTrust');
    }
  });
  return false;
}

async function pickRealFolder(): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders;
  if (!folders || folders.length === 0) {
    void vscode.window.showErrorMessage(
      'Mini Agent requires an open workspace folder with a real file system.',
    );
    return undefined;
  }

  // Filter to file-scheme folders only (no vscode-vfs://, vscode-test-web://, etc.)
  const realFolders = folders.filter(f => f.uri.scheme === 'file');
  if (realFolders.length === 0) {
    void vscode.window.showErrorMessage(
      'Mini Agent requires a local file-system workspace. Virtual workspaces are not supported.',
    );
    return undefined;
  }

  if (realFolders.length === 1) {
    return realFolders[0];
  }

  const picked = await vscode.window.showWorkspaceFolderPick({
    placeHolder: 'Select the workspace folder for Mini Agent (one session per selection)',
  });

  if (!picked) { return undefined; }

  // Enforce: must be a real file-scheme folder.
  if (picked.uri.scheme !== 'file') {
    void vscode.window.showErrorMessage(
      `Mini Agent cannot use virtual folder "${picked.name}". Select a local folder instead.`,
    );
    return undefined;
  }

  return picked;
}
