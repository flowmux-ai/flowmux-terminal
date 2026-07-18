// SPDX-License-Identifier: GPL-3.0-or-later

import assert from "node:assert/strict";
import test from "node:test";
import {
  commaSeparatedGlobs,
  fuzzyMatches,
  rankQuickOpen,
} from "../.test-build/search_state.js";

test("fuzzy matching supports multilingual subsequences and terms", () => {
  assert.equal(fuzzyMatches("문🙂", "src/문서🙂.rs"), true);
  assert.equal(fuzzyMatches("sr 문", "src/문서.rs"), true);
  assert.equal(fuzzyMatches("문서 js", "src/문서.rs"), false);
});

test("quick open ranks recent and shorter paths first", () => {
  const paths = ["deep/folder/main.rs", "main.rs", "최근-main.rs", "other.txt"];
  assert.deepEqual(rankQuickOpen(paths, "main", ["최근-main.rs"]), [
    "최근-main.rs",
    "main.rs",
    "deep/folder/main.rs",
  ]);
});

test("comma separated globs are trimmed and bounded", () => {
  assert.deepEqual(commaSeparatedGlobs(" src/**, , **/*.rs "), ["src/**", "**/*.rs"]);
  assert.equal(
    commaSeparatedGlobs(Array.from({ length: 40 }, (_, index) => `p${index}`).join(",")).length,
    32,
  );
});
