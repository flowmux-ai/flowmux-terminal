#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Pull cmux upstream into a sibling checkout and produce a triage report
# of what changed since the pinned commit. We do NOT vendor cmux source
# into this tree — the checkout lives in `.upstream-cmux/` (gitignored)
# and is only used to read documentation diffs.
#
# Usage:
#   scripts/sync-upstream.sh                    # diff vs. pinned
#   scripts/sync-upstream.sh --since v0.30.0    # diff since a tag/sha
#   scripts/sync-upstream.sh --bump             # update PINNED to current HEAD

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UP_DIR="${ROOT}/.upstream-cmux"
PIN_FILE="${UP_DIR}/PINNED"
INBOX_DIR="${ROOT}/docs/upstream-mapping/_inbox"
REMOTE_URL="https://github.com/manaflow-ai/cmux.git"

since=""
bump=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --since) since="$2"; shift 2 ;;
    --bump)  bump=1; shift ;;
    -h|--help)
      sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "${INBOX_DIR}"

if [[ ! -d "${UP_DIR}/.git" ]]; then
  echo ">> cloning upstream cmux into ${UP_DIR}"
  git clone --filter=blob:none "${REMOTE_URL}" "${UP_DIR}"
else
  echo ">> fetching upstream cmux"
  git -C "${UP_DIR}" fetch --tags --prune origin
fi

git -C "${UP_DIR}" checkout -q origin/HEAD

if [[ -z "${since}" ]]; then
  if [[ -f "${PIN_FILE}" ]]; then
    since="$(awk -F= '/^commit=/{print $2}' "${PIN_FILE}")"
  else
    echo "no PINNED file; pass --since <ref> to diff against a known point" >&2
    exit 1
  fi
fi

now_sha="$(git -C "${UP_DIR}" rev-parse HEAD)"
date_tag="$(date -u +%Y-%m-%d)"
report="${INBOX_DIR}/${date_tag}__${now_sha:0:12}.md"

{
  echo "# Upstream sync report — ${date_tag}"
  echo
  echo "- pinned (was): \`${since}\`"
  echo "- upstream HEAD (now): \`${now_sha}\`"
  echo
  echo "## CHANGELOG.md additions"
  echo
  git -C "${UP_DIR}" log --pretty=format:'- %h %s' "${since}..HEAD" -- CHANGELOG.md \
    || echo "_(no changelog activity)_"
  echo
  echo
  echo "## README.md changes"
  echo
  echo '```diff'
  git -C "${UP_DIR}" diff "${since}..HEAD" -- README.md | head -400 || true
  echo '```'
  echo
  echo "## New top-level entries (often new features)"
  echo
  git -C "${UP_DIR}" diff --name-status --diff-filter=A "${since}..HEAD" \
    | awk '$2 !~ /\// {print "- `" $2 "`"}' \
    || true
  echo
  echo "## Triage"
  echo
  echo "For each entry above, classify as:"
  echo
  echo "- \`port\` — implement in flowmux. Open/update \`docs/upstream-mapping/<feature>.md\`."
  echo "- \`skip\` — macOS-only or not applicable on Linux. Note the reason."
  echo "- \`defer\` — out of scope for the current milestone."
  echo "- \`n/a\`  — not a behavioral change (typo, internal refactor, build only)."
} > "${report}"

echo ">> report written: ${report}"

if (( bump )); then
  {
    echo "commit=${now_sha}"
    echo "date=${date_tag}"
  } > "${PIN_FILE}"
  echo ">> bumped PINNED to ${now_sha}"
fi
