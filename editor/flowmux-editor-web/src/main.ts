// SPDX-License-Identifier: GPL-3.0-or-later

import "monaco-editor/esm/vs/editor/editor.all.js";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api.js";
import "monaco-editor/esm/vs/basic-languages/monaco.contribution.js";
import "monaco-editor/esm/vs/language/css/monaco.contribution.js";
import "monaco-editor/esm/vs/language/html/monaco.contribution.js";
import "monaco-editor/esm/vs/language/json/monaco.contribution.js";
import "monaco-editor/esm/vs/language/typescript/monaco.contribution.js";
import "./styles.css";

import {
  EDITOR_ACTION_SPECS,
  monacoKeybinding,
  type EditorActionRunner,
} from "./editor_actions";
import { adjustedFontSize, conflictUiState, visibleDocumentState } from "./editor_state";
import { languageForPath } from "./language";
import { commaSeparatedGlobs, rankQuickOpen } from "./search_state";
import {
  advanceDocumentEdit,
  type DocumentDiskStatus,
  type DocumentPayload,
  type EditorAppearance,
  type EditorMessage,
  type HostMessage,
  type WorkspaceSearchMatch,
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
  /** Last save failure that was not a disk conflict; cleared on edit or save. */
  saveError: string | null;
  /** Content edits not yet sent to the host (throttled, see syncDocument). */
  pendingChanges: boolean;
  changeTimer: ReturnType<typeof setTimeout> | null;
}

/**
 * Content sync is throttled: the first change in a burst arms the timer, and
 * everything typed inside the window rides along in one message. The full
 * document crosses the bridge per message, so per-keystroke sends make large
 * files janky. Any message that carries a document version must call
 * syncDocument() first — the host rejects version gaps.
 */
const CHANGE_SYNC_DELAY_MS = 150;

interface RecoveryProposal {
  documentId: string;
  documentVersion: number;
  diskState: "unchanged" | "changed" | "deleted";
}

const documentState = requiredElement("document-state");
const emptyState = requiredElement("empty-state");
const editorContainer = requiredElement("editor");
const diffEditorContainer = requiredElement("diff-editor");
const modeSwitch = requiredElement("mode-switch");
const modeEdit = requiredButton("mode-edit");
const modeDiff = requiredButton("mode-diff");
const conflictBanner = requiredElement("conflict-banner");
const conflictMessage = requiredElement("conflict-message");
const conflictCompare = requiredButton("conflict-compare");
const conflictKeep = requiredButton("conflict-keep");
const conflictReload = requiredButton("conflict-reload");
const conflictSaveAs = requiredButton("conflict-save-as");
const conflictCloseDiff = requiredButton("conflict-close-diff");
const searchDialog = requiredDialog("search-dialog");
const searchDialogTitle = requiredElement("search-dialog-title");
const searchDialogClose = requiredButton("search-dialog-close");
const searchQuery = requiredInput("search-query");
const searchOptions = requiredElement("search-options");
const searchCase = requiredButton("search-case");
const searchWord = requiredButton("search-word");
const searchRegex = requiredButton("search-regex");
const searchInclude = requiredInput("search-include");
const searchExclude = requiredInput("search-exclude");
const searchStatus = requiredElement("search-status");
const searchResults = requiredElement("search-results");
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
const saveAsDialog = requiredDialog("save-as-dialog");
const saveAsPath = requiredInput("save-as-path");
const saveAsError = requiredElement("save-as-error");
const saveAsCancel = requiredButton("save-as-cancel");
const saveAsSubmit = requiredButton("save-as-submit");

let surfaceId = new URLSearchParams(window.location.search).get("surface") ?? "unbound";
let activeDocumentId: string | null = null;
let editorFontSize = 13;
let wordWrapEnabled = false;
let minimapEnabled = false;
let closeDialogDocumentId: string | null = null;
let closeAfterSaveDocumentId: string | null = null;
let saveAsDocumentId: string | null = null;
let saveAsOverwrite = false;
let diffDocumentId: string | null = null;
let diffOriginalModel: monaco.editor.ITextModel | null = null;
let diffEditor: monaco.editor.IStandaloneDiffEditor | null = null;
let recoveryDialogDocumentId: string | null = null;
let recoveryDialogDocumentVersion = 0;
const recoveryQueue: RecoveryProposal[] = [];
let viewStateTimer: ReturnType<typeof setTimeout> | null = null;
let searchTimer: ReturnType<typeof setTimeout> | null = null;
let searchMode: "quick" | "workspace" = "quick";
let searchRequestCounter = 0;
let activeSearchRequestId: string | null = null;
let quickOpenPaths: string[] = [];
let quickOpenTruncated = false;
let selectedSearchResult = 0;
let renderedSearchResults: SearchSelection[] = [];
const recentPaths: string[] = [];
const documents = new Map<string, OpenDocument>();

interface SearchSelection {
  path: string;
  line: number;
  column: number;
  length: number;
}

const DEFAULT_APPEARANCE: EditorAppearance = {
  dark: true,
  background: "#15171bff",
  foreground: "#e6e9efff",
  cursor: "#72b7a8ff",
  selectionBackground: "#365c59a0",
  selectionForeground: "#e6e9efff",
  fontFamily: "monospace",
  fontSize: 13,
};
const EDITOR_THEME = "flowmux-editor";
const EDITOR_FONT_FALLBACKS =
  'SFMono-Regular, "Cascadia Code", "Noto Sans Mono CJK KR", "Noto Sans Mono", "Apple SD Gothic Neo", "Hiragino Sans", monospace';
let appliedAppearance = DEFAULT_APPEARANCE;
defineEditorTheme(DEFAULT_APPEARANCE);

const editor = monaco.editor.create(editorContainer, {
  automaticLayout: true,
  theme: EDITOR_THEME,
  fontFamily: editorFontFamily(DEFAULT_APPEARANCE.fontFamily),
  fontSize: 13,
  lineNumbers: "on",
  cursorBlinking: "blink",
  cursorSmoothCaretAnimation: "off",
  minimap: { enabled: false },
  padding: { top: 10, bottom: 10 },
  renderLineHighlight: "line",
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
conflictCompare.addEventListener("click", () => requestConflictAction("compare"));
conflictKeep.addEventListener("click", () => requestConflictAction("keep_mine"));
conflictReload.addEventListener("click", () => requestConflictAction("reload_from_disk"));
conflictSaveAs.addEventListener("click", () => showSaveAsDialog());
conflictCloseDiff.addEventListener("click", () => closeDiffView());
modeEdit.addEventListener("click", () => closeDiffView());
modeDiff.addEventListener("click", () => requestDiffView());
saveAsCancel.addEventListener("click", () => closeSaveAsDialog());
saveAsSubmit.addEventListener("click", () => submitSaveAs());
saveAsPath.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    submitSaveAs();
  }
});
saveAsDialog.addEventListener("cancel", (event) => {
  event.preventDefault();
  closeSaveAsDialog();
});
searchDialogClose.addEventListener("click", () => closeSearchDialog());
searchDialog.addEventListener("cancel", (event) => {
  event.preventDefault();
  closeSearchDialog();
});
searchQuery.addEventListener("input", () => searchInputChanged());
searchQuery.addEventListener("keydown", (event) => searchKeyDown(event));
for (const button of [searchCase, searchWord, searchRegex]) {
  button.addEventListener("click", () => {
    button.setAttribute("aria-pressed", String(button.getAttribute("aria-pressed") !== "true"));
    scheduleWorkspaceSearch();
  });
}
for (const input of [searchInclude, searchExclude]) {
  input.addEventListener("input", () => scheduleWorkspaceSearch());
}

// One handler per runner name in the shared action table. `target` is the
// editor the shortcut fired in, so Find/Replace open their widget inside the
// diff editor's modified pane as well as the main editor.
const editorActionRunners: Record<
  EditorActionRunner,
  (target: monaco.editor.ICodeEditor) => void
> = {
  save: () => requestSave(),
  saveAs: () => showSaveAsDialog(),
  saveAll: () => requestSaveAll(),
  find: (target) => {
    void target.getAction("actions.find")?.run();
  },
  replace: (target) => {
    // Monaco disables the replace action on read-only editors; falling back
    // to plain find keeps Ctrl+H responsive there.
    const replace = target.getAction("editor.action.startFindReplaceAction");
    if (replace !== null && target.getOption(monaco.editor.EditorOption.readOnly) === false) {
      void replace.run();
    } else {
      void target.getAction("actions.find")?.run();
    }
  },
  quickOpen: () => showQuickOpen(),
  workspaceSearch: () => showWorkspaceSearch(),
  closeDocument: () => requestCloseActiveDocument(),
  toggleWordWrap: () => {
    wordWrapEnabled = !wordWrapEnabled;
    const wordWrap = wordWrapEnabled ? "on" : "off";
    editor.updateOptions({ wordWrap });
    diffEditor?.updateOptions({ wordWrap });
  },
  toggleMinimap: () => {
    minimapEnabled = !minimapEnabled;
    editor.updateOptions({ minimap: { enabled: minimapEnabled } });
  },
  increaseFontSize: () => setEditorFontSize(adjustedFontSize(editorFontSize, 1)),
  decreaseFontSize: () => setEditorFontSize(adjustedFontSize(editorFontSize, -1)),
  resetFontSize: () => setEditorFontSize(13),
};

// Registered on the main editor and on the diff editor's modified pane so
// every shortcut keeps working in Diff mode. The chord table lives in
// editor_actions.ts where the Node test suite pins the assignments
// (Ctrl+F find, Ctrl+H replace, Ctrl+S save on Linux, Cmd on macOS).
const sharedEditorActions: monaco.editor.IActionDescriptor[] = EDITOR_ACTION_SPECS.map(
  (spec) => ({
    id: spec.id,
    label: spec.label,
    keybindings:
      spec.chord === null
        ? undefined
        : [
            monacoKeybinding(
              spec.chord,
              monaco.KeyMod,
              monaco.KeyCode as unknown as Record<string, number>,
            ),
          ],
    run: (target) => editorActionRunners[spec.run](target),
  }),
);

for (const action of sharedEditorActions) {
  editor.addAction(action);
}

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
    case "set_appearance":
      applyAppearance(message.appearance);
      break;
    case "initialize_editor":
      clearViewStateTimer();
      resetCloseDialog();
      resetRecoveryDialog(true);
      closeSaveAsDialog(false);
      closeDiffView(false);
      for (const document of [...documents.values()]) {
        clearChangeTimer(document);
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
        document.saveError = null;
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
        if (message.conflict) {
          document.payload.externalChange = true;
          document.diskStatus = "modified";
        } else {
          // A permissions or I/O failure is not a conflict; offering
          // "Reload from disk" here would throw the user's edits away.
          document.saveError = message.reason;
        }
        renderState();
        if (message.conflict && message.documentId === activeDocumentId) {
          conflictMessage.textContent = message.reason;
        }
      }
      break;
    }
    case "save_as_completed": {
      const document = documents.get(message.document.id);
      if (document !== undefined) {
        if (document.changeSequence === message.changeSequence) {
          addOrReplaceDocument(message.document);
        } else {
          document.payload = {
            ...document.payload,
            relativePath: message.document.relativePath,
            name: message.document.name,
            encoding: message.document.encoding,
            eol: message.document.eol,
            readOnly: message.document.readOnly,
            externalChange: false,
          };
          document.diskStatus = "unchanged";
          monaco.editor.setModelLanguage(
            document.model,
            message.document.language ?? languageForPath(message.document.relativePath),
          );
          renderState();
        }
      }
      closeSaveAsDialog();
      break;
    }
    case "save_as_failed": {
      const document = documents.get(message.documentId);
      if (document !== undefined && saveAsDocumentId === message.documentId) {
        saveAsOverwrite = message.targetExists;
        saveAsSubmit.textContent = message.targetExists ? "Replace" : "Save";
        saveAsError.textContent = message.targetExists
          ? "That file already exists. Choose Replace to overwrite it."
          : message.reason;
        saveAsPath.focus();
      }
      break;
    }
    case "document_disk_status": {
      // No version guard: disk status is not version-sensitive, and the host
      // only reports each transition once. Matching on the version would drop
      // the notification whenever an edit is in flight, hiding the conflict
      // until a save fails.
      const document = documents.get(message.documentId);
      if (document !== undefined) {
        document.diskStatus = message.status;
        document.payload.externalChange = message.status !== "unchanged";
        renderState();
      }
      break;
    }
    case "recovery_available":
      showRecoveryDialog(message.documentId, message.documentVersion, message.diskState);
      break;
    case "show_diff": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.payload.version === message.documentVersion) {
        showDiff(document, message.diskContent);
      }
      break;
    }
    case "conflict_action_failed": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.payload.version === message.documentVersion) {
        conflictMessage.textContent = message.reason;
        conflictBanner.hidden = false;
      }
      break;
    }
    case "show_workspace_search":
      showWorkspaceSearch();
      break;
    case "quick_open_completed":
      if (message.requestId === activeSearchRequestId && searchMode === "quick") {
        activeSearchRequestId = null;
        quickOpenPaths = message.paths;
        quickOpenTruncated = message.truncated;
        renderQuickOpen();
      }
      break;
    case "workspace_search_completed":
      if (message.requestId === activeSearchRequestId && searchMode === "workspace") {
        activeSearchRequestId = null;
        renderWorkspaceSearch(message.result.matches, message.result.truncated, message.error);
      }
      break;
    case "reveal_range": {
      const document = documents.get(message.documentId);
      if (document !== undefined && document.payload.version === message.documentVersion) {
        activateDocument(message.documentId);
        const lineNumber = message.line + 1;
        const startColumn = message.column + 1;
        const range = new monaco.Range(
          lineNumber,
          startColumn,
          lineNumber,
          startColumn + message.length,
        );
        editor.setSelection(range);
        editor.revealRangeInCenter(range);
        editor.focus();
      }
      break;
    }
  }
}

function addOrReplaceDocument(payload: DocumentPayload): void {
  const existing = documents.get(payload.id);
  if (existing !== undefined) {
    if (diffDocumentId === payload.id) {
      closeDiffView(false);
    }
    // The host's replacement supersedes anything typed but not yet synced.
    clearChangeTimer(existing);
    existing.pendingChanges = false;
    const viewState = activeDocumentId === payload.id ? editor.saveViewState() : null;
    existing.suppressChanges = true;
    if (existing.model.getValue() !== payload.content) {
      // pushEditOperations instead of setValue so an external reload (e.g.
      // `git checkout`) does not destroy the undo history.
      existing.model.pushEditOperations(
        [],
        [{ range: existing.model.getFullModelRange(), text: payload.content }],
        () => null,
      );
      existing.model.pushStackElement();
    }
    monaco.editor.setModelLanguage(existing.model, payload.language ?? languageForPath(payload.relativePath));
    existing.payload = { ...payload };
    existing.diskStatus = payload.externalChange ? "modified" : "unchanged";
    existing.saveError = null;
    existing.suppressChanges = false;
    if (viewState !== null) {
      editor.restoreViewState(viewState);
    } else if (activeDocumentId !== payload.id) {
      // A background document replaced by the host should reopen at the
      // cursor position the payload carries, not at line 1.
      existing.restoreViewPending = true;
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
    saveError: null,
    pendingChanges: false,
    changeTimer: null,
  };
  model.onDidChangeContent(() => {
    if (document.suppressChanges) {
      return;
    }
    if (closeAfterSaveDocumentId === document.payload.id) {
      closeAfterSaveDocumentId = null;
    }
    document.payload.dirty = true;
    document.saveError = null;
    document.pendingChanges = true;
    if (document.changeTimer === null) {
      document.changeTimer = setTimeout(() => syncDocument(document), CHANGE_SYNC_DELAY_MS);
    }
    renderState();
  });
  documents.set(payload.id, document);
  renderState();
}

function clearChangeTimer(document: OpenDocument): void {
  if (document.changeTimer !== null) {
    clearTimeout(document.changeTimer);
    document.changeTimer = null;
  }
}

/** Flush throttled edits so the host's document version catches up. */
function syncDocument(document: OpenDocument): void {
  clearChangeTimer(document);
  if (!document.pendingChanges) {
    return;
  }
  document.pendingChanges = false;
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
    content: document.model.getValue(),
  });
}

function activateDocument(documentId: string | null): void {
  const previousDocumentId = activeDocumentId;
  if (previousDocumentId !== documentId) {
    reportActiveViewState();
  }
  const document = documentId === null ? undefined : documents.get(documentId);
  if (diffDocumentId !== null && diffDocumentId !== document?.payload.id) {
    closeDiffView(false);
  }
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
      syncDocument(document);
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
  if (saveAsDocumentId === documentId) {
    closeSaveAsDialog();
  }
  if (diffDocumentId === documentId) {
    closeDiffView(false);
  }
  for (let index = recoveryQueue.length - 1; index >= 0; index -= 1) {
    if (recoveryQueue[index]?.documentId === documentId) {
      recoveryQueue.splice(index, 1);
    }
  }
  clearChangeTimer(document);
  document.model.dispose();
  documents.delete(documentId);
  if (activeDocumentId === documentId) {
    activateDocument(ids[index + 1] ?? ids[index - 1] ?? null);
  } else {
    renderState();
  }
}

function requestClose(document: OpenDocument): void {
  syncDocument(document);
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
  syncDocument(document);
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

function requestConflictAction(action: "compare" | "keep_mine" | "reload_from_disk"): void {
  const document = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  if (document === undefined || !document.payload.externalChange) {
    return;
  }
  syncDocument(document);
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "conflict_action_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
    action,
  });
}

function requestDiffView(): void {
  const document = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  if (document === undefined || diffDocumentId === document.payload.id) {
    return;
  }
  syncDocument(document);
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "diff_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
  });
}

function showDiff(document: OpenDocument, diskContent: string): void {
  closeDiffView(false);
  diffOriginalModel = monaco.editor.createModel(
    diskContent,
    document.payload.language ?? languageForPath(document.payload.relativePath),
    monaco.Uri.parse(
      `flowmux-disk://comparison/${encodeURIComponent(document.payload.id)}/${document.payload.version}`,
    ),
  );
  if (diffEditor === null) {
    diffEditor = monaco.editor.createDiffEditor(diffEditorContainer, {
      automaticLayout: true,
      theme: EDITOR_THEME,
      fontFamily: editorFontFamily(appliedAppearance.fontFamily),
      fontSize: editorFontSize,
      lineNumbers: "on",
      cursorBlinking: "blink",
      cursorSmoothCaretAnimation: "off",
      minimap: { enabled: false },
      renderLineHighlight: "line",
      renderSideBySide: true,
      scrollBeyondLastLine: false,
      originalEditable: false,
      wordWrap: wordWrapEnabled ? "on" : "off",
    });
    const modified = diffEditor.getModifiedEditor();
    for (const action of sharedEditorActions) {
      modified.addAction(action);
    }
    modified.addAction({
      id: "flowmux.diff.close",
      label: "Close Diff",
      keybindings: [monaco.KeyCode.Escape],
      precondition: "!findWidgetVisible",
      run: () => closeDiffView(),
    });
  }
  diffEditor.getModifiedEditor().updateOptions({ readOnly: document.payload.readOnly });
  diffEditor.setModel({ original: diffOriginalModel, modified: document.model });
  diffDocumentId = document.payload.id;
  renderState();
  diffEditor.focus();
}

function closeDiffView(render = true): void {
  diffEditor?.setModel(null);
  diffOriginalModel?.dispose();
  diffOriginalModel = null;
  diffDocumentId = null;
  if (render) {
    renderState();
    editor.focus();
  }
}

function showSaveAsDialog(documentId: string | null = activeDocumentId): void {
  const document = documentId === null ? undefined : documents.get(documentId);
  if (document === undefined) {
    return;
  }
  if (searchDialog.open) {
    closeSearchDialog();
  }
  saveAsDocumentId = document.payload.id;
  saveAsOverwrite = false;
  saveAsPath.value = document.payload.relativePath;
  saveAsError.textContent = "";
  saveAsSubmit.textContent = "Save";
  if (!saveAsDialog.open) {
    saveAsDialog.showModal();
  }
  saveAsPath.focus();
  saveAsPath.select();
}

function submitSaveAs(): void {
  const document = saveAsDocumentId === null ? undefined : documents.get(saveAsDocumentId);
  const path = saveAsPath.value.trim();
  if (document === undefined) {
    closeSaveAsDialog();
    return;
  }
  if (path.length === 0) {
    saveAsError.textContent = "Enter a workspace-relative file path.";
    return;
  }
  saveAsError.textContent = "Saving…";
  syncDocument(document);
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "save_as_requested",
    documentId: document.payload.id,
    documentVersion: document.payload.version,
    changeSequence: document.changeSequence,
    content: document.model.getValue(),
    path,
    overwrite: saveAsOverwrite,
  });
}

function closeSaveAsDialog(focus = true): void {
  saveAsDocumentId = null;
  saveAsOverwrite = false;
  saveAsError.textContent = "";
  saveAsSubmit.textContent = "Save";
  if (saveAsDialog.open) {
    saveAsDialog.close();
  }
  if (focus) {
    editor.focus();
  }
}

function showQuickOpen(): void {
  if (!canShowSearchDialog()) {
    return;
  }
  searchMode = "quick";
  searchDialogTitle.textContent = "Open File";
  searchQuery.placeholder = "Type a file name…";
  searchOptions.hidden = true;
  searchQuery.value = "";
  quickOpenPaths = [];
  quickOpenTruncated = false;
  clearSearchResults("Loading files…");
  openSearchDialog();
  const requestId = nextSearchRequestId();
  activeSearchRequestId = requestId;
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "quick_open_requested",
    requestId,
  });
}

function showWorkspaceSearch(): void {
  if (!canShowSearchDialog()) {
    return;
  }
  searchMode = "workspace";
  searchDialogTitle.textContent = "Find in Workspace";
  searchQuery.placeholder = "Search files…";
  searchOptions.hidden = false;
  searchQuery.value = "";
  clearSearchResults("Type to search the workspace.");
  openSearchDialog();
}

function canShowSearchDialog(): boolean {
  return !closeDialog.open && !recoveryDialog.open && !saveAsDialog.open;
}

function openSearchDialog(): void {
  cancelActiveSearch();
  if (!searchDialog.open) {
    searchDialog.showModal();
  }
  searchQuery.focus();
  searchQuery.select();
}

function closeSearchDialog(): void {
  cancelActiveSearch();
  clearSearchTimer();
  if (searchDialog.open) {
    searchDialog.close();
  }
  editor.focus();
}

function searchInputChanged(): void {
  if (searchMode === "quick") {
    // The file index is still loading; rendering now would flash
    // "0 matching files". The completion handler re-renders with this query.
    if (activeSearchRequestId !== null) {
      return;
    }
    renderQuickOpen();
  } else {
    scheduleWorkspaceSearch();
  }
}

function scheduleWorkspaceSearch(): void {
  if (searchMode !== "workspace" || !searchDialog.open) {
    return;
  }
  clearSearchTimer();
  searchTimer = setTimeout(() => requestWorkspaceSearch(), 180);
}

function requestWorkspaceSearch(): void {
  clearSearchTimer();
  cancelActiveSearch();
  const query = searchQuery.value;
  if (query.length === 0) {
    clearSearchResults("Type to search the workspace.");
    return;
  }
  const requestId = nextSearchRequestId();
  activeSearchRequestId = requestId;
  clearSearchResults("Searching…");
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "workspace_search_requested",
    requestId,
    query,
    options: {
      caseSensitive: searchCase.getAttribute("aria-pressed") === "true",
      wholeWord: searchWord.getAttribute("aria-pressed") === "true",
      useRegex: searchRegex.getAttribute("aria-pressed") === "true",
      include: commaSeparatedGlobs(searchInclude.value),
      exclude: commaSeparatedGlobs(searchExclude.value),
      maxResults: 500,
      maxFileBytes: 2 * 1024 * 1024,
    },
  });
}

function cancelActiveSearch(): void {
  if (activeSearchRequestId === null) {
    return;
  }
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "search_cancelled",
    requestId: activeSearchRequestId,
  });
  activeSearchRequestId = null;
}

function nextSearchRequestId(): string {
  searchRequestCounter += 1;
  return `search-${searchRequestCounter}`;
}

function clearSearchTimer(): void {
  if (searchTimer !== null) {
    clearTimeout(searchTimer);
    searchTimer = null;
  }
}

function renderQuickOpen(): void {
  const paths = rankQuickOpen(quickOpenPaths, searchQuery.value, recentPaths);
  clearSearchResultNodes();
  for (const path of paths) {
    appendSearchResult(
      { path, line: 0, column: 0, length: 0 },
      path,
      null,
      0,
      0,
    );
  }
  const suffix = quickOpenTruncated ? " (first 2,000 indexed files)" : "";
  searchStatus.textContent = `${paths.length} matching files${suffix}`;
  selectSearchResult(0);
}

function renderWorkspaceSearch(
  matches: WorkspaceSearchMatch[],
  truncated: boolean,
  error: string | null,
): void {
  clearSearchResultNodes();
  if (error !== null) {
    searchStatus.textContent = error;
    return;
  }
  let previousPath: string | null = null;
  for (const found of matches) {
    const label = previousPath === found.path ? `Line ${found.line + 1}` : `${found.path}:${found.line + 1}`;
    appendSearchResult(
      found,
      label,
      found.preview,
      found.previewColumn,
      found.previewLength,
    );
    previousPath = found.path;
  }
  const suffix = truncated ? " — result limit reached" : "";
  searchStatus.textContent = `${matches.length} matches${suffix}`;
  selectSearchResult(0);
}

function clearSearchResults(status: string): void {
  clearSearchResultNodes();
  searchStatus.textContent = status;
}

function clearSearchResultNodes(): void {
  searchResults.replaceChildren();
  renderedSearchResults = [];
  selectedSearchResult = 0;
}

function appendSearchResult(
  selection: SearchSelection,
  label: string,
  preview: string | null,
  previewColumn: number,
  previewLength: number,
): void {
  const index = renderedSearchResults.length;
  renderedSearchResults.push(selection);
  const button = document.createElement("button");
  button.type = "button";
  button.className = "search-result";
  button.setAttribute("role", "option");
  button.dataset.index = String(index);
  const pathLabel = document.createElement("span");
  pathLabel.className = "search-result-path";
  pathLabel.textContent = label;
  button.append(pathLabel);
  if (preview !== null) {
    const previewLabel = document.createElement("span");
    previewLabel.className = "search-result-preview";
    const before = preview.slice(0, previewColumn);
    const match = preview.slice(previewColumn, previewColumn + previewLength);
    const after = preview.slice(previewColumn + previewLength);
    previewLabel.append(document.createTextNode(before));
    const mark = document.createElement("mark");
    mark.textContent = match;
    previewLabel.append(mark, document.createTextNode(after));
    button.append(previewLabel);
  }
  button.addEventListener("click", () => openSearchResult(index));
  button.addEventListener("mouseenter", () => selectSearchResult(index));
  searchResults.append(button);
}

function searchKeyDown(event: KeyboardEvent): void {
  if (event.key === "ArrowDown") {
    event.preventDefault();
    selectSearchResult(selectedSearchResult + 1);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    selectSearchResult(selectedSearchResult - 1);
  } else if (event.key === "Enter") {
    event.preventDefault();
    openSearchResult(selectedSearchResult);
  }
}

function selectSearchResult(index: number): void {
  if (renderedSearchResults.length === 0) {
    return;
  }
  const next = Math.max(0, Math.min(index, renderedSearchResults.length - 1));
  // Touch only the two affected rows: this runs per mouseenter and per arrow
  // key over a list of up to 500 results.
  const previous = searchResults.children[selectedSearchResult];
  if (previous instanceof HTMLElement) {
    previous.classList.remove("is-selected");
    previous.setAttribute("aria-selected", "false");
  }
  selectedSearchResult = next;
  const selected = searchResults.children[next];
  if (selected instanceof HTMLElement) {
    selected.classList.add("is-selected");
    selected.setAttribute("aria-selected", "true");
    selected.scrollIntoView({ block: "nearest" });
  }
}

function openSearchResult(index: number): void {
  const selection = renderedSearchResults[index];
  if (selection === undefined) {
    return;
  }
  const recentIndex = recentPaths.indexOf(selection.path);
  if (recentIndex >= 0) {
    recentPaths.splice(recentIndex, 1);
  }
  recentPaths.unshift(selection.path);
  recentPaths.splice(20);
  closeSearchDialog();
  postToHost({
    protocolVersion: PROTOCOL_VERSION,
    surfaceId,
    type: "search_result_open_requested",
    path: selection.path,
    line: selection.line,
    column: selection.column,
    length: selection.length,
  });
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
  syncDocument(document);
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
  syncDocument(document);
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

function defineEditorTheme(appearance: EditorAppearance): void {
  monaco.editor.defineTheme(EDITOR_THEME, {
    base: appearance.dark ? "vs-dark" : "vs",
    inherit: true,
    rules: [],
    colors: {
      "editor.background": appearance.background,
      "editor.foreground": appearance.foreground,
      "editorCursor.foreground": appearance.cursor,
      "editorLineNumber.foreground": colorWithAlpha(appearance.foreground, 0.38),
      "editorLineNumber.activeForeground": colorWithAlpha(appearance.foreground, 0.72),
      "editor.selectionBackground": appearance.selectionBackground,
      "editor.selectionForeground": appearance.selectionForeground,
      "editor.inactiveSelectionBackground": colorWithAlpha(appearance.selectionBackground, 0.45),
      "editorIndentGuide.background1": colorWithAlpha(appearance.foreground, 0.16),
      "editorIndentGuide.activeBackground1": colorWithAlpha(appearance.foreground, 0.38),
    },
  });
}

function applyAppearance(appearance: EditorAppearance): void {
  appliedAppearance = appearance;
  defineEditorTheme(appearance);
  monaco.editor.setTheme(EDITOR_THEME);
  const fontFamily = editorFontFamily(appearance.fontFamily);
  editorFontSize = appearance.fontSize;
  editor.updateOptions({ fontFamily, fontSize: editorFontSize });
  diffEditor?.updateOptions({ fontFamily, fontSize: editorFontSize });

  const root = document.documentElement;
  root.style.colorScheme = appearance.dark ? "dark" : "light";
  root.style.setProperty("--ink", appearance.background);
  root.style.setProperty("--text", appearance.foreground);
  root.style.setProperty("--accent", appearance.cursor);
  root.style.setProperty("--selection", appearance.selectionBackground);
}

function editorFontFamily(primary: string): string {
  return `${primary}, ${EDITOR_FONT_FALLBACKS}`;
}

function colorWithAlpha(color: string, alpha: number): string {
  const clamped = Math.round(Math.min(1, Math.max(0, alpha)) * 255);
  return `${color.slice(0, 7)}${clamped.toString(16).padStart(2, "0")}`;
}

function setEditorFontSize(fontSize: number): void {
  editorFontSize = fontSize;
  editor.updateOptions({ fontSize });
  diffEditor?.updateOptions({ fontSize });
}

function renderState(): void {
  const active = activeDocumentId === null ? undefined : documents.get(activeDocumentId);
  const showingDiff = active !== undefined && diffDocumentId === active.payload.id;
  emptyState.classList.toggle("is-hidden", active !== undefined);
  modeSwitch.hidden = active === undefined;
  modeEdit.setAttribute("aria-pressed", String(!showingDiff));
  modeDiff.setAttribute("aria-pressed", String(showingDiff));
  editorContainer.classList.toggle("is-visible", active !== undefined && !showingDiff);
  diffEditorContainer.classList.toggle("is-visible", showingDiff);
  if (showingDiff) {
    diffEditor?.layout();
  } else if (active !== undefined) {
    editor.layout();
  }
  if (active === undefined) {
    documentState.className = "document-state";
    documentState.hidden = true;
    conflictBanner.hidden = true;
    return;
  }

  const state = visibleDocumentState(
    active.diskStatus,
    active.payload.readOnly,
    active.payload.dirty,
  );
  if (active.saveError !== null) {
    documentState.textContent = active.saveError;
    documentState.className = "document-state is-conflict";
    documentState.hidden = false;
  } else {
    documentState.textContent = state.text;
    documentState.className = `document-state${state.kind === "normal" ? "" : ` is-${state.kind}`}`;
    documentState.hidden = state.hidden || active.payload.externalChange;
  }
  renderConflictBanner(active, showingDiff);
}

function renderConflictBanner(active: OpenDocument, showingDiff: boolean): void {
  const state = conflictUiState(active.diskStatus, active.payload.externalChange, showingDiff);
  conflictBanner.hidden = state.hidden;
  if (state.hidden) {
    return;
  }
  conflictMessage.textContent = state.message;
  conflictCompare.disabled = state.compareDisabled;
  conflictReload.disabled = state.reloadDisabled;
  conflictKeep.textContent = state.keepLabel;
  conflictCloseDiff.hidden = state.closeCompareHidden;
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

function requiredInput(id: string): HTMLInputElement {
  const element = requiredElement(id);
  if (!(element instanceof HTMLInputElement)) {
    throw new Error(`Editor element is not an input: ${id}`);
  }
  return element;
}
