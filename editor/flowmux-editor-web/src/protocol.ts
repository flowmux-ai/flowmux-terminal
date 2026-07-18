// SPDX-License-Identifier: GPL-3.0-or-later

export const PROTOCOL_VERSION = 1 as const;

export interface DocumentPayload {
  id: string;
  uri: string;
  relativePath: string;
  name: string;
  content: string;
  version: number;
  language?: string;
  encoding: "UTF-8" | "UTF-8 BOM";
  eol: "LF" | "CRLF";
  dirty: boolean;
  readOnly: boolean;
  externalChange: boolean;
  cursorLine: number;
  cursorColumn: number;
  scrollTop: number;
}

export type DocumentDiskStatus = "unchanged" | "modified" | "deleted";
export type RecoveryDiskState = "unchanged" | "changed" | "deleted";

export interface SearchOptions {
  caseSensitive: boolean;
  wholeWord: boolean;
  useRegex: boolean;
  include: string[];
  exclude: string[];
  maxResults: number;
  maxFileBytes: number;
}

export interface WorkspaceSearchMatch {
  path: string;
  line: number;
  column: number;
  length: number;
  preview: string;
  previewColumn: number;
  previewLength: number;
}

export interface WorkspaceSearchResult {
  matches: WorkspaceSearchMatch[];
  truncated: boolean;
  cancelled: boolean;
}

interface HostMessageBase {
  protocolVersion: typeof PROTOCOL_VERSION;
  surfaceId: string;
}

export type HostMessage =
  | (HostMessageBase & {
      type: "initialize_editor";
      workspaceName: string;
      documents: DocumentPayload[];
      activeDocumentId: string | null;
    })
  | (HostMessageBase & { type: "open_document"; document: DocumentPayload })
  | (HostMessageBase & { type: "replace_document"; document: DocumentPayload })
  | (HostMessageBase & { type: "close_document"; documentId: string; documentVersion: number })
  | (HostMessageBase & {
      type: "set_active_document";
      documentId: string;
      documentVersion: number;
    })
  | (HostMessageBase & {
      type: "save_completed";
      documentId: string;
      documentVersion: number;
      changeSequence: number;
    })
  | (HostMessageBase & {
      type: "save_failed";
      documentId: string;
      documentVersion: number;
      changeSequence: number;
      reason: string;
    })
  | (HostMessageBase & {
      type: "document_disk_status";
      documentId: string;
      documentVersion: number;
      status: DocumentDiskStatus;
    })
  | (HostMessageBase & {
      type: "recovery_available";
      documentId: string;
      documentVersion: number;
      diskState: RecoveryDiskState;
    })
  | (HostMessageBase & { type: "show_workspace_search" })
  | (HostMessageBase & {
      type: "quick_open_completed";
      requestId: string;
      paths: string[];
      truncated: boolean;
    })
  | (HostMessageBase & {
      type: "workspace_search_completed";
      requestId: string;
      result: WorkspaceSearchResult;
      error: string | null;
    })
  | (HostMessageBase & {
      type: "reveal_range";
      documentId: string;
      documentVersion: number;
      line: number;
      column: number;
      length: number;
    });

interface EditorMessageBase {
  protocolVersion: typeof PROTOCOL_VERSION;
  surfaceId: string;
}

export type EditorMessage =
  | (EditorMessageBase & { type: "editor_ready" })
  | (EditorMessageBase & {
      type: "active_document_changed";
      documentId: string;
      documentVersion: number;
    })
  | (EditorMessageBase & {
      type: "document_changed";
      documentId: string;
      documentVersion: number;
      changeSequence: number;
      content: string;
    })
  | (EditorMessageBase & {
      type: "save_requested";
      documentId: string;
      documentVersion: number;
      changeSequence: number;
      content: string;
    })
  | (EditorMessageBase & {
      type: "close_requested";
      documentId: string;
      documentVersion: number;
      dirty: boolean;
    })
  | (EditorMessageBase & {
      type: "discard_close_requested";
      documentId: string;
      documentVersion: number;
    })
  | (EditorMessageBase & {
      type: "recovery_decision";
      documentId: string;
      documentVersion: number;
      choice: "restore" | "discard";
    })
  | (EditorMessageBase & {
      type: "view_state_changed";
      documentId: string;
      documentVersion: number;
      cursorLine: number;
      cursorColumn: number;
      scrollTop: number;
    })
  | (EditorMessageBase & { type: "quick_open_requested"; requestId: string })
  | (EditorMessageBase & {
      type: "workspace_search_requested";
      requestId: string;
      query: string;
      options: SearchOptions;
    })
  | (EditorMessageBase & { type: "search_cancelled"; requestId: string })
  | (EditorMessageBase & {
      type: "search_result_open_requested";
      path: string;
      line: number;
      column: number;
      length: number;
    });

export interface DocumentEditAdvance {
  baseVersion: number;
  nextVersion: number;
  changeSequence: number;
}

export function advanceDocumentEdit(
  version: number,
  changeSequence: number,
): DocumentEditAdvance {
  return {
    baseVersion: version,
    nextVersion: version + 1,
    changeSequence: changeSequence + 1,
  };
}

export function isHostMessage(value: unknown): value is HostMessage {
  if (!isRecord(value)) {
    return false;
  }
  if (
    value.protocolVersion !== PROTOCOL_VERSION ||
    typeof value.surfaceId !== "string" ||
    typeof value.type !== "string"
  ) {
    return false;
  }

  switch (value.type) {
    case "initialize_editor":
      return (
        typeof value.workspaceName === "string" &&
        Array.isArray(value.documents) &&
        value.documents.every(isDocumentPayload) &&
        (value.activeDocumentId === null || typeof value.activeDocumentId === "string")
      );
    case "open_document":
    case "replace_document":
      return isDocumentPayload(value.document);
    case "close_document":
    case "set_active_document":
      return typeof value.documentId === "string" && isVersion(value.documentVersion);
    case "save_completed":
      return (
        typeof value.documentId === "string" &&
        isVersion(value.documentVersion) &&
        isVersion(value.changeSequence)
      );
    case "save_failed":
      return (
        typeof value.documentId === "string" &&
        isVersion(value.documentVersion) &&
        isVersion(value.changeSequence) &&
        typeof value.reason === "string"
      );
    case "document_disk_status":
      return (
        typeof value.documentId === "string" &&
        isVersion(value.documentVersion) &&
        (value.status === "unchanged" || value.status === "modified" || value.status === "deleted")
      );
    case "recovery_available":
      return (
        typeof value.documentId === "string" &&
        isVersion(value.documentVersion) &&
        (value.diskState === "unchanged" ||
          value.diskState === "changed" ||
          value.diskState === "deleted")
      );
    case "show_workspace_search":
      return true;
    case "quick_open_completed":
      return (
        typeof value.requestId === "string" &&
        Array.isArray(value.paths) &&
        value.paths.every((path) => typeof path === "string") &&
        typeof value.truncated === "boolean"
      );
    case "workspace_search_completed":
      return (
        typeof value.requestId === "string" &&
        isWorkspaceSearchResult(value.result) &&
        (value.error === null || typeof value.error === "string")
      );
    case "reveal_range":
      return (
        typeof value.documentId === "string" &&
        isVersion(value.documentVersion) &&
        isVersion(value.line) &&
        isVersion(value.column) &&
        isVersion(value.length)
      );
    default:
      return false;
  }
}

function isWorkspaceSearchResult(value: unknown): value is WorkspaceSearchResult {
  if (!isRecord(value)) {
    return false;
  }
  return (
    Array.isArray(value.matches) &&
    value.matches.every(isWorkspaceSearchMatch) &&
    typeof value.truncated === "boolean" &&
    typeof value.cancelled === "boolean"
  );
}

function isWorkspaceSearchMatch(value: unknown): value is WorkspaceSearchMatch {
  return (
    isRecord(value) &&
    typeof value.path === "string" &&
    isVersion(value.line) &&
    isVersion(value.column) &&
    isVersion(value.length) &&
    typeof value.preview === "string" &&
    isVersion(value.previewColumn) &&
    isVersion(value.previewLength)
  );
}

function isDocumentPayload(value: unknown): value is DocumentPayload {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value.id === "string" &&
    typeof value.uri === "string" &&
    typeof value.relativePath === "string" &&
    typeof value.name === "string" &&
    typeof value.content === "string" &&
    isVersion(value.version) &&
    (value.language === undefined || typeof value.language === "string") &&
    (value.encoding === "UTF-8" || value.encoding === "UTF-8 BOM") &&
    (value.eol === "LF" || value.eol === "CRLF") &&
    typeof value.dirty === "boolean" &&
    typeof value.readOnly === "boolean" &&
    typeof value.externalChange === "boolean" &&
    isVersion(value.cursorLine) &&
    isVersion(value.cursorColumn) &&
    typeof value.scrollTop === "number" &&
    Number.isFinite(value.scrollTop) &&
    value.scrollTop >= 0
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isVersion(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
