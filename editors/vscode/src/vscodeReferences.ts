import * as vscode from 'vscode';
import { MAX_REFERENCES, type ReferenceInput } from './references';

interface UriLike { readonly scheme: string; readonly fsPath: string }
interface BinaryLike { readonly mimeType: string; data(): PromiseLike<Uint8Array> }
interface LocationLike {
  readonly uri: UriLike;
  readonly range: { readonly start: { readonly line: number }; readonly end: { readonly line: number } };
}

function isUri(value: unknown): value is UriLike {
  return typeof value === 'object' && value !== null
    && typeof (value as UriLike).scheme === 'string' && typeof (value as UriLike).fsPath === 'string';
}

function isBinary(value: unknown): value is BinaryLike {
  return typeof value === 'object' && value !== null
    && typeof (value as BinaryLike).mimeType === 'string' && typeof (value as BinaryLike).data === 'function';
}

function unsupported(name: string, reason: string): ReferenceInput {
  return { kind: 'unsupported', name, reason };
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
    const display = typeof name === 'string' && name ? name : 'reference';
    if (isLocation(value)) {
      if (value.uri.scheme !== 'file') { out.push(unsupported(display, `only local files can be sent, not ${value.uri.scheme}: references`)); continue; }
      try {
        const document = await vscode.workspace.openTextDocument(value.uri as vscode.Uri);
        out.push({
          kind: 'snippet', fsPath: value.uri.fsPath, text: document.getText(value.range as vscode.Range),
          startLine: value.range.start.line, endLine: value.range.end.line, ...named,
        });
      } catch {
        // A deleted, binary or oversized document must not fail the whole request.
        out.push(unsupported(display, 'the document could not be opened as text'));
      }
    } else if (isUri(value)) {
      if (value.scheme === 'file') { out.push({ kind: 'file', fsPath: value.fsPath, ...named }); }
      else { out.push(unsupported(display, `only local files can be sent, not ${value.scheme}: references`)); }
    } else if (typeof value === 'string') {
      if (value.length > 0) { out.push({ kind: 'text', text: value, ...named }); }
    } else if (isBinary(value)) {
      if (!value.mimeType.startsWith('image/')) { out.push(unsupported(display, `${value.mimeType.slice(0, 60)} attachments are not supported`)); continue; }
      try {
        out.push({ kind: 'image', mimeType: value.mimeType, bytes: await value.data(), ...named });
      } catch {
        out.push(unsupported(display, 'the image data could not be read'));
      }
    } else {
      out.push(unsupported(display, 'this kind of reference is not supported'));
    }
  }
  if (includeSelection) {
    const editor = vscode.window.activeTextEditor;
    if (editor && editor.document.uri.scheme !== 'file' && !editor.selection.isEmpty) {
      out.push(unsupported('selection', `only local files can be sent, not ${editor.document.uri.scheme}: documents`));
    } else if (editor && !editor.selection.isEmpty) {
      out.push({
        kind: 'snippet', fsPath: editor.document.uri.fsPath, text: editor.document.getText(editor.selection),
        startLine: editor.selection.start.line, endLine: editor.selection.end.line,
      });
    }
  }
  return out;
}
