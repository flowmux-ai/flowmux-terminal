// SPDX-License-Identifier: GPL-3.0-or-later

import * as monaco from "monaco-editor/esm/vs/editor/editor.api.js";
import "monaco-editor/esm/vs/basic-languages/monaco.contribution.js";
import "monaco-editor/esm/vs/language/css/monaco.contribution.js";
import "monaco-editor/esm/vs/language/html/monaco.contribution.js";
import "monaco-editor/esm/vs/language/json/monaco.contribution.js";
import "monaco-editor/esm/vs/language/typescript/monaco.contribution.js";
import "./styles.css";

import { languageForPath, languageLabel } from "./language";
import {
  advanceDocumentEdit,
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
}

const tabs = requiredElement("document-tabs");
const workspaceName = requiredElement("workspace-name");
const documentPath = requiredElement("document-path");
const documentState = requiredElement("document-state");
const emptyState = requiredElement("empty-state");
const editorContainer = requiredElement("editor");
const cursorStatus = requiredElement("cursor-status");
const languageStatus = requiredElement("language-status");
const encodingStatus = requiredElement("encoding-status");
const eolStatus = requiredElement("eol-status");

let surfaceId = new URLSearchParams(window.location.search).get("surface") ?? "unbound";
let activeDocumentId: string | null = null;
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

editor.addAction({
  id: "flowmux.save",
  label: "Save",
  keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS],
  run: () => requestSave(),
});

editor.onDidChangeCursorPosition(({ position }) => {
  cursorStatus.textContent = `Ln ${position.lineNumber}, Col ${position.column}`;
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
      workspaceName.textContent = message.workspaceName;
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
        renderChrome();
      }
      break;
    }
    case "save_failed": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.changeSequence === message.changeSequence) {
        document.payload.externalChange = true;
        documentState.textContent = message.reason;
        documentState.className = "document-state is-conflict";
      }
      break;
    }
  }
}

function addOrReplaceDocument(payload: DocumentPayload): void {
  const existing = documents.get(payload.id);
  if (existing !== undefined) {
    existing.suppressChanges = true;
    existing.model.setValue(payload.content);
    monaco.editor.setModelLanguage(existing.model, payload.language ?? languageForPath(payload.relativePath));
    existing.payload = { ...payload };
    existing.suppressChanges = false;
    renderChrome();
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
  };
  model.onDidChangeContent(() => {
    if (document.suppressChanges) {
      return;
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
    renderChrome();
  });
  documents.set(payload.id, document);
  renderChrome();
}

function activateDocument(documentId: string | null): void {
  const document = documentId === null ? undefined : documents.get(documentId);
  activeDocumentId = document?.payload.id ?? null;
  editor.setModel(document?.model ?? null);
  editor.updateOptions({ readOnly: document?.payload.readOnly ?? false });
  renderChrome();
  if (document !== undefined) {
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
  document.model.dispose();
  documents.delete(documentId);
  if (activeDocumentId === documentId) {
    activateDocument(ids[index + 1] ?? ids[index - 1] ?? null);
  } else {
    renderChrome();
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

function requestSave(): void {
  if (activeDocumentId === null) {
    return;
  }
  const document = documents.get(activeDocumentId);
  if (document === undefined || document.payload.readOnly) {
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

function renderChrome(): void {
  tabs.replaceChildren();
  for (const document of documents.values()) {
    const isActive = document.payload.id === activeDocumentId;
    const tab = window.document.createElement("div");
    tab.className = `document-tab${isActive ? " is-active" : ""}${document.payload.dirty ? " is-dirty" : ""}`;
    tab.title = document.payload.relativePath;
    const activate = window.document.createElement("button");
    activate.type = "button";
    activate.className = "tab-activate";
    activate.setAttribute("role", "tab");
    activate.setAttribute("aria-selected", String(isActive));
    activate.addEventListener("click", () => {
      activateDocument(document.payload.id);
      postToHost({
        protocolVersion: PROTOCOL_VERSION,
        surfaceId,
        type: "active_document_changed",
        documentId: document.payload.id,
        documentVersion: document.payload.version,
      });
    });

    const title = window.document.createElement("span");
    title.className = "tab-title";
    title.textContent = document.payload.name;
    const state = window.document.createElement("span");
    state.className = "tab-state";
    state.setAttribute("aria-label", document.payload.dirty ? "Unsaved changes" : "Saved");
    const close = window.document.createElement("button");
    close.type = "button";
    close.className = "tab-close";
    close.textContent = "×";
    close.setAttribute("aria-label", `Close ${document.payload.name}`);
    close.addEventListener("click", (event) => {
      event.stopPropagation();
      requestClose(document);
    });
    activate.append(title, state);
    tab.append(activate, close);
    tabs.append(tab);
  }

  const active = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  emptyState.classList.toggle("is-hidden", active !== undefined);
  editorContainer.classList.toggle("is-visible", active !== undefined);
  if (active === undefined) {
    documentPath.textContent = "No file open";
    documentState.textContent = "Ready";
    documentState.className = "document-state";
    languageStatus.textContent = "Plain text";
    encodingStatus.textContent = "UTF-8";
    eolStatus.textContent = "LF";
    return;
  }

  const language = active.payload.language ?? languageForPath(active.payload.relativePath);
  documentPath.textContent = active.payload.relativePath;
  languageStatus.textContent = languageLabel(language);
  encodingStatus.textContent = active.payload.encoding;
  eolStatus.textContent = active.payload.eol;
  if (active.payload.externalChange) {
    documentState.textContent = "Changed on disk";
    documentState.className = "document-state is-conflict";
  } else if (active.payload.readOnly) {
    documentState.textContent = "Read only";
    documentState.className = "document-state";
  } else if (active.payload.dirty) {
    documentState.textContent = "Unsaved";
    documentState.className = "document-state is-dirty";
  } else {
    documentState.textContent = "Saved";
    documentState.className = "document-state";
  }
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
