#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
temporary_notice=$(mktemp "${TMPDIR:-/tmp}/flowmux-third-party.XXXXXX")
trap 'rm -f "$temporary_notice"' EXIT

cd "$repo_root"
cargo about generate about.hbs --workspace --locked --fail \
    --output-file "$temporary_notice"
sed -e 's/\r$//' -e 's/[[:blank:]]*$//' "$temporary_notice" | \
    awk '
        { lines[NR] = $0; if ($0 != "") last = NR }
        END { for (i = 1; i <= last; i++) print lines[i] }
    ' \
    > THIRD_PARTY_LICENSES.md

echo "==> refreshed THIRD_PARTY_LICENSES.md"
