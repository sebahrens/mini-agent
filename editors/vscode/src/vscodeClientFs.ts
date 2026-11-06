import * as vscode from 'vscode';
import { realpath } from 'node:fs/promises';
import type { ClientFsHost } from './clientFs';

/** Whether the user opted in. Machine scope: workspace settings cannot turn it on. */
export function clientFileAccessEnabled(): boolean {
  const inspected = vscode.workspace.getConfiguration('mini-agent').inspect<boolean>('clientFileAccess');
  return inspected?.globalValue === true;
}

/** Reads the live TextDocument (unsaved edits included) and writes through a WorkspaceEdit. */
export const vscodeClientFsHost: ClientFsHost = {
  async readText(fsPath) {
    const document = await vscode.workspace.openTextDocument(vscode.Uri.file(fsPath));
    return document.getText();
  },
  async writeText(fsPath, text) {
    const uri = vscode.Uri.file(fsPath);
    let exists = true;
    try { await vscode.workspace.fs.stat(uri); } catch { exists = false; }
    if (!exists) {
      await vscode.workspace.fs.writeFile(uri, Buffer.from(text, 'utf8'));
      return;
    }
    const document = await vscode.workspace.openTextDocument(uri);
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(0, 0, document.lineCount, 0), text);
    if (!(await vscode.workspace.applyEdit(edit))) { throw new Error('the editor rejected the edit'); }
    if (!(await document.save())) { throw new Error('the editor could not save the file'); }
  },
  realpath: path => realpath(path),
};
