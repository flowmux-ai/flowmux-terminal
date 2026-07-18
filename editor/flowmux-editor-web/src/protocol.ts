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
    default:
      return false;
  }
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
    typeof value.externalChange === "boolean"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isVersion(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
