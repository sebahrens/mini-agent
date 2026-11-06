import * as vscode from 'vscode';
import { MAX_REFERENCES, type ReferenceInput } from './references';

interface UriLike { readonly scheme: string; readonly fsPath: string }
interface LocationLike {
  readonly uri: UriLike;
  readonly range: { readonly start: { readonly line: number }; readonly end: { readonly line: number } };
}

function isUri(value: unknown): value is UriLike {
  return typeof value === 'object' && value !== null
    && typeof (value as UriLike).scheme === 'string' && typeof (value as UriLike).fsPath === 'string';
}

function isLocation(value: unknown): value is LocationLike {
  return typeof value === 'object' && value !== null
    && isUri((value as LocationLike).uri) && typeof (value as LocationLike).range === 'object';
}

/**
 * Normalises the references the user attached to a chat request (files, ranges,
 * #-variables) and, only when asked, the active editor selection. Only `file:`
 * URIs are accepted; containment in the workspace is enforced by the builder.
 * Ranged text comes from the open TextDocument, so unsaved edits are included.
 */
export async function collectReferences(
  request: Pick<vscode.ChatRequest, 'references'>,
  includeSelection: boolean,
): Promise<ReferenceInput[]> {
  const out: ReferenceInput[] = [];
  for (const reference of request.references.slice(0, MAX_REFERENCES + 1)) {
    const value: unknown = reference.value;
    const name: unknown = (reference as { name?: unknown }).name;
    const named = typeof name === 'string' && name ? { name } : {};
    if (isLocation(value)) {
      if (value.uri.scheme !== 'file') { continue; }
      const document = await vscode.workspace.openTextDocument(value.uri as vscode.Uri);
      out.push({
        kind: 'snippet', fsPath: value.uri.fsPath, text: document.getText(value.range as vscode.Range),
        startLine: value.range.start.line, endLine: value.range.end.line, ...named,
      });
    } else if (isUri(value)) {
      if (value.scheme === 'file') { out.push({ kind: 'file', fsPath: value.fsPath, ...named }); }
    } else if (typeof value === 'string' && value.length > 0) {
      out.push({ kind: 'text', text: value, ...named });
    }
  }
  if (includeSelection) {
    const editor = vscode.window.activeTextEditor;
    if (editor && editor.document.uri.scheme === 'file' && !editor.selection.isEmpty) {
      out.push({
        kind: 'snippet', fsPath: editor.document.uri.fsPath, text: editor.document.getText(editor.selection),
        startLine: editor.selection.start.line, endLine: editor.selection.end.line,
      });
    }
  }
  return out;
}
