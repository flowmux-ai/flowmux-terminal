// SPDX-License-Identifier: GPL-3.0-or-later

export type TabMove = "first" | "last" | "next" | "previous";

export function movedTabIndex(current: number, count: number, move: TabMove): number | null {
  if (count <= 0) {
    return null;
  }
  const safeCurrent = Math.min(Math.max(current, 0), count - 1);
  switch (move) {
    case "first":
      return 0;
    case "last":
      return count - 1;
    case "next":
      return (safeCurrent + 1) % count;
    case "previous":
      return (safeCurrent - 1 + count) % count;
  }
}

export function adjustedFontSize(current: number, delta: number): number {
  return Math.min(32, Math.max(10, current + delta));
}
