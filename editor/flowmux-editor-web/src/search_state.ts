// SPDX-License-Identifier: GPL-3.0-or-later

export function fuzzyMatches(query: string, candidate: string): boolean {
  const normalizedCandidate = candidate.toLocaleLowerCase();
  return query
    .trim()
    .toLocaleLowerCase()
    .split(/\s+/u)
    .filter(Boolean)
    .every((token) => {
      let offset = 0;
      for (const character of token) {
        const found = normalizedCandidate.indexOf(character, offset);
        if (found < 0) {
          return false;
        }
        offset = found + character.length;
      }
      return true;
    });
}

export function rankQuickOpen(
  paths: readonly string[],
  query: string,
  recentPaths: readonly string[],
  limit = 200,
): string[] {
  const recentRank = new Map(recentPaths.map((path, index) => [path, index]));
  return paths
    .filter((path) => fuzzyMatches(query, path))
    .sort((left, right) => {
      const leftRecent = recentRank.get(left) ?? Number.MAX_SAFE_INTEGER;
      const rightRecent = recentRank.get(right) ?? Number.MAX_SAFE_INTEGER;
      return leftRecent - rightRecent || left.length - right.length || left.localeCompare(right);
    })
    .slice(0, limit);
}

export function commaSeparatedGlobs(value: string): string[] {
  return value
    .split(",")
    .map((entry) => entry.trim())
    .filter(Boolean)
    .slice(0, 32);
}
