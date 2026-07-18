// SPDX-License-Identifier: GPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import {
  advanceDocumentEdit,
  isHostMessage,
  PROTOCOL_VERSION,
} from "../.test-build/protocol.js";

const koreanDocument = {
  id: "문서-1",
  uri: "file:///workspace/문서/인사말.rs",
  relativePath: "문서/인사말.rs",
  name: "인사말.rs",
  content: "fn main() { println!(\"안녕하세요 🌏\"); }\n",
  version: 0,
  language: "rust",
  encoding: "UTF-8",
  eol: "LF",
  dirty: false,
  readOnly: false,
  externalChange: false,
  cursorLine: 0,
  cursorColumn: 0,
  scrollTop: 0,
};

test("accepts a complete multilingual initialization message", () => {
  assert.equal(
    isHostMessage({
      protocolVersion: PROTOCOL_VERSION,
      surfaceId: "surface-1",
      type: "initialize_editor",
      workspaceName: "다국어 프로젝트",
      documents: [koreanDocument],
      activeDocumentId: koreanDocument.id,
    }),
    true,
  );
});

test("rejects unsupported versions, unknown types, and incomplete documents", () => {
  assert.equal(
    isHostMessage({
      protocolVersion: 2,
      surfaceId: "surface-1",
      type: "close_document",
      documentId: "document-1",
      documentVersion: 0,
    }),
    false,
  );
  assert.equal(
    isHostMessage({ protocolVersion: 1, surfaceId: "surface-1", type: "run_extension" }),
    false,
  );
  assert.equal(
    isHostMessage({
      protocolVersion: 1,
      surfaceId: "surface-1",
      type: "open_document",
      document: { ...koreanDocument, content: undefined },
    }),
    false,
  );
  assert.equal(
    isHostMessage({
      protocolVersion: 1,
      surfaceId: "surface-1",
      type: "open_document",
      document: { ...koreanDocument, scrollTop: Number.NaN },
    }),
    false,
  );
});

test("rejects negative and fractional document versions", () => {
  for (const documentVersion of [-1, 0.5, Number.NaN]) {
    assert.equal(
      isHostMessage({
        protocolVersion: 1,
        surfaceId: "surface-1",
        type: "set_active_document",
        documentId: "document-1",
        documentVersion,
      }),
      false,
    );
  }
});

test("accepts only known document disk states", () => {
  const message = {
    protocolVersion: 1,
    surfaceId: "surface-1",
    type: "document_disk_status",
    documentId: "document-1",
    documentVersion: 2,
  };
  assert.equal(isHostMessage({ ...message, status: "modified" }), true);
  assert.equal(isHostMessage({ ...message, status: "deleted" }), true);
  assert.equal(isHostMessage({ ...message, status: "unknown" }), false);
});

test("accepts only complete recovery proposals", () => {
  const message = {
    protocolVersion: 1,
    surfaceId: "surface-1",
    type: "recovery_available",
    documentId: "document-1",
    documentVersion: 2,
    diskState: "unchanged",
  };
  assert.equal(isHostMessage(message), true);
  assert.equal(isHostMessage({ ...message, diskState: "changed" }), true);
  assert.equal(isHostMessage({ ...message, diskState: "deleted" }), true);
  assert.equal(isHostMessage({ ...message, diskState: "unknown" }), false);
  assert.equal(isHostMessage({ ...message, documentVersion: -1 }), false);
});

test("advances the local version while sending the host's current base version", () => {
  assert.deepEqual(advanceDocumentEdit(7, 3), {
    baseVersion: 7,
    nextVersion: 8,
    changeSequence: 4,
  });
  assert.deepEqual(advanceDocumentEdit(8, 4), {
    baseVersion: 8,
    nextVersion: 9,
    changeSequence: 5,
  });
});
