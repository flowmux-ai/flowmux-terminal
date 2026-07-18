// SPDX-License-Identifier: GPL-3.0-or-later

import * as monaco from "monaco-editor/esm/vs/editor/editor.api.js";
import "monaco-editor/esm/vs/basic-languages/monaco.contribution.js";
import "monaco-editor/esm/vs/language/css/monaco.contribution.js";
import "monaco-editor/esm/vs/language/html/monaco.contribution.js";
import "monaco-editor/esm/vs/language/json/monaco.contribution.js";
import "monaco-editor/esm/vs/language/typescript/monaco.contribution.js";
import "./styles.css";

import { adjustedFontSize, visibleDocumentState } from "./editor_state";
import { languageForPath } from "./language";
import {
  advanceDocumentEdit,
  type DocumentDiskStatus,
  type DocumentPayload,
  type EditorMessage,
  type HostMessage,
  isHostMessage,
  PROTOCOL_VERSION,
} from "./protocol";

declare global {
  interface Window {
    MonacoEnvironment: monaco.Environment;
    flowmuxEditorHost: { receive: (message: unknown) => void };
    __flowmuxEditorMessages?: EditorMessage[];
    webkit?: {
      messageHandlers?: {
        flowmuxEditor?: { postMessage: (message: string) => void };
      };
    };
  }
}

window.MonacoEnvironment = {
  getWorker(_moduleId: string, label: string): Worker {
    if (label === "json") {
      return new Worker(new URL("./json.worker.js", import.meta.url), { type: "module" });
    }
    if (["css", "scss", "less"].includes(label)) {
      return new Worker(new URL("./css.worker.js", import.meta.url), { type: "module" });
    }
    if (["html", "handlebars", "razor"].includes(label)) {
      return new Worker(new URL("./html.worker.js", import.meta.url), { type: "module" });
    }
    if (["typescript", "javascript"].includes(label)) {
      return new Worker(new URL("./ts.worker.js", import.meta.url), { type: "module" });
    }
    return new Worker(new URL("./editor.worker.js", import.meta.url), { type: "module" });
  },
};

interface OpenDocument {
  payload: DocumentPayload;
  model: monaco.editor.ITextModel;
  changeSequence: number;
  suppressChanges: boolean;
  diskStatus: DocumentDiskStatus;
  restoreViewPending: boolean;
}

interface RecoveryProposal {
  documentId: string;
  documentVersion: number;
  diskState: "unchanged" | "changed" | "deleted";
}

const documentState = requiredElement("document-state");
const emptyState = requiredElement("empty-state");
const editorContainer = requiredElement("editor");
const closeDialog = requiredDialog("close-dialog");
const closeDialogDocument = requiredElement("close-dialog-document");
const closeDialogCancel = requiredButton("close-dialog-cancel");
const closeDialogDiscard = requiredButton("close-dialog-discard");
const closeDialogSave = requiredButton("close-dialog-save");
const recoveryDialog = requiredDialog("recovery-dialog");
const recoveryDialogDocument = requiredElement("recovery-dialog-document");
const recoveryDialogWarning = requiredElement("recovery-dialog-warning");
const recoveryDialogDiscard = requiredButton("recovery-dialog-discard");
const recoveryDialogRestore = requiredButton("recovery-dialog-restore");

let surfaceId = new URLSearchParams(window.location.search).get("surface") ?? "unbound";
let activeDocumentId: string | null = null;
let editorFontSize = 13;
let wordWrapEnabled = false;
let minimapEnabled = false;
let closeDialogDocumentId: string | null = null;
let closeAfterSaveDocumentId: string | null = null;
let recoveryDialogDocumentId: string | null = null;
let recoveryDialogDocumentVersion = 0;
const recoveryQueue: RecoveryProposal[] = [];
let viewStateTimer: ReturnType<typeof setTimeout> | null = null;
const documents = new Map<string, OpenDocument>();

monaco.editor.defineTheme("flowmux-dark", {
  base: "vs-dark",
  inherit: true,
  rules: [],
  colors: {
    "editor.background": "#15171B",
    "editor.foreground": "#E6E9EF",
    "editorCursor.foreground": "#72B7A8",
    "editorLineNumber.foreground": "#5D6470",
    "editorLineNumber.activeForeground": "#B9BEC8",
    "editor.selectionBackground": "#365C59A0",
    "editor.inactiveSelectionBackground": "#2C464580",
    "editorIndentGuide.background1": "#2B2F37",
    "editorIndentGuide.activeBackground1": "#4A525F",
  },
});

const editor = monaco.editor.create(editorContainer, {
  automaticLayout: true,
  theme: "flowmux-dark",
  fontFamily:
    "SFMono-Regular, Cascadia Code, Noto Sans Mono CJK KR, Noto Sans Mono, Apple SD Gothic Neo, Hiragino Sans, monospace",
  fontSize: 13,
  lineHeight: 20,
  minimap: { enabled: false },
  padding: { top: 10, bottom: 10 },
  renderWhitespace: "selection",
  scrollBeyondLastLine: false,
  smoothScrolling: false,
  wordWrap: "off",
});

editor.onDidChangeCursorPosition(() => scheduleViewStateReport());
editor.onDidScrollChange(() => scheduleViewStateReport());

closeDialogCancel.addEventListener("click", () => hideCloseDialog());
closeDialogDiscard.addEventListener("click", () => discardCloseDialogDocument());
closeDialogSave.addEventListener("click", () => saveCloseDialogDocument());
closeDialog.addEventListener("cancel", (event) => {
  event.preventDefault();
  hideCloseDialog();
});
recoveryDialogDiscard.addEventListener("click", () => resolveRecovery("discard"));
recoveryDialogRestore.addEventListener("click", () => resolveRecovery("restore"));
recoveryDialog.addEventListener("cancel", (event) => event.preventDefault());

editor.addAction({
  id: "flowmux.save",
  label: "Save",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS],
  run: () => requestSave(),
});

editor.addAction({
  id: "flowmux.saveAll",
  label: "Save All",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Alt | monaco.KeyCode.KeyS],
  run: () => requestSaveAll(),
});

editor.addAction({
  id: "flowmux.closeDocument",
  label: "Close Current Document",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyW],
  run: () => requestCloseActiveDocument(),
});

editor.addAction({
  id: "flowmux.toggleWordWrap",
  label: "Toggle Word Wrap",
  keybindings: [monaco.KeyMod.Alt | monaco.KeyCode.KeyZ],
  run: () => {
    wordWrapEnabled = !wordWrapEnabled;
    editor.updateOptions({ wordWrap: wordWrapEnabled ? "on" : "off" });
  },
});

editor.addAction({
  id: "flowmux.toggleMinimap",
  label: "Toggle Minimap",
  run: () => {
    minimapEnabled = !minimapEnabled;
    editor.updateOptions({ minimap: { enabled: minimapEnabled } });
  },
});

editor.addAction({
  id: "flowmux.increaseFontSize",
  label: "Increase Editor Font Size",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Equal],
  run: () => setEditorFontSize(adjustedFontSize(editorFontSize, 1)),
});

editor.addAction({
  id: "flowmux.decreaseFontSize",
  label: "Decrease Editor Font Size",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Minus],
  run: () => setEditorFontSize(adjustedFontSize(editorFontSize, -1)),
});

editor.addAction({
  id: "flowmux.resetFontSize",
  label: "Reset Editor Font Size",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Digit0],
  run: () => setEditorFontSize(13),
});

window.flowmuxEditorHost = {
  receive(message: unknown): void {
    if (!isHostMessage(message)) {
      return;
    }
    handleHostMessage(message);
  },
};

postToHost({ protocolVersion: PROTOCOL_VERSION, surfaceId, type: "editor_ready" });

function handleHostMessage(message: HostMessage): void {
  if (message.surfaceId !== surfaceId && surfaceId !== "unbound") {
    return;
  }
  surfaceId = message.surfaceId;

  switch (message.type) {
    case "initialize_editor":
      clearViewStateTimer();
      resetCloseDialog();
      resetRecoveryDialog(true);
      for (const document of [...documents.values()]) {
        document.model.dispose();
      }
      documents.clear();
      for (const document of message.documents) {
        addOrReplaceDocument(document);
      }
      activateDocument(message.activeDocumentId);
      break;
    case "open_document":
    case "replace_document":
      addOrReplaceDocument(message.document);
      activateDocument(message.document.id);
      break;
    case "close_document":
      closeDocument(message.documentId);
      break;
    case "set_active_document":
      activateDocument(message.documentId);
      break;
    case "save_completed": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.changeSequence === message.changeSequence) {
        document.payload.version = message.documentVersion;
        document.payload.dirty = false;
        document.payload.externalChange = false;
        document.diskStatus = "unchanged";
        renderState();
        if (closeAfterSaveDocumentId === document.payload.id) {
          closeAfterSaveDocumentId = null;
          requestClose(document);
        }
      }
      break;
    }
    case "save_failed": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.changeSequence === message.changeSequence) {
        if (closeAfterSaveDocumentId === document.payload.id) {
          closeAfterSaveDocumentId = null;
        }
        document.payload.externalChange = true;
        document.diskStatus = "modified";
        documentState.textContent = message.reason;
        documentState.className = "document-state is-conflict";
        documentState.hidden = false;
      }
      break;
    }
    case "document_disk_status": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.payload.version === message.documentVersion) {
        document.diskStatus = message.status;
        document.payload.externalChange = message.status !== "unchanged";
        renderState();
      }
      break;
    }
    case "recovery_available":
      showRecoveryDialog(message.documentId, message.documentVersion, message.diskState);
      break;
  }
}

function addOrReplaceDocument(payload: DocumentPayload): void {
  const existing = documents.get(payload.id);
  if (existing !== undefined) {
    const viewState = activeDocumentId === payload.id ? editor.saveViewState() : null;
    existing.suppressChanges = true;
    existing.model.setValue(payload.content);
    monaco.editor.setModelLanguage(existing.model, payload.language ?? languageForPath(payload.relativePath));
    existing.payload = { ...payload };
    existing.diskStatus = payload.externalChange ? "modified" : "unchanged";
    existing.suppressChanges = false;
    if (viewState !== null) {
      editor.restoreViewState(viewState);
    }
    renderState();
    return;
  }

  const model = monaco.editor.createModel(
    payload.content,
    payload.language ?? languageForPath(payload.relativePath),
    monaco.Uri.parse(payload.uri),
  );
  const document: OpenDocument = {
    payload: { ...payload },
    model,
    changeSequence: 0,
    suppressChanges: false,
    diskStatus: payload.externalChange ? "modified" : "unchanged",
    restoreViewPending: true,
  };
  model.onDidChangeContent(() => {
    if (document.suppressChanges) {
      return;
    }
    if (closeAfterSaveDocumentId === document.payload.id) {
      closeAfterSaveDocumentId = null;
    }
    document.payload.dirty = true;
    const edit = advanceDocumentEdit(document.payload.version, document.changeSequence);
    document.payload.version = edit.nextVersion;
    document.changeSequence = edit.changeSequence;
    postToHost({
      protocolVersion: PROTOCOL_VERSION,
      surfaceId,
      type: "document_changed",
      documentId: document.payload.id,
      documentVersion: edit.baseVersion,
      changeSequence: edit.changeSequence,
      content: model.getValue(),
    });
    renderState();
  });
  documents.set(payload.id, document);
  renderState();
}

function activateDocument(documentId: string | null): void {
  const previousDocumentId = activeDocumentId;
  if (previousDocumentId !== documentId) {
    reportActiveViewState();
  }
  const document = documentId === null ? undefined : documents.get(documentId);
  activeDocumentId = document?.payload.id ?? null;
  editor.setModel(document?.model ?? null);
  editor.updateOptions({ readOnly: document?.payload.readOnly ?? false });
  renderState();
  if (document !== undefined) {
    if (document.restoreViewPending) {
      editor.setPosition({
        lineNumber: document.payload.cursorLine + 1,
        column: document.payload.cursorColumn + 1,
      });
      editor.setScrollTop(document.payload.scrollTop);
      document.restoreViewPending = false;
    }
    if (previousDocumentId !== document.payload.id) {
      postToHost({
        protocolVersion: PROTOCOL_VERSION,
        surfaceId,
        type: "active_document_changed",
        documentId: document.payload.id,
        documentVersion: document.payload.version,
      });
    }
    editor.focus();
  }
}

function closeDocument(documentId: string): void {
  const document = documents.get(documentId);
  if (document === undefined) {
    return;
  }
  const ids = [...documents.keys()];
  const index = ids.indexOf(documentId);
  if (closeDialogDocumentId === documentId || closeAfterSaveDocumentId === documentId) {
    resetCloseDialog();
  }
  if (recoveryDialogDocumentId === documentId) {
    resetRecoveryDialog();
  }
  for (let index = recoveryQueue.length - 1; index >= 0; index -= 1) {
    if (recoveryQueue[index]?.documentId === documentId) {
      recoveryQueue.splice(index, 1);
    }
  }
  document.model.dispose();
  documents.delete(documentId);
  if (activeDocumentId === documentId) {
    activateDocument(ids[index + 1] ?? ids[index - 1] ?? null);
  } else {
    renderState();
  }
}

function requestClose(document: OpenDocument): void {
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "close_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
    dirty: document.payload.dirty,
  });
}

function requestCloseActiveDocument(): void {
  const document = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  if (document === undefined) {
    return;
  }
  if (document.payload.dirty) {
    showCloseDialog(document);
  } else {
    requestClose(document);
  }
}

function showCloseDialog(document: OpenDocument): void {
  closeDialogDocumentId = document.payload.id;
  closeDialogDocument.textContent = `“${document.payload.name}”`;
  closeDialogSave.disabled = document.payload.readOnly;
  if (!closeDialog.open) {
    closeDialog.showModal();
  }
  (closeDialogSave.disabled ? closeDialogDiscard : closeDialogSave).focus();
}

function hideCloseDialog(): void {
  closeDialogDocumentId = null;
  if (closeDialog.open) {
    closeDialog.close();
  }
  editor.focus();
}

function resetCloseDialog(): void {
  closeAfterSaveDocumentId = null;
  hideCloseDialog();
}

function saveCloseDialogDocument(): void {
  const document =
    closeDialogDocumentId === null ? undefined : documents.get(closeDialogDocumentId);
  if (document === undefined || document.payload.readOnly) {
    return;
  }
  closeAfterSaveDocumentId = document.payload.id;
  hideCloseDialog();
  requestSave(document.payload.id);
}

function discardCloseDialogDocument(): void {
  const document =
    closeDialogDocumentId === null ? undefined : documents.get(closeDialogDocumentId);
  if (document === undefined) {
    hideCloseDialog();
    return;
  }
  hideCloseDialog();
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "discard_close_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
  });
}

function showRecoveryDialog(
  documentId: string,
  documentVersion: number,
  diskState: "unchanged" | "changed" | "deleted",
): void {
  const document = documents.get(documentId);
  if (document === undefined || document.payload.version !== documentVersion) {
    return;
  }
  if (recoveryDialogDocumentId !== null) {
    const duplicate = recoveryQueue.some((proposal) => proposal.documentId === documentId);
    if (recoveryDialogDocumentId !== documentId && !duplicate) {
      recoveryQueue.push({ documentId, documentVersion, diskState });
    }
    return;
  }
  recoveryDialogDocumentId = documentId;
  recoveryDialogDocumentVersion = documentVersion;
  recoveryDialogDocument.textContent = `“${document.payload.name}”`;
  recoveryDialogWarning.textContent =
    diskState === "unchanged"
      ? ""
      : "The file also changed on disk, so restoring it will require resolving a conflict before saving.";
  if (!recoveryDialog.open) {
    recoveryDialog.showModal();
  }
  recoveryDialogRestore.focus();
}

function resolveRecovery(choice: "restore" | "discard"): void {
  if (recoveryDialogDocumentId === null) {
    return;
  }
  const documentId = recoveryDialogDocumentId;
  const documentVersion = recoveryDialogDocumentVersion;
  resetRecoveryDialog();
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "recovery_decision",
    documentId,
    documentVersion,
    choice,
  });
}

function resetRecoveryDialog(clearQueue = false): void {
  recoveryDialogDocumentId = null;
  recoveryDialogDocumentVersion = 0;
  if (recoveryDialog.open) {
    recoveryDialog.close();
  }
  if (clearQueue) {
    recoveryQueue.length = 0;
  } else {
    const next = recoveryQueue.shift();
    if (next !== undefined) {
      showRecoveryDialog(next.documentId, next.documentVersion, next.diskState);
      return;
    }
  }
  editor.focus();
}

function scheduleViewStateReport(): void {
  if (activeDocumentId === null) {
    return;
  }
  clearViewStateTimer();
  viewStateTimer = setTimeout(() => reportActiveViewState(), 300);
}

function clearViewStateTimer(): void {
  if (viewStateTimer !== null) {
    clearTimeout(viewStateTimer);
    viewStateTimer = null;
  }
}

function reportActiveViewState(): void {
  clearViewStateTimer();
  const document = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  const position = editor.getPosition();
  if (document === undefined || position === null) {
    return;
  }
  document.payload.cursorLine = Math.max(0, position.lineNumber - 1);
  document.payload.cursorColumn = Math.max(0, position.column - 1);
  document.payload.scrollTop = Math.max(0, editor.getScrollTop());
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "view_state_changed",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
    cursorLine: document.payload.cursorLine,
    cursorColumn: document.payload.cursorColumn,
    scrollTop: document.payload.scrollTop,
  });
}

function requestSave(documentId: string | null = activeDocumentId): void {
  const document = documentId === null ? undefined : documents.get(documentId);
  if (document === undefined || document.payload.readOnly || !document.payload.dirty) {
    return;
  }
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "save_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
    changeSequence: document.changeSequence,
    content: document.model.getValue(),
  });
}

function requestSaveAll(): void {
  for (const document of documents.values()) {
    if (document.payload.dirty && !document.payload.readOnly) {
      requestSave(document.payload.id);
    }
  }
}

function setEditorFontSize(fontSize: number): void {
  editorFontSize = fontSize;
  editor.updateOptions({ fontSize });
}

function renderState(): void {
  const active = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  emptyState.classList.toggle("is-hidden", active !== undefined);
  editorContainer.classList.toggle("is-visible", active !== undefined);
  if (active === undefined) {
    documentState.className = "document-state";
    documentState.hidden = true;
    return;
  }

  const state = visibleDocumentState(
    active.diskStatus,
    active.payload.readOnly,
    active.payload.dirty,
  );
  documentState.textContent = state.text;
  documentState.className = `document-state${state.kind === "normal" ? "" : ` is-${state.kind}`}`;
  documentState.hidden = state.hidden;
}

function postToHost(message: EditorMessage): void {
  const handler = window.webkit?.messageHandlers?.flowmuxEditor;
  if (handler !== undefined) {
    handler.postMessage(JSON.stringify(message));
    return;
  }
  window.__flowmuxEditorMessages ??= [];
  window.__flowmuxEditorMessages.push(message);
}

function requiredElement(id: string): HTMLElement {
  const element = document.getElementById(id);
  if (element === null) {
    throw new Error(`Missing required editor element: ${id}`);
  }
  return element;
}

function requiredDialog(id: string): HTMLDialogElement {
  const element = requiredElement(id);
  if (!(element instanceof HTMLDialogElement)) {
    throw new Error(`Editor element is not a dialog: ${id}`);
  }
  return element;
}

function requiredButton(id: string): HTMLButtonElement {
  const element = requiredElement(id);
  if (!(element instanceof HTMLButtonElement)) {
    throw new Error(`Editor element is not a button: ${id}`);
  }
  return element;
}
