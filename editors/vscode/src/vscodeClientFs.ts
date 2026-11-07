import * as vscode from 'vscode';
import { lstat, realpath } from 'node:fs/promises';
import { ClientFsAppliedUnsaved, ClientFsConflict, sha256Hex, type ClientFsHost } from './clientFs';

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
  async writeText(fsPath, text, expectedSha256) {
    const uri = vscode.Uri.file(fsPath);
    try { await vscode.workspace.fs.stat(uri); } catch { throw new ClientFsConflict(); }
    const document = await vscode.workspace.openTextDocument(uri);
    // The precondition is checked against the live buffer after every await
    // above, and nothing is awaited between this check and the call that
    // hands the edit to the editor, so no extension-host turn can interleave.
    if (sha256Hex(document.getText()) !== expectedSha256) { throw new ClientFsConflict(); }
    const version = document.version;
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, new vscode.Range(0, 0, document.lineCount, 0), text);
    const applying = vscode.workspace.applyEdit(edit);
    if (!(await applying)) { throw new Error('the editor rejected the edit'); }
    // The edit applies as at most one version step (none when the text is
    // identical); more means the user typed in the window between the check
    // and the main thread applying it.
    // The replacement already landed (and can be undone); never save over it.
    if (document.version > version + 1) {
      throw new ClientFsAppliedUnsaved();
    }
    // save() resolves false for a document that is not dirty: an identical write,
    // or auto-save winning the race. That is success; only a still-dirty document failed.
    if (!(await document.save()) && document.isDirty) { throw new Error('the editor could not save the file'); }
  },
  realpath: path => realpath(path),
  isSymlink: async path => {
    try { return (await lstat(path)).isSymbolicLink(); } catch { return false; }
  },
};
